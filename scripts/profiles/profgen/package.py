"""Пакеты моделей производителя: документы пресетов, набор по умолчанию, источники, отчёт.

Один пакет на модель — тело импорта ProRemote (`ImportPackageRequestDTO`) с полями `license`, `sources`, `model`.
Проверка пакетов (С4) — в `validate.py`, её константы доступны и отсюда.
"""
import base64
import copy
from collections.abc import Iterable, Mapping, Sequence
from dataclasses import dataclass
from types import ModuleType
from typing import Any

from .assets import BedAsset, bed_assets, bed_values
from .engine_compat import apply_template_constants
from .errors import GeneratorError
from .hand_kit import HandKit, HandPreset
from .ids import IdAllocator
from .keymap import CARRY_AHEAD, Converted, KeyMap, convert, normalize_thumbnails
from .limits import ModelLimits
from .orca_tree import FlatPreset, VendorTree
from .report import ReportBuilder
from .rules import RuleHit
from .validate import (CONTENT_KIND, KIND_ORDER, MAX_PRESET_JSON_BYTES, MAX_VENDOR_BYTES, NOZZLE_PATTERN,
                       validate_vendor)

__all__ = [
    "GENERATOR_URL", "KIND_ORDER", "LICENSE", "MAX_PRESET_JSON_BYTES", "MAX_VENDOR_BYTES",
    "VendorBuild", "build_vendor", "pick_default", "validate_vendor",
]

LICENSE = "AGPL-3.0-or-later"
GENERATOR_URL = "https://github.com/dinamicby/protype-slicer-engine"
ANY_NOZZLE = "any"
PROCESS_WORDS = ("процесс", "процесса", "процессов")
FILAMENT_WORDS = ("пластик", "пластика", "пластиков")
NOZZLE_TOLERANCE_MM = 0.01


@dataclass(frozen=True)
class VendorBuild:
    packages: dict[str, dict]
    report: dict
    agent_to_engine: dict


@dataclass(frozen=True)
class _Build:
    vendor: ModuleType
    tree: VendorTree
    keymap: KeyMap
    limits: Mapping[str, ModelLimits]
    ids: IdAllocator
    report: ReportBuilder
    hand: Mapping[str, HandKit]


@dataclass(frozen=True)
class _Provenance:
    """Откуда пресет: `origin`, версия и даты его документа."""
    origin: dict[str, str]
    version: int
    created_at: str
    updated_at: str


@dataclass(frozen=True)
class _Machine:
    model: Any
    preset: FlatPreset
    nozzle: str
    machine_id: str
    extruder_id: str


@dataclass(frozen=True)
class _ModelPart:
    model: Any
    machine_model_path: str
    machines: tuple[_Machine, ...]
    assets: tuple[BedAsset, ...]


@dataclass(frozen=True)
class _Doc:
    entry: dict[str, Any]
    paths: tuple[str, ...]
    model_keys: tuple[str, ...]


