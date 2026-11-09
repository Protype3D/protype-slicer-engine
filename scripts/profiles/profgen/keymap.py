"""Ключи Orca → ключи схемы Slicer и приведение значений к типам схемы."""
import json
import math
import re
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from hashlib import sha256
from pathlib import Path
from typing import Any

from .errors import GeneratorError
from .orca_tree import _flatten

# Пять псевдонимов, которых словари не дают: (область, ключ Orca) -> ключ схемы.
ORCA_ALIASES: dict[tuple[str, str], str] = {
    ("Filament", "hot_plate_temp"): "bed_temperature_normal",
    ("Filament", "hot_plate_temp_initial_layer"): "bed_temperature_initial_layer",
    ("Machine", "machine_max_jerk_x"): "machine_max_jerk_xy",
    ("Machine", "printer_model"): "machine_model",
    ("Machine", "printer_variant"): "machine_variant",
}

# Старые слова значений, которые libslic3r сам переводит при чтении (PrintConfigDef::handle_legacy,
# одинаково в пине 39a9077b и в Orca 2.5.0): "zig-zag" BambuStudio — это rectilinear, не новый
# zigzag Orca. Агент Slicer переводит так же (apps/agent/src/profile/legacy_values.rs).
# Ключ Orca -> {слово Orca -> слово движка}.
_ZIG_ZAG = {"zig-zag": "rectilinear"}
LEGACY_VALUES: dict[str, dict[str, str]] = {
    key: _ZIG_ZAG
    for key in ("sparse_infill_pattern", "top_surface_pattern", "bottom_surface_pattern",
                "internal_solid_infill_pattern", "ironing_pattern", "support_ironing_pattern")
}

# Списки, которые libslic3r читает одной строкой через запятую, поэтому элемент Orca может нести
# несколько записей ("0x0, 16x0, 16x13"). Агент Slicer держит по одной записи на элемент и
# вырезает остальное при установке пакета (apps/agent/src/profile/joined_entries.rs); здесь
# элементы режутся по запятым, а запись не того вида отвергается. Ключ -> вид одной записи.
JOINED_ENTRIES: dict[str, re.Pattern[str]] = {
    "thumbnails": re.compile(r"^\d{1,4}x\d{1,4}/(PNG|JPG|QOI|BTT_TFT|COLPIC)$"),
    "bed_exclude_area": re.compile(r"^-?\d+(\.\d+)?x-?\d+(\.\d+)?$"),
}

# Ключи, которых ещё нет в схеме, но которые переносятся наперёд: ключ -> (область, тип).
CARRY_AHEAD: dict[str, tuple[str, str]] = {"thumbnails": ("Machine", "vec_string")}

NOT_SETTINGS: frozenset[str] = frozenset(_flatten.META_KEYS) | {
    "compatible_printers",
    "compatible_printers_condition",
    "compatible_prints",
    "compatible_prints_condition",
    "print_settings_id",
    "filament_settings_id",
    "printer_settings_id",
    "default_print_profile",
    "default_filament_profile",
    "thumbnails_format",
}

AGENT_TRANSLATION_SOURCE = (
    "Slicer apps/agent/src/profile/dto.rs: apply_libslic3r_aliases + apply_libslic3r_enum_values")
DEFAULT_THUMBNAIL_FORMAT = "PNG"
_TRUE = frozenset({"1", "true"})
_FALSE = frozenset({"0", "false"})


@dataclass(frozen=True)
class SettingDef:
    key: str
    scope: str
    type: str
    units: str | None
    range: tuple[float, float] | None
    enum_values: tuple[str, ...]
    enum_map: Mapping[str, str]
    libslic3r_key: str | None = None


@dataclass(frozen=True)
class Converted:
    values: dict[str, dict[str, Any]]
    dropped: tuple[str, ...]
    rejected: tuple[tuple[str, str], ...]
    carried: tuple[str, ...]


def _read_json(path: Path) -> tuple[Any, str]:
    try:
        raw = path.read_bytes()
        return json.loads(raw), sha256(raw).hexdigest()
    except (OSError, ValueError) as exc:
        raise GeneratorError(f"не читается {path}: {exc}") from exc


def _definition(raw: Mapping[str, Any]) -> SettingDef:
    bounds = raw.get("range")
    return SettingDef(
        key=raw["key"],
        scope=raw["scope"],
        type=raw["type"],
        units=raw.get("units"),
        range=(float(bounds["min"]), float(bounds["max"])) if bounds else None,
        enum_values=tuple(raw.get("enum_values") or ()),
        enum_map=dict(raw.get("libslic3r_enum_map") or {}),
        libslic3r_key=raw.get("libslic3r_key") if isinstance(raw.get("libslic3r_key"), str) else None,
    )


