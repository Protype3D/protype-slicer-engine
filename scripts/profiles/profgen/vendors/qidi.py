"""Модели QIDI и правила QIDI поверх профилей Orca (С1, С10, С13)."""
import copy
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from typing import Any

from ..errors import GeneratorError
from ..limits import ModelLimits, apply_limits, cap_value
from ..rules import RuleHit, RuleResult

VENDOR_ID = "qidi"
VENDOR_DIR = "Qidi"
VENDOR_NAME = "QIDI"

WRAP_OPEN = "{if current_extruder != next_extruder}"
WRAP_CLOSE = "{endif}"

# Подъём перед сменой Box в Orca v2.5.0 ничем не ограничен: в последних 3 мм высоты Klipper
# отвергнет ход за предел оси. QIDI Studio ограничивает его высотой печати; текст дословно из
# QIDITECH/QIDIStudio 21d47457, resources/profiles/X 4 Series/machine/Qidi X-Max 4 0.4 nozzle.json.
ORCA_LIFT = "G1 Z{max_layer_z + 3.0} F1200"
CLAMPED_LIFT = (
    "{if max_layer_z + 3 > max_print_height}\n"
    "G0 Z{max_print_height} F1200\n"
    "{else}\n"
    "G0 Z{max_layer_z + 3} F1200\n"
    "{endif}"
)

VERIFIED_MODEL_KEY = "qidi-xmax-4"
VERIFIED = frozenset({VERIFIED_MODEL_KEY})

EXHAUST_OFF_TYPES = frozenset({"ABS", "ASA"})
STAINLESS_NOZZLE = "stainless_steel"
HARDENED_NOZZLE = "hardened_steel"

RULE_TARGETS: dict[str, tuple[str, str]] = {
    "wrap_change_filament": ("Machine", "change_filament_gcode"),
    "clamp_toolchange_lift": ("Machine", "change_filament_gcode"),
    "no_chamber_heater": ("Machine", "support_chamber_temp_control"),
    "nozzle_type_bimetal": ("Extruder", "nozzle_type"),
    "chamber_clamp": ("Filament", "chamber_temperature"),
    "exhaust_off_abs_asa": ("Filament", "during_print_exhaust_fan_speed"),
}


@dataclass(frozen=True)
class VendorModel:
    key: str
    orca_name: str
    display_name: str
    has_box: bool


MODELS: tuple[VendorModel, ...] = (
    VendorModel("qidi-xmax-4", "Qidi X-Max 4", "QIDI X-Max 4", True),
    VendorModel("qidi-xplus-4", "Qidi X-Plus 4", "QIDI X-Plus 4", True),
    VendorModel("qidi-q2", "Qidi Q2", "QIDI Q2", True),
    VendorModel("qidi-q2c", "Qidi Q2C", "QIDI Q2C", True),
    VendorModel("qidi-q1pro", "Qidi Q1 Pro", "QIDI Q1 Pro", False),
    VendorModel("qidi-xmax-3", "Qidi X-Max 3", "QIDI X-Max 3", False),
    VendorModel("qidi-xplus-3", "Qidi X-Plus 3", "QIDI X-Plus 3", False),
    VendorModel("qidi-xsmart-3", "Qidi X-Smart 3", "QIDI X-Smart 3", False),
)


def wrap_change_filament(gcode: str) -> tuple[str, bool]:
    """Оборачивает шаблон смены в проверку смены экструдера; уже обёрнутый не трогает."""
    first_line = next((line.strip() for line in gcode.splitlines() if line.strip()), "")
    if first_line == WRAP_OPEN:
        return gcode, False
    return f"{WRAP_OPEN}\n{gcode.rstrip(chr(10))}\n{WRAP_CLOSE}\n", True


