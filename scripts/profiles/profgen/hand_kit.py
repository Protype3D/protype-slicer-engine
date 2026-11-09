"""Ручной комплект модели (Д3): пресеты со значениями Protype вместо значений Orca.

Решение Влада 07.11.2026: значения комплекта X-Max 4, на которых уже печатают, не меняются до сверки
с Corpo4. Записи `presets.json` Slicer лежат дословно в `kits/<vendor>.json`, а `source` в файле
говорит, откуда их скопировали. Файл публикует открытое зеркало движка, поэтому `origin` пресетов
указывает на него и на коммит генератора, где он лежит: закрытый Slicer получателю пакета не нужен.
Машина и экструдер комплекта встают на место машины Orca `replaces`, процесс и пластик добавляются к
пресетам Orca. Из записи берутся `values`, `display_name`, `version` и даты; производитель и модель
у пресета пакетные.
"""
import json
import re
import subprocess
from collections.abc import Mapping
from dataclasses import dataclass
from hashlib import sha1
from pathlib import Path
from typing import Any

from .errors import GeneratorError
from .ids import ID_PATTERN

KIND_OF: dict[str, str] = {"Machine": "machine", "Extruder": "extruder", "Process": "process", "Filament": "filament"}
SOURCE_KEYS = ("repo", "commit", "path", "blob")
FULL_SHA = re.compile(r"^[0-9a-f]{40}$")


@dataclass(frozen=True)
class HandPreset:
    kind: str
    id: str
    display_name: str
    version: int
    created_at: str
    updated_at: str
    label: str
    values: Mapping[str, Any]


@dataclass(frozen=True)
class MachineAdditions:
    """Добавки к ручной машине, которых нет в `presets.json` и которые не двигают голову.

    `from_orca` — ключи, взятые из замещённой машины Orca (`thumbnails`); `bed_assets` — файлы
    стола модели с ключами их показа, как у остальных машин; `version` — новая версия машины,
    чтобы посеянные копии обновились; `keys` — ровно те ключи, которыми машина отходит от
    `presets.json` (генератор сверяет).
    """
    version: int | None = None
    from_orca: tuple[str, ...] = ()
    bed_assets: bool = False
    keys: tuple[str, ...] = ()

    def as_report(self) -> dict[str, Any]:
        return {"version": self.version, "from_orca": list(self.from_orca), "bed_assets": self.bed_assets,
                "keys": list(self.keys)}


ADDITION_FIELDS = ("version", "from_orca", "bed_assets", "keys")


@dataclass(frozen=True)
class HandKit:
    model_key: str
    replaces: str
    source: Mapping[str, str]
    presets: Mapping[str, HandPreset]
    path: str
    commit: str
    blob: str
    additions: MachineAdditions | None = None


def git_blob_id(raw: bytes) -> str:
    """Id файла в git (`git hash-object`): по нему файл узнаётся в любом клоне, в том числе в зеркале."""
    return sha1(b"blob %d\0" % len(raw) + raw).hexdigest()


def load_hand_kit(path: Path, repo_path: str, commit: str) -> HandKit:
    """Снимок ручного комплекта, лежащий в репозитории по `repo_path` в коммите `commit`.

    GeneratorError, если снимок неполон или противоречив.
    """
    try:
        raw = Path(path).read_bytes()
        doc = json.loads(raw)
    except (OSError, ValueError) as exc:
        raise GeneratorError(f"ручной комплект {path} не читается: {exc}") from exc
    if not isinstance(doc, dict):
        raise GeneratorError(f"ручной комплект {path}: не объект")
    if not repo_path:
        raise GeneratorError(f"ручной комплект {path}: пустой path в репозитории")
    if not FULL_SHA.fullmatch(commit):
        raise GeneratorError(f"ручной комплект {path}: commit {commit!r} — не полный sha")
    return _parse(doc, str(path), repo_path, commit, git_blob_id(raw))


def committed_location(path: Path) -> tuple[str, str]:
    """Путь файла от корня его git-репозитория и последний коммит, менявший его; файл должен быть чист."""
    def run(cwd: Path, *args: str) -> str:
        try:
            done = subprocess.run(["git", "-C", str(cwd), *args], check=True, capture_output=True, text=True)
        except (OSError, subprocess.CalledProcessError) as exc:
            detail = getattr(exc, "stderr", "") or exc
            raise GeneratorError(f"ручной комплект {path}: git не ответил: {str(detail).strip()}") from exc
        return done.stdout.strip()

    top = Path(run(Path(path).resolve().parent, "rev-parse", "--show-toplevel")).resolve()
    rel = Path(path).resolve().relative_to(top).as_posix()
    commit = "" if run(top, "status", "--porcelain", "--", rel) else run(top, "log", "-1", "--format=%H", "--", rel)
    if not commit:
        raise GeneratorError(f"ручной комплект {rel} незакоммичен: origin пресетов указал бы на коммит без него")
    return rel, commit