class KeyMap:
    def __init__(self, schema_version: int, schema_sha256: str, keys_sha256: str, libslic3r_pin: str,
                 defs: dict[str, dict[str, SettingDef]], names: dict[str, dict[str, SettingDef]]):
        self.schema_version = schema_version
        self.schema_sha256 = schema_sha256
        self.keys_sha256 = keys_sha256
        self.libslic3r_pin = libslic3r_pin
        self._defs = defs
        self._names = names

    @classmethod
    def load(cls, schema_path: Path, keys_path: Path) -> "KeyMap":
        schema, schema_hash = _read_json(Path(schema_path))
        keys, keys_hash = _read_json(Path(keys_path))
        daemon = keys.get("daemon_renames", {})
        legacy = keys.get("legacy_renames", {})

        def final(name: str) -> str:
            step = daemon.get(name, name)
            return legacy.get(step, step)

        defs: dict[str, dict[str, SettingDef]] = {}
        claims: dict[str, dict[str, dict[str, SettingDef]]] = {}
        try:
            for raw in schema["defs"]:
                sdef = _definition(raw)
                defs.setdefault(sdef.scope, {})[sdef.key] = sdef
                own = {sdef.key}
                if isinstance(raw.get("libslic3r_key"), str):
                    own.add(raw["libslic3r_key"])
                for name in own | {final(n) for n in own}:
                    claims.setdefault(sdef.scope, {}).setdefault(name, {})[sdef.key] = sdef
            version = int(schema["schema_version"])
            pin = str(keys["libslic3r_pin"])
        except (KeyError, TypeError, ValueError) as exc:
            raise GeneratorError(f"схема или снапшот ключей неполны: {exc!r}") from exc
        names = {scope: cls._resolve(scope, by_name) for scope, by_name in claims.items()}
        for (scope, orca_key), schema_key in ORCA_ALIASES.items():
            if schema_key not in defs.get(scope, {}):
                raise GeneratorError(f"псевдоним {orca_key}: в схеме нет {scope}.{schema_key}")
            names[scope][orca_key] = defs[scope][schema_key]
        return cls(version, schema_hash, keys_hash, pin, defs, names)

    @staticmethod
    def _resolve(scope: str, by_name: Mapping[str, Mapping[str, SettingDef]]) -> dict[str, SettingDef]:
        resolved: dict[str, SettingDef] = {}
        for name, claimants in by_name.items():
            if len(claimants) == 1:
                resolved[name] = next(iter(claimants.values()))
            elif name in claimants:
                resolved[name] = claimants[name]
            else:
                raise GeneratorError(f"неоднозначный ключ {name!r} в области {scope}: {sorted(claimants)}")
        return resolved

    def target(self, scope: str, orca_key: str) -> SettingDef | None:
        return self._names.get(scope, {}).get(orca_key)

    def has(self, scope: str, key: str) -> bool:
        return key in self._defs.get(scope, {})

    def agent_to_engine(self) -> dict[str, Any]:
        """Перевод агент -> движок: ключи с псевдонимом и/или картой значений, по `(область, ключ)`."""
        entries = []
        for scope in sorted(self._defs):
            for key in sorted(self._defs[scope]):
                sdef = self._defs[scope][key]
                entry: dict[str, Any] = {"scope": scope, "key": key}
                if sdef.libslic3r_key and sdef.libslic3r_key != key:
                    entry["libslic3r_key"] = sdef.libslic3r_key
                if sdef.enum_map:
                    entry["libslic3r_enum_map"] = dict(sdef.enum_map)
                if len(entry) > 2:
                    entries.append(entry)
        return {"schema_sha256": self.schema_sha256, "source": AGENT_TRANSLATION_SOURCE, "defs": entries}


def _as_scalar(value: Any) -> Any:
    if isinstance(value, (list, tuple)):
        if not value:
            raise ValueError("empty list")
        return value[0]
    return value


def _as_items(value: Any) -> list[Any]:
    if isinstance(value, (list, tuple)):
        return list(value)
    return [part.strip() for part in str(value).split(",") if part.strip()]


def _number(value: Any, sdef: SettingDef) -> float:
    if isinstance(value, bool):
        raise ValueError(f"number {value!r}")
    text = str(value).strip()
    if text.endswith("%"):
        if sdef.units != "%":
            raise ValueError("percent for absolute value")
        text = text[:-1].strip()
    try:
        number = float(text)
    except ValueError:
        raise ValueError(f"number {text!r}") from None
    if not math.isfinite(number):
        raise ValueError("not a finite number")
    if sdef.range and not sdef.range[0] <= number <= sdef.range[1]:
        raise ValueError(f"outside [{sdef.range[0]:g}, {sdef.range[1]:g}]")
    return number


def _boolean(value: Any) -> bool:
    if isinstance(value, bool):
        return value
    text = str(value).strip().lower()
    if text in _TRUE:
        return True
    if text in _FALSE:
        return False
    raise ValueError(f"bool {value!r}")