def clamp_toolchange_lift(gcode: str) -> tuple[str, bool]:
    """Неограниченный подъём Orca перед сменой -> подъём QIDI Studio не выше max_print_height."""
    lines = gcode.split("\n")
    clamped = [CLAMPED_LIFT if line.strip() == ORCA_LIFT else line for line in lines]
    return "\n".join(clamped), clamped != lines


def _preset_name(values: Mapping[str, Any]) -> str:
    return str(values.get("name", ""))


def _replace_nozzle(value: Any) -> Any:
    """Нержавеющее сопло -> закалённое, форма (список или строка) сохраняется."""
    if isinstance(value, list):
        return [HARDENED_NOZZLE if item == STAINLESS_NOZZLE else item for item in value]
    return HARDENED_NOZZLE if value == STAINLESS_NOZZLE else value


def machine_rules(
    model: VendorModel, machine: Mapping[str, Any], limits: ModelLimits
) -> RuleResult:
    """Правила машины: лимиты, обёртка смены Box, камера, тип сопла."""
    name = _preset_name(machine)
    hits: list[RuleHit] = []

    values, report = apply_limits(machine, limits)
    if values != dict(machine):
        hits.append(RuleHit("limits", name))

    if model.has_box:
        template = values.get("change_filament_gcode")
        if not isinstance(template, str) or not template.strip():
            raise GeneratorError(f"{name}: Box-модель без change_filament_gcode")
        values["change_filament_gcode"], wrapped = wrap_change_filament(template)
        if wrapped:
            hits.append(RuleHit("wrap_change_filament", name))
        values["change_filament_gcode"], lifted = clamp_toolchange_lift(values["change_filament_gcode"])
        if lifted:
            hits.append(RuleHit("clamp_toolchange_lift", name))

    if limits.chamber_max == 0 and values.get("support_chamber_temp_control") != "0":
        values["support_chamber_temp_control"] = "0"
        hits.append(RuleHit("no_chamber_heater", name))

    if "nozzle_type" in values:
        nozzle = _replace_nozzle(values["nozzle_type"])
        if nozzle != values["nozzle_type"]:
            values["nozzle_type"] = nozzle
            hits.append(RuleHit("nozzle_type_bimetal", name))

    return RuleResult(values, tuple(hits), report)


def _filament_base_type(filament: Mapping[str, Any]) -> str:
    raw = filament.get("filament_type")
    first = raw[0] if isinstance(raw, list) and raw else raw
    return first.split("-")[0] if isinstance(first, str) else ""


def filament_rules(
    filament: Mapping[str, Any],
    models: Sequence[VendorModel],
    limits: Mapping[str, ModelLimits],
) -> RuleResult:
    """Правила пластика: предел камеры по строжайшей модели, вытяжка X-Max 4 для ABS/ASA."""
    name = _preset_name(filament)
    values = copy.deepcopy(dict(filament))
    hits: list[RuleHit] = []

    if models and "chamber_temperature" in values:
        cap = min(limits[model.key].chamber_max for model in models)
        clamped = cap_value(values["chamber_temperature"], cap)
        if clamped != values["chamber_temperature"]:
            values["chamber_temperature"] = clamped
            hits.append(RuleHit("chamber_clamp", name))

    has_verified = any(model.key == VERIFIED_MODEL_KEY for model in models)
    if has_verified and _filament_base_type(values) in EXHAUST_OFF_TYPES:
        if values.get("during_print_exhaust_fan_speed") != ["0"]:
            values["during_print_exhaust_fan_speed"] = ["0"]
            hits.append(RuleHit("exhaust_off_abs_asa", name))

    return RuleResult(values, tuple(hits), {})


def machine_extras(model: VendorModel, limits: ModelLimits, extruder_id: str) -> dict[str, Any]:
    """Ключи схемы, которых нет в Orca."""
    return {
        "kinematics": "corexy",
        "extruder_count": 1,
        "extruder_slots": [extruder_id],
        "has_chamber_heater": limits.chamber_max > 0,
        "chamber_max_temperature": limits.chamber_max,
    }