def build_vendor(vendor: ModuleType, tree: VendorTree, keymap: KeyMap, limits: Mapping[str, ModelLimits],
                 models: Sequence | None = None, hand_kits: Sequence[HandKit] | None = None) -> VendorBuild:
    """Пакеты выбранных моделей (`None` — `vendor.MODELS` на момент вызова) и отчёт.

    `hand_kits` — ручные комплекты моделей (Д3): их пресеты выходят со значениями Protype.
    """
    selected = _selected_models(vendor, models)
    ctx = _Build(vendor, tree, keymap, limits, IdAllocator(vendor.VENDOR_ID),
                 ReportBuilder(vendor.VENDOR_ID, KIND_ORDER), _hand_by_model(hand_kits or (), selected))
    parts = [_model_part(ctx, model) for model in selected]
    machines = {m.preset.name: m for part in parts for m in part.machines}
    for kit in ctx.hand.values():
        _check_replaced(kit, machines)
    docs = [doc for part in parts for m in part.machines for doc in _machine_docs(ctx, m, part.assets)]
    compatible: dict[str, dict[str, list[str]]] = {}
    for kind in ("process", "filament"):
        kind_docs, compatible[kind] = _shared_docs(ctx, kind, machines)
        docs += kind_docs
    docs += [_hand_doc(ctx, kit, kit.presets[kind]) for kit in ctx.hand.values() for kind in ("process", "filament")]
    packages = {part.model.key: _package(ctx, part, docs, compatible) for part in parts}
    for kit in ctx.hand.values():
        ctx.report.hand_kit(kit)
    ctx.ids.check()
    validate_vendor(packages)
    unverified = [m.key for m in selected if limits[m.key].printer_cfg is None]
    outside = {key for scope, key in vendor.RULE_TARGETS.values() if keymap.target(scope, key) is None}
    return VendorBuild(packages, ctx.report.build(tree.source, keymap, unverified, outside),
                       keymap.agent_to_engine())


def pick_default(kind: str, wanted: str | None, compatible: Iterable[str], machine: str) -> str:
    """Пресет набора по умолчанию: сам `wanted` или единственный совместимый с тем же именем до « @»."""
    if not wanted:
        raise GeneratorError(f"машина {machine!r}: у неё нет {kind} по умолчанию")
    names = list(compatible)
    if wanted in names:
        return wanted
    same = sorted({name for name in names if _name_stem(name) == _name_stem(wanted)})
    if len(same) == 1:
        return same[0]
    raise GeneratorError(
        f"машина {machine!r}: {kind} по умолчанию {wanted!r} с ней несовместим, "
        f"а совместимых с тем же именем до « @» {len(same)}: {same}")


def _name_stem(name: str) -> str:
    return str(name).split(" @", 1)[0]


def _first(value: Any) -> Any:
    if isinstance(value, (list, tuple)):
        return value[0] if value else None
    return value


def _selected_models(vendor: ModuleType, models: Sequence | None) -> tuple:
    chosen = tuple(vendor.MODELS if models is None else models)
    if not chosen:
        raise GeneratorError(f"{vendor.VENDOR_ID}: список моделей пуст")
    keys = [m.key for m in chosen]
    repeated = sorted({k for k in keys if keys.count(k) > 1})
    if repeated:
        raise GeneratorError(f"{vendor.VENDOR_ID}: модели повторяются: {', '.join(repeated)}")
    return chosen


def _hand_by_model(kits: Sequence[HandKit], selected: Sequence) -> dict[str, HandKit]:
    keys = {m.key for m in selected}
    by_model: dict[str, HandKit] = {}
    for kit in kits:
        if kit.model_key not in keys:
            raise GeneratorError(f"ручной комплект модели {kit.model_key}: этой модели нет в сборке")
        if by_model.setdefault(kit.model_key, kit) is not kit:
            raise GeneratorError(f"у модели {kit.model_key} два ручных комплекта")
    return by_model


def _check_replaced(kit: HandKit, machines: Mapping[str, "_Machine"]) -> None:
    """Машина Orca, чьё место занимает комплект, есть в модели; id и сопло комплекта — её."""
    machine = machines.get(kit.replaces)
    if machine is None or machine.model.key != kit.model_key:
        raise GeneratorError(f"ручной комплект {kit.model_key}: у модели нет машины {kit.replaces!r}")
    for kind, slot_id in (("machine", machine.machine_id), ("extruder", machine.extruder_id)):
        if kit.presets[kind].id != slot_id:
            raise GeneratorError(f"ручной комплект {kit.model_key}: {kind} {kit.presets[kind].id}, "
                                 f"а место {kit.replaces!r} закреплено за {slot_id}")
    raw = kit.presets["extruder"].values.get("nozzle_diameter")
    diameter = _first(raw)
    if not isinstance(diameter, (int, float)) or abs(float(diameter) - float(machine.nozzle)) > NOZZLE_TOLERANCE_MM:
        raise GeneratorError(f"ручной комплект {kit.model_key}: nozzle_diameter экструдера {raw!r}, "
                             f"а машина {kit.replaces!r} под сопло {machine.nozzle}")