def _enum(value: Any, sdef: SettingDef) -> str:
    text = str(value)
    if text in sdef.enum_values:
        return text
    mapped = [v for v in sdef.enum_values if sdef.enum_map.get(v) == text]
    if len(mapped) > 1:
        mapped = [v for v in mapped if text.lower().startswith(v.lower())]
    if len(mapped) == 1:
        return mapped[0]
    raise ValueError(f"enum '{text}'")


def _point(item: Any) -> list[float]:
    if isinstance(item, (list, tuple)):
        parts = [str(p) for p in item]
    else:
        parts = str(item).strip().lower().split("x")
    try:
        if len(parts) != 2:
            raise ValueError
        return [float(parts[0]), float(parts[1])]
    except ValueError:
        raise ValueError(f"vec2 {item!r}") from None


def _split_joined(items: Sequence[Any]) -> list[str]:
    return [part.strip() for item in items for part in str(item).split(",") if part.strip()]


def _joined_entries(items: Sequence[Any], pattern: re.Pattern[str]) -> list[str]:
    entries = _split_joined(items)
    wrong = next((entry for entry in entries if not pattern.match(entry)), None)
    if wrong is not None:
        raise ValueError(f"entry {wrong!r}")
    return entries


def _real(item: Any) -> float:
    try:
        return float(str(item).strip())
    except ValueError:
        raise ValueError(f"number {item!r}") from None


def coerce(value: Any, sdef: SettingDef) -> Any:
    """Приводит значение Orca к типу схемы; при отказе ValueError с причиной."""
    kind = sdef.type
    if kind.startswith("vec_"):
        items = _as_items(value)
        if any(item == "nil" for item in items):
            raise ValueError("nil")
        if kind == "vec_vec2":
            return [_point(item) for item in items]
        if kind == "vec_float":
            return [_real(item) for item in items]
        if kind == "vec_string":
            pattern = JOINED_ENTRIES.get(sdef.key)
            return _joined_entries(items, pattern) if pattern else [str(item) for item in items]
        raise ValueError(f"unsupported type '{kind}'")
    scalar = _as_scalar(value)
    if scalar == "nil":
        raise ValueError("nil")
    if kind == "bool":
        return _boolean(scalar)
    if kind == "int":
        return round(_number(scalar, sdef))
    if kind == "float":
        return _number(scalar, sdef)
    if kind == "enum":
        return _enum(scalar, sdef)
    if kind == "string":
        return str(scalar)
    raise ValueError(f"unsupported type '{kind}'")


def normalize_thumbnails(machine: Mapping[str, Any]) -> list[str] | None:
    """`thumbnails` в виде `WxH/FMT`; `thumbnail_size` QIDI Studio игнорируется."""
    if "thumbnails" not in machine:
        return None
    fmt = str(_as_scalar(machine.get("thumbnails_format") or DEFAULT_THUMBNAIL_FORMAT)).strip()
    return [text if "/" in text else f"{text}/{fmt}" for text in _split_joined(_as_items(machine["thumbnails"]))]


def _legacy(orca_key: str, raw: Any) -> Any:
    """Значение со старым словом, переведённым так, как его прочтёт libslic3r."""
    table = LEGACY_VALUES.get(orca_key)
    if not table:
        return raw
    if isinstance(raw, list):
        return [table.get(item, item) if isinstance(item, str) else item for item in raw]
    return table.get(raw, raw) if isinstance(raw, str) else raw


def convert(orca_values: Mapping[str, Any], scopes: Sequence[str], keymap: KeyMap) -> Converted:
    values: dict[str, dict[str, Any]] = {scope: {} for scope in scopes}
    owner: dict[tuple[str, str], str] = {}
    dropped: list[str] = []
    rejected: list[tuple[str, str]] = []
    carried: list[str] = []
    for orca_key in sorted(orca_values):
        if orca_key in NOT_SETTINGS:
            continue
        raw = orca_values[orca_key]
        hit = next((sd for sd in (keymap.target(s, orca_key) for s in scopes) if sd), None)
        if hit is None:
            scope, kind = CARRY_AHEAD.get(orca_key, (None, None))
            if scope in values:
                carry = SettingDef(orca_key, scope, kind, None, None, (), {})
                try:
                    values[scope][orca_key] = coerce(raw, carry)
                    carried.append(orca_key)
                except ValueError as exc:
                    rejected.append((orca_key, str(exc)))
            else:
                dropped.append(orca_key)
            continue
        try:
            result = coerce(_legacy(orca_key, raw), hit)
        except ValueError as exc:
            rejected.append((hit.key, str(exc)))
            continue
        slot = (hit.scope, hit.key)
        if slot in owner:
            rejected.append((hit.key, f"дубль {owner[slot]}"))
            continue
        owner[slot] = orca_key
        values[hit.scope][hit.key] = result
    return Converted(values, tuple(sorted(dropped)), tuple(sorted(rejected)), tuple(carried))
