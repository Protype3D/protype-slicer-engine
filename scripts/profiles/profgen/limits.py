"""Таблица лимитов printer.cfg и камеры (С13) и поэлементный минимум с профилем Orca."""
import copy
import json
from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from .errors import GeneratorError

CFG_TO_ORCA: dict[str, tuple[str, ...]] = {
    "max_velocity": ("machine_max_speed_x", "machine_max_speed_y"),
    "max_accel": (
        "machine_max_acceleration_x",
        "machine_max_acceleration_y",
        "machine_max_acceleration_extruding",
        "machine_max_acceleration_travel",
    ),
    "max_z_velocity": ("machine_max_speed_z",),
    "max_z_accel": ("machine_max_acceleration_z",),
    "square_corner_velocity": ("machine_max_jerk_x", "machine_max_jerk_y"),
}


@dataclass(frozen=True)
class ModelLimits:
    printer_cfg: Mapping[str, float] | None
    cfg_url: str | None
    chamber_max: int
    chamber_source: str


def format_number(value: float) -> str:
    """Число строкой без хвоста `.0`."""
    return str(int(value)) if float(value) == int(value) else str(float(value))


def _cap_element(element: Any, cap: float) -> Any:
    """Число-строка выше предела заменяется пределом; остальное остаётся как есть."""
    if not isinstance(element, str):
        return element
    try:
        number = float(element)
    except ValueError:
        return element
    return format_number(cap) if number > cap else element


def cap_value(value: Any, cap: float) -> Any:
    """Поэлементный минимум значения Orca (список строк или строка) и числа; вход не меняется."""
    if isinstance(value, list):
        return [_cap_element(element, cap) for element in value]
    return _cap_element(value, cap)


def _parse_model(key: str, body: Any) -> ModelLimits:
    if not isinstance(body, dict) or not isinstance(body.get("chamber"), dict):
        raise GeneratorError(f"limits: у модели {key} нет раздела chamber")
    chamber = body["chamber"]
    if not isinstance(chamber.get("max"), int) or not isinstance(chamber.get("source"), str):
        raise GeneratorError(f"limits: у модели {key} chamber без max/source")
    raw_cfg = body.get("printer_cfg")
    if raw_cfg is None:
        return ModelLimits(None, None, chamber["max"], chamber["source"])
    if not isinstance(raw_cfg, dict):
        raise GeneratorError(f"limits: printer_cfg модели {key} должен быть объектом или null")
    numbers: dict[str, float] = {}
    for cfg_key in CFG_TO_ORCA:
        number = raw_cfg.get(cfg_key)
        if isinstance(number, bool) or not isinstance(number, (int, float)):
            raise GeneratorError(f"limits: у модели {key} в printer_cfg нет числа {cfg_key}")
        numbers[cfg_key] = number
    return ModelLimits(numbers, raw_cfg.get("url"), chamber["max"], chamber["source"])


def load_limits(path: Path) -> dict[str, ModelLimits]:
    """Читает таблицу лимитов моделей."""
    try:
        document = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        raise GeneratorError(f"limits: не прочитать {path}: {error}") from error
    models = document.get("models") if isinstance(document, dict) else None
    if not isinstance(models, dict):
        raise GeneratorError(f"limits: в {path} нет раздела models")
    return {key: _parse_model(key, body) for key, body in models.items()}


def apply_limits(
    machine: Mapping[str, Any], limits: ModelLimits
) -> tuple[dict[str, Any], dict[str, dict]]:
    """Новый словарь с минимумом профиля и printer.cfg и запись отчёта по каждому ключу."""
    out = copy.deepcopy(dict(machine))
    report: dict[str, dict] = {}
    if limits.printer_cfg is None:
        return out, report
    for cfg_key, orca_keys in CFG_TO_ORCA.items():
        cfg_value = limits.printer_cfg.get(cfg_key)
        if cfg_value is None:
            continue
        for orca_key in orca_keys:
            before = machine.get(orca_key)
            result = [format_number(cfg_value)] if before is None else cap_value(before, cfg_value)
            out[orca_key] = result
            report[orca_key] = {
                "profile": copy.deepcopy(before),
                "cfg": cfg_value,
                "result": copy.deepcopy(result),
            }
    return out, report