def _model_part(ctx: _Build, model: Any) -> _ModelPart:
    presets = ctx.tree.machines_of(model.orca_name)
    if not presets:
        raise GeneratorError(f"модель {model.key} ({model.orca_name!r}): в дереве Orca нет её машин")
    if model.key not in ctx.limits:
        raise GeneratorError(f"модель {model.key}: нет строки в таблице лимитов")
    orca_model = ctx.tree.machine_model(model.orca_name)
    machines = tuple(sorted((_machine(ctx, model, p) for p in presets), key=lambda m: float(m.nozzle)))
    nozzles = [m.nozzle for m in machines]
    if len(set(nozzles)) != len(nozzles) or set(nozzles) != set(orca_model.nozzles):
        raise GeneratorError(
            f"модель {model.key}: сопла машин {nozzles} не совпадают с моделью Orca {list(orca_model.nozzles)}")
    assets, skipped = bed_assets(ctx.tree, orca_model, model.key)
    for entry in skipped:
        ctx.report.skip(entry)
    return _ModelPart(model, orca_model.path, machines, tuple(assets))


def _machine(ctx: _Build, model: Any, preset: FlatPreset) -> _Machine:
    raw = preset.values.get("printer_variant")
    if not isinstance(raw, str) or not NOZZLE_PATTERN.fullmatch(raw):
        raise GeneratorError(f"машина {preset.name!r}: printer_variant {raw!r} — не диаметр сопла вида 0.4")
    return _Machine(model, preset, raw, ctx.ids.id_for("machine", preset.name), ctx.ids.id_for("extruder", preset.name))


def _machine_docs(ctx: _Build, machine: _Machine, assets: Sequence[BedAsset]) -> tuple[_Doc, _Doc]:
    model, preset = machine.model, machine.preset
    kit = ctx.hand.get(model.key)
    if kit is not None and kit.replaces == preset.name:
        return _hand_machine_doc(ctx, kit, preset, assets), _hand_doc(ctx, kit, kit.presets["extruder"])
    model_limits = ctx.limits[model.key]
    thumbnails = normalize_thumbnails(preset.values)
    flat = dict(preset.values) if thumbnails is None else {**preset.values, "thumbnails": thumbnails}
    ruled = ctx.vendor.machine_rules(model, flat, model_limits)
    _record_hits(ctx.report, ruled.hits)
    ctx.report.limits(preset.name, ruled.limits)
    compat = apply_template_constants(ruled.values, preset.name)
    _record_hits(ctx.report, compat.hits)
    converted = _convert(ctx, "machine", compat.values, ("Machine", "Extruder"))
    values = converted.values["Machine"]
    if "printable_area" not in values:
        raise GeneratorError(f"машина {preset.name!r}: значение printable_area не дошло до схемы")
    derived = {**ctx.vendor.machine_extras(model, model_limits, machine.extruder_id),
               **bed_values(assets, values["printable_area"])}
    machine_doc = _doc(ctx, "machine", machine.machine_id, f"{model.display_name} {machine.nozzle} nozzle",
                       {**values, **_derived(ctx, "Machine", derived)}, [model.key], preset.paths)
    extruder_doc = _doc(ctx, "extruder", machine.extruder_id, f"{model.display_name} — {machine.nozzle} nozzle",
                        converted.values["Extruder"], [model.key], preset.paths)
    return machine_doc, extruder_doc


