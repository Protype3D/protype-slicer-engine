"""Проверка пакетов производителя (С3, С4, форма `model`); ею же проверяется закоммиченный каталог."""
import base64
import binascii
import re
from collections.abc import Mapping
from typing import Any

from .assets import MAX_ASSET_BYTES
from .errors import GeneratorError
from .ids import ID_PATTERN
from .output import dump_json

MAX_PRESET_JSON_BYTES = 256 * 1024
MAX_VENDOR_BYTES = 64 * 1024 * 1024
KIND_ORDER = ("machine", "extruder", "process", "filament")
CONTENT_KIND = {"machine": "Machine", "extruder": "Extruder", "process": "Process", "filament": "Filament"}
ORIGIN_KEYS = ("repo", "tag", "commit", "path", "license", "generator")
ASSET_MIME = {"bed_model": "model/stl", "bed_overlay": "image/svg+xml"}
NOZZLE_PATTERN = re.compile(r"^\d+\.\d+$")
ASSET_REF_PATTERN = re.compile(r"^[0-9a-z]{1,64}$")


def validate_vendor(packages: Mapping[str, dict]) -> None:
    """GeneratorError со списком всех нарушений; ключ словаря — ключ модели пакета."""
    problems = [p for key in sorted(packages) for p in _package_problems(key, packages[key])]
    problems += _shared_problems(packages) + _vendor_size_problems(packages)
    if problems:
        raise GeneratorError(
            f"пакеты не прошли проверку, нарушений {len(problems)}:\n" + "\n".join(f"- {p}" for p in problems))


def _dicts(value: Any) -> list[dict]:
    return [item for item in value if isinstance(item, dict)] if isinstance(value, list) else []


def _entries(pkg: Any) -> list[dict]:
    return _dicts(pkg.get("presets")) if isinstance(pkg, dict) else []


def _ref(value: Any) -> str | None:
    """Ссылка-строка или None: нестроковые id уже отмечены как нарушение и не ищутся."""
    return value if isinstance(value, str) else None


def _package_problems(key: str, pkg: Any) -> list[str]:
    if not isinstance(pkg, dict):
        return [f"{key}: пакет не объект"]
    problems = [f"{key}: поле {field} = {pkg.get(field)!r}, ждали {key!r}"
                for field in ("key", "printer_model_key") if pkg.get(field) != key]
    presets = pkg.get("presets")
    if not isinstance(presets, list) or len(_dicts(presets)) != len(presets):
        problems.append(f"{key}: presets не список объектов")
    entries = _entries(pkg)
    by_id: dict[str, dict] = {}
    for entry in entries:
        problems += _entry_problems(key, entry)
        pid = _ref(entry.get("preset_uuid"))
        if pid is not None and pid in by_id:
            problems.append(f"{key}: id {pid} повторяется в пакете")
        if pid is not None:
            by_id.setdefault(pid, entry)
    return problems + _kit_problems(key, pkg.get("model"), by_id) + _slot_problems(key, entries, by_id) \
        + _asset_problems(key, pkg.get("assets"), by_id)


def _entry_problems(key: str, entry: dict) -> list[str]:
    pid, kind, content = entry.get("preset_uuid"), entry.get("kind"), entry.get("content")
    where = f"{key}: {kind} {pid}"
    problems = []
    if not isinstance(pid, str) or not ID_PATTERN.fullmatch(pid):
        problems.append(f"{key}: id {pid!r} не по шаблону С3")
    if kind not in CONTENT_KIND:
        return problems + [f"{where}: вид не из {list(KIND_ORDER)}"]
    if not isinstance(content, dict):
        return problems + [f"{where}: content не объект"]
    if content.get("id") != pid:
        problems.append(f"{where}: content.id {content.get('id')!r} не равен preset_uuid")
    if content.get("kind") != CONTENT_KIND[kind]:
        problems.append(f"{where}: content.kind {content.get('kind')!r} не соответствует виду {kind}")
    problems += _placement_problems(key, where, kind, content) + _origin_problems(where, content.get("origin"))
    size = len(dump_json(content))
    if size > MAX_PRESET_JSON_BYTES:
        problems.append(f"{where}: content весит {size} байт, предел {MAX_PRESET_JSON_BYTES}")
    return problems


def _placement_problems(key: str, where: str, kind: str, content: dict) -> list[str]:
    if kind == "machine":
        problems = [] if content.get("printer_model") == key else [
            f"{where}: printer_model {content.get('printer_model')!r}, ждали {key!r}"]
        if "compatible_printer_models" in content:
            problems.append(f"{where}: машина несёт compatible_printer_models")
        return problems
    models = content.get("compatible_printer_models")
    problems = []
    if not isinstance(models, list) or not all(isinstance(m, str) for m in models) \
            or key not in models or models != sorted(set(models)):
        problems.append(f"{where}: compatible_printer_models {models!r} — не отсортированный список с {key!r}")
    if "printer_model" in content:
        problems.append(f"{where}: {kind} несёт printer_model")
    return problems


def _origin_problems(where: str, origin: Any) -> list[str]:
    if not isinstance(origin, dict):
        return [f"{where}: origin не объект"]
    empty = [k for k in ORIGIN_KEYS if not isinstance(origin.get(k), str) or not origin.get(k)]
    extra = sorted(set(origin) - set(ORIGIN_KEYS))
    if empty or extra:
        return [f"{where}: origin должен держать шесть непустых строк, пусто {empty}, лишнее {extra}"]
    return []