def _text(doc: Mapping[str, Any], key: str, where: str) -> str:
    value = doc.get(key)
    if not isinstance(value, str) or not value:
        raise GeneratorError(f"{where}: поле {key} — не непустая строка")
    return value


def _parse(doc: Mapping[str, Any], where: str, repo_path: str, commit: str, blob: str) -> HandKit:
    model_key, replaces = _text(doc, "model", where), _text(doc, "replaces", where)
    source = doc.get("source")
    if not isinstance(source, dict):
        raise GeneratorError(f"{where}: source не объект")
    source = {key: _text(source, key, f"{where} source") for key in SOURCE_KEYS}
    entries = doc.get("presets")
    if not isinstance(entries, list) or not all(isinstance(e, dict) for e in entries):
        raise GeneratorError(f"{where}: presets не список объектов")
    presets = [_preset(e, where) for e in entries]
    by_kind = {p.kind: p for p in presets}
    counts = {kind: sum(1 for p in presets if p.kind == KIND_OF[kind]) for kind in KIND_OF}
    if any(count != 1 for count in counts.values()):
        raise GeneratorError(f"{where}: нужно ровно по одному пресету каждого вида, есть {counts}")
    labels = {p.label for p in presets}
    if len(labels) != 1:
        raise GeneratorError(f"{where}: у пресетов разный bundle_version {sorted(labels)}")
    slots = by_kind["machine"].values.get("extruder_slots")
    if slots != [by_kind["extruder"].id]:
        raise GeneratorError(f"{where}: extruder_slots машины {slots!r} не указывают на экструдер комплекта")
    additions = _additions(doc.get("additions"), where)
    return HandKit(model_key, replaces, source, by_kind, repo_path, commit, blob, additions)


def _additions(raw: Any, where: str) -> MachineAdditions | None:
    if raw is None:
        return None
    if not isinstance(raw, dict):
        raise GeneratorError(f"{where}: additions не объект")
    kinds = sorted(k for k in raw if not k.startswith("_"))
    if kinds != ["machine"]:
        raise GeneratorError(f"{where}: additions бывают только у machine, есть {kinds}")
    machine = raw["machine"]
    if not isinstance(machine, dict):
        raise GeneratorError(f"{where}: additions.machine не объект")
    extra = sorted(set(machine) - set(ADDITION_FIELDS))
    if extra:
        raise GeneratorError(f"{where}: additions.machine — лишние поля {extra}")
    version, from_orca, bed = machine.get("version"), machine.get("from_orca", []), machine.get("bed_assets", False)
    keys = machine.get("keys")
    if not isinstance(keys, list) or not keys or not all(isinstance(k, str) and k for k in keys):
        raise GeneratorError(f"{where}: additions.machine.keys {keys!r} — нужен непустой список ключей")
    if version is not None and (not isinstance(version, int) or isinstance(version, bool) or version < 1):
        raise GeneratorError(f"{where}: additions.machine.version {version!r} — не целое ≥ 1")
    if not isinstance(from_orca, list) or not all(isinstance(k, str) and k for k in from_orca):
        raise GeneratorError(f"{where}: additions.machine.from_orca {from_orca!r} — не список ключей")
    if not isinstance(bed, bool):
        raise GeneratorError(f"{where}: additions.machine.bed_assets {bed!r} — не true/false")
    return MachineAdditions(version, tuple(from_orca), bed, tuple(sorted(keys)))


def _preset(entry: Mapping[str, Any], where: str) -> HandPreset:
    kind = entry.get("kind")
    if kind not in KIND_OF:
        raise GeneratorError(f"{where}: вид {kind!r} не из {list(KIND_OF)}")
    pid = entry.get("id")
    if not isinstance(pid, str) or not ID_PATTERN.fullmatch(pid):
        raise GeneratorError(f"{where}: {kind} id {pid!r} не по шаблону С3")
    here = f"{where} {kind} {pid}"
    version, values = entry.get("version"), entry.get("values")
    if not isinstance(version, int) or isinstance(version, bool) or version < 1:
        raise GeneratorError(f"{here}: version {version!r} — не целое ≥ 1")
    if not isinstance(values, dict) or not values:
        raise GeneratorError(f"{here}: values пусты или не объект")
    return HandPreset(KIND_OF[kind], pid, _text(entry, "display_name", here), version,
                      _text(entry, "created_at", here), _text(entry, "updated_at", here),
                      _text(entry, "bundle_version", here), values)