def _shared_docs(ctx: _Build, kind: str, machines: Mapping[str, _Machine]) -> tuple[list[_Doc], dict[str, list[str]]]:
    """Процессы или пластики, совместимые хоть с одной выбранной машиной, и их имена по машинам."""
    docs = []
    compatible: dict[str, list[str]] = {name: [] for name in machines}
    for preset in ctx.tree.instantiable(kind):
        listed = preset.values.get("compatible_printers") or []
        names = dict.fromkeys([listed] if isinstance(listed, str) else listed)
        hit = [machines[name] for name in names if name in machines]
        if not hit:
            continue
        for machine in hit:
            compatible[machine.preset.name].append(preset.name)
        docs.append(_shared_doc(ctx, kind, preset, hit))
    return docs, compatible


def _shared_doc(ctx: _Build, kind: str, preset: FlatPreset, machines: Sequence[_Machine]) -> _Doc:
    models = sorted({m.model.key: m.model for m in machines}.values(), key=lambda m: m.key)
    values = preset.values
    if kind == "filament":
        ruled = ctx.vendor.filament_rules(values, models, ctx.limits)
        _record_hits(ctx.report, ruled.hits)
        values = ruled.values
    compat = apply_template_constants(values, preset.name)
    _record_hits(ctx.report, compat.hits)
    values = compat.values
    scope = CONTENT_KIND[kind]
    converted = _convert(ctx, kind, values, (scope,))
    nozzles = {m.nozzle for m in machines}
    nozzle = next(iter(nozzles)) if len(nozzles) == 1 else ANY_NOZZLE
    derived = _derived(ctx, scope, {f"{kind}_for_nozzle": nozzle})
    return _doc(ctx, kind, ctx.ids.id_for(kind, preset.name), preset.name,
                {**converted.values[scope], **derived}, [m.key for m in models], preset.paths)


def _record_hits(report: ReportBuilder, hits: Iterable[RuleHit]) -> None:
    for rule_hit in hits:
        report.hit(rule_hit)


def _convert(ctx: _Build, source_kind: str, values: Mapping[str, Any], scopes: Sequence[str]) -> Converted:
    """`convert` и запись его потерь в отчёт: отброс по виду источника, отказ по области ключа схемы."""
    converted = convert(values, scopes, ctx.keymap)
    for key in converted.dropped:
        ctx.report.drop(source_kind, key)
    for key, reason in sorted(set(converted.rejected)):
        ctx.report.reject(_target_scope(ctx.keymap, scopes, key).lower(), key, reason)
    for key in converted.carried:
        ctx.report.carry(key)
    return converted


def _target_scope(keymap: KeyMap, scopes: Sequence[str], key: str) -> str:
    for scope in scopes:
        if keymap.has(scope, key):
            return scope
    carried = CARRY_AHEAD.get(key)
    if carried and carried[0] in scopes:
        return carried[0]
    raise GeneratorError(f"отказ по ключу {key!r}: его нет ни в одной области {list(scopes)}")


def _derived(ctx: _Build, scope: str, wanted: Mapping[str, Any]) -> dict[str, Any]:
    """Производные значения только для ключей схемы; остальные — в отчёт."""
    missing = sorted(key for key in wanted if not ctx.keymap.has(scope, key))
    for key in missing:
        ctx.report.derived_missing(key)
    return {key: value for key, value in wanted.items() if key not in missing}