def _kit_problems(key: str, model: Any, by_id: Mapping[str, dict]) -> list[str]:
    nozzles = model.get("nozzles") if isinstance(model, dict) else None
    kit = model.get("default_kit") if isinstance(model, dict) else None
    if not isinstance(nozzles, list) or not isinstance(kit, dict):
        return [f"{key}: model без списка nozzles или объекта default_kit"]
    if not all(isinstance(n, str) and NOZZLE_PATTERN.fullmatch(n) for n in nozzles):
        return [f"{key}: nozzles {nozzles} не вида 0.4"]
    problems = []
    if nozzles != sorted(set(nozzles), key=float):
        problems.append(f"{key}: nozzles {nozzles} не по возрастанию")
    if set(nozzles) != set(kit):
        problems.append(f"{key}: nozzles {nozzles} не совпадают с ключами default_kit {sorted(kit)}")
    for nozzle in sorted(kit):
        slots = kit[nozzle]
        if not isinstance(slots, dict) or set(slots) != set(KIND_ORDER):
            problems.append(f"{key}: default_kit {nozzle}: ждали ключи {list(KIND_ORDER)}")
            continue
        for kind in KIND_ORDER:
            found = by_id.get(_ref(slots[kind]))
            if found is None:
                problems.append(f"{key}: default_kit {nozzle} {kind} {slots[kind]} нет в пакете")
            elif found.get("kind") != kind:
                problems.append(f"{key}: default_kit {nozzle} {kind} {slots[kind]} — пресет вида {found.get('kind')}")
    return problems


def _slot_problems(key: str, entries: list[dict], by_id: Mapping[str, dict]) -> list[str]:
    problems = []
    for entry in entries:
        content = entry.get("content")
        values = content.get("values") if isinstance(content, dict) else None
        if entry.get("kind") != "machine" or not isinstance(values, dict):
            continue
        slots = values.get("extruder_slots", [])
        for slot in slots if isinstance(slots, list) else [slots]:
            if by_id.get(_ref(slot), {}).get("kind") != "extruder":
                problems.append(f"{key}: машина {entry.get('preset_uuid')}: extruder_slots → {slot!r}, "
                                "такого экструдера в пакете нет")
    return problems


def _asset_size(asset: dict) -> int | None:
    try:
        return len(base64.b64decode(asset.get("data_base64"), validate=True))
    except (binascii.Error, TypeError, ValueError):
        return None


def _asset_problems(key: str, assets: Any, by_id: Mapping[str, dict]) -> list[str]:
    if not isinstance(assets, list) or len(_dicts(assets)) != len(assets):
        return [f"{key}: assets не список объектов"]
    problems = []
    for asset in assets:
        ref, kind, mime = asset.get("asset_ref"), asset.get("kind"), asset.get("mime")
        where = f"{key}: ассет {ref!r}"
        if not isinstance(ref, str) or not ASSET_REF_PATTERN.fullmatch(ref):
            problems.append(f"{where}: asset_ref не по шаблону {ASSET_REF_PATTERN.pattern}")
        if kind not in ASSET_MIME or ASSET_MIME[kind] != mime:
            problems.append(f"{where}: вид {kind!r} с mime {mime!r} вне С4")
        if by_id.get(_ref(asset.get("preset_uuid")), {}).get("kind") != "machine":
            problems.append(f"{where}: preset_uuid {asset.get('preset_uuid')!r} — не машина пакета")
        size = _asset_size(asset)
        if size is None:
            problems.append(f"{where}: data_base64 не читается")
        elif size > MAX_ASSET_BYTES:
            problems.append(f"{where}: весит {size} байт, предел {MAX_ASSET_BYTES}")
    return problems


def _shared_problems(packages: Mapping[str, Any]) -> list[str]:
    first: dict[str, tuple[str, dict]] = {}
    problems = []
    for key in sorted(packages):
        for entry in _entries(packages[key]):
            pid = _ref(entry.get("preset_uuid"))
            if pid is None:
                continue
            other_key, other = first.setdefault(pid, (key, entry))
            if other_key == key:
                continue
            differ = [field for field in ("kind", "name", "content") if entry.get(field) != other.get(field)]
            if differ:
                problems.append(f"id {pid}: разные {', '.join(differ)} в {other_key} и {key}")
    return problems


def _vendor_size_problems(packages: Mapping[str, Any]) -> list[str]:
    presets: dict[str, int] = {}
    assets: dict[str, int] = {}
    for key in sorted(packages):
        for entry in _entries(packages[key]):
            pid = _ref(entry.get("preset_uuid"))
            if pid is not None and pid not in presets and isinstance(entry.get("content"), dict):
                presets[pid] = len(dump_json(entry["content"]))
        pkg_assets = packages[key].get("assets") if isinstance(packages[key], dict) else None
        for asset in _dicts(pkg_assets):
            ref, size = _ref(asset.get("asset_ref")), _asset_size(asset)
            if ref is not None and size is not None:
                assets.setdefault(ref, size)
    total = sum(presets.values()) + sum(assets.values())
    if total > MAX_VENDOR_BYTES:
        return [f"производитель весит {total} байт, предел {MAX_VENDOR_BYTES}"]
    return []