def _hand_machine_doc(ctx: _Build, kit: HandKit, orca: FlatPreset, assets: Sequence[BedAsset]) -> _Doc:
    """Ручная машина и её добавки (`additions`): ключи замещённой машины Orca и показ файлов стола."""
    hand, add = kit.presets["machine"], kit.additions
    if add is None:
        return _hand_doc(ctx, kit, hand)
    extra: dict[str, Any] = {}
    if add.from_orca:
        thumbnails = normalize_thumbnails(orca.values)
        flat = dict(orca.values) if thumbnails is None else {**orca.values, "thumbnails": thumbnails}
        converted = convert({k: flat[k] for k in add.from_orca if k in flat}, ("Machine",), ctx.keymap)
        missing = [k for k in add.from_orca if k not in converted.values["Machine"]]
        if missing:
            raise GeneratorError(f"ручной комплект {kit.model_key}: у машины Orca {orca.name!r} нет {missing}")
        extra.update({k: converted.values["Machine"][k] for k in add.from_orca})
    if add.bed_assets:
        extra.update(_derived(ctx, "Machine", bed_values(assets, hand.values["printable_area"])))
    changed = sorted(k for k, v in extra.items() if k not in hand.values or hand.values[k] != v)
    if changed != list(add.keys):
        raise GeneratorError(f"ручной комплект {kit.model_key}: машина отходит от снимка ключами {changed}, "
                             f"а в additions.machine.keys названы {list(add.keys)}")
    return _hand_doc(ctx, kit, hand, extra, add.version)


def _hand_doc(ctx: _Build, kit: HandKit, preset: HandPreset, extra: Mapping[str, Any] | None = None,
              version: int | None = None) -> _Doc:
    """Пресет ручного комплекта: его значения дословно, ключи — только из схемы; `extra` — добавки."""
    scope = CONTENT_KIND[preset.kind]
    outside = sorted(key for key in preset.values if not ctx.keymap.has(scope, key))
    if outside:
        raise GeneratorError(f"ручной комплект {kit.model_key}: {preset.kind} {preset.id} — ключи вне схемы {outside}")
    origin = {"repo": GENERATOR_URL, "tag": preset.label, "commit": kit.commit, "path": kit.path,
              "license": LICENSE, "generator": GENERATOR_URL}
    provenance = _Provenance(origin, version or preset.version, preset.created_at, preset.updated_at)
    values = {**preset.values, **(extra or {})}
    return _doc(ctx, preset.kind, preset.id, preset.display_name, values, [kit.model_key], (), provenance)


def _orca_provenance(ctx: _Build, paths: tuple[str, ...]) -> _Provenance:
    source = ctx.tree.source
    origin = {"repo": source.repo, "tag": source.tag, "commit": source.commit, "path": paths[0],
              "license": LICENSE, "generator": GENERATOR_URL}
    return _Provenance(origin, 1, source.released_at, source.released_at)


def _doc(ctx: _Build, kind: str, preset_id: str, display_name: str, values: Mapping[str, Any],
         model_keys: Sequence[str], paths: tuple[str, ...], provenance: _Provenance | None = None) -> _Doc:
    """Документ пресета; без `provenance` — пресет Orca из файлов `paths`."""
    made = provenance or _orca_provenance(ctx, paths)
    placement = ({"printer_model": model_keys[0]} if kind == "machine"
                 else {"compatible_printer_models": sorted(model_keys)})
    content = {
        "schema_version": ctx.keymap.schema_version, "id": preset_id, "kind": CONTENT_KIND[kind],
        "source": "system", "vendor_id": ctx.vendor.VENDOR_ID, "display_name": display_name, "version": made.version,
        "inherits_id": None, "derived_from": None, "values": copy.deepcopy(dict(values)), "locks": {},
        **placement,
        "origin": dict(made.origin),
        "created_at": made.created_at, "updated_at": made.updated_at,
    }
    entry = {"preset_uuid": preset_id, "kind": kind, "role": "primary", "source": "system", "key": None,
             "name": display_name, "content": content, "physics": None}
    return _Doc(entry, paths, tuple(model_keys))


def _package(ctx: _Build, part: _ModelPart, docs: Sequence[_Doc],
             compatible: Mapping[str, Mapping[str, list[str]]]) -> dict:
    model, source = part.model, ctx.tree.source
    own = [doc for doc in docs if model.key in doc.model_keys]
    kit = {m.nozzle: _kit(ctx, m, compatible) for m in part.machines}
    entries = sorted((copy.deepcopy(doc.entry) for doc in own),
                     key=lambda e: (KIND_ORDER.index(e["kind"]), e["name"], e["preset_uuid"]))
    for entry in entries:
        ctx.report.count(entry["kind"], model.key, entry["preset_uuid"])
    nozzles = [m.nozzle for m in part.machines]
    files = {path for doc in own for path in doc.paths} | {part.machine_model_path} | {a.repo_path for a in part.assets}
    kit_hand = ctx.hand.get(model.key)
    hand_machine = next((m for m in part.machines if kit_hand and m.preset.name == kit_hand.replaces), None)
    description = _description(ctx.vendor, model, source.tag, nozzles, entries)
    if hand_machine is not None:
        description += f" Комплект {hand_machine.nozzle}: значения Protype."
    return {
        "scope": "official", "key": model.key, "name": model.display_name,
        "description": description,
        "vendor_id": ctx.vendor.VENDOR_ID, "printer_model_key": model.key,
        "presets": entries,
        "assets": _asset_entries(part, _bare_hand_machines(kit_hand, hand_machine)),
        "license": LICENSE,
        "sources": {"repo": source.repo, "tag": source.tag, "commit": source.commit, "files": sorted(files)},
        "model": {"display_name": model.display_name, "nozzles": nozzles, "default_kit": kit,
                  "verified": model.key in ctx.vendor.VERIFIED},
    }


def _kit(ctx: _Build, machine: _Machine, compatible: Mapping[str, Mapping[str, list[str]]]) -> dict[str, str]:
    values, name = machine.preset.values, machine.preset.name
    process = pick_default("process", _first(values.get("default_print_profile")),
                           compatible["process"][name], name)
    filament = pick_default("filament", _first(values.get("default_filament_profile")),
                            compatible["filament"][name], name)
    return {"machine": machine.machine_id, "extruder": machine.extruder_id,
            "process": ctx.ids.id_for("process", process), "filament": ctx.ids.id_for("filament", filament)}


def _bare_hand_machines(kit: HandKit | None, machine: _Machine | None) -> set[str]:
    """Ручная машина без добавки файлов стола: её значения на файлы не ссылаются."""
    if kit is None or machine is None or (kit.additions is not None and kit.additions.bed_assets):
        return set()
    return {machine.machine_id}


def _asset_entries(part: _ModelPart, skip: set[str]) -> list[dict]:
    """Ассеты стола модели на каждой её машине, кроме `skip`."""
    entries = [
        {"preset_uuid": m.machine_id, "kind": a.kind, "asset_ref": a.asset_ref, "mime": a.mime,
         "data_base64": base64.b64encode(a.data).decode("ascii")}
        for m in part.machines if m.machine_id not in skip for a in part.assets
    ]
    return sorted(entries, key=lambda e: (e["preset_uuid"], e["kind"]))


def _description(vendor: ModuleType, model: Any, tag: str, nozzles: Sequence[str], entries: Sequence[dict]) -> str:
    processes = sum(1 for e in entries if e["kind"] == "process")
    filaments = sum(1 for e in entries if e["kind"] == "filament")
    nozzle_word = "сопло" if len(nozzles) == 1 else "сопла"
    return (f"{model.display_name}: профили {vendor.VENDOR_NAME}-Official из OrcaSlicer {tag} — "
            f"{nozzle_word} {', '.join(nozzles)}; "
            f"{_ru_count(processes, PROCESS_WORDS)}, {_ru_count(filaments, FILAMENT_WORDS)}.")


def _ru_count(number: int, words: tuple[str, str, str]) -> str:
    """Число с существительным в нужной форме: 1 процесс, 3 процесса, 5 процессов."""
    one, few, many = words
    if number % 10 == 1 and number % 100 != 11:
        return f"{number} {one}"
    if 2 <= number % 10 <= 4 and not 12 <= number % 100 <= 14:
        return f"{number} {few}"
    return f"{number} {many}"
