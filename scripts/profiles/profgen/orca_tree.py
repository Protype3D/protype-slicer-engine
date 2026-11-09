"""Дерево производителя Orca: индекс имён, плоские пресеты, модели машин, файлы стола."""
import importlib.util
import json
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from .errors import GeneratorError
from .orca_source import SourceTree

_FLATTEN_PATH = Path(__file__).resolve().parents[2] / "flatten_orca_preset.py"
_spec = importlib.util.spec_from_file_location("flatten_orca_preset", _FLATTEN_PATH)
_flatten = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_flatten)

_LISTS = {
    "machine_model": "machine_model_list",
    "machine": "machine_list",
    "process": "process_list",
    "filament": "filament_list",
}


@dataclass(frozen=True)
class FlatPreset:
    kind: str
    name: str
    values: dict[str, Any]
    paths: tuple[str, ...]


@dataclass(frozen=True)
class MachineModel:
    name: str
    nozzles: tuple[str, ...]
    bed_model: str | None
    bed_texture: str | None
    hotend_model: str | None
    path: str


class VendorTree:
    def __init__(self, source: SourceTree, vendor_dir: str):
        self.source = source
        self._vendor_dir = vendor_dir
        self._vendor_root = source.profiles_root / vendor_dir
        self._repo_root = source.profiles_root.parent.parent
        self._index = self._read_index(source.profiles_root / f"{vendor_dir}.json")
        self._flat_cache: dict[str, list[FlatPreset]] = {}

    def _read_index(self, index_file: Path) -> dict[str, dict[str, str]]:
        try:
            doc = json.loads(index_file.read_text(encoding="utf-8"))
        except (OSError, ValueError) as exc:
            raise GeneratorError(f"индекс производителя не читается: {index_file.name}: {exc}") from exc
        index: dict[str, dict[str, str]] = {}
        for kind, list_key in _LISTS.items():
            entries: dict[str, str] = {}
            for entry in doc.get(list_key, []):
                entries[entry["name"]] = str(self._vendor_root / entry["sub_path"])
            index[kind] = entries
        return index

    def _repo_path(self, path: str) -> str:
        return Path(path).relative_to(self._repo_root).as_posix()

    def _flat(self, kind: str) -> list[FlatPreset]:
        cached = self._flat_cache.get(kind)
        if cached is not None:
            return cached
        index = self._index[kind]
        profile_dir = str(self._vendor_root / kind)
        presets = []
        for name in sorted(index):
            try:
                values, paths = _flatten.load_chain_traced(profile_dir, name, index)
            except SystemExit as exc:
                raise GeneratorError(f"{kind} {name!r}: {exc}") from exc
            presets.append(FlatPreset(kind, name, values, tuple(self._repo_path(p) for p in paths)))
        self._flat_cache[kind] = presets
        return presets

    def machine_model(self, name: str) -> MachineModel:
        path = self._index["machine_model"].get(name)
        if path is None:
            raise GeneratorError(f"модель машины {name!r} не найдена в индексе {self._vendor_dir}")
        try:
            doc = json.loads(Path(path).read_text(encoding="utf-8"))
        except (OSError, ValueError) as exc:
            raise GeneratorError(f"модель машины {name!r}: {exc}") from exc
        nozzles = tuple(sorted(
            (n.strip() for n in str(doc.get("nozzle_diameter", "")).split(";") if n.strip()),
            key=float))
        return MachineModel(
            name, nozzles, doc.get("bed_model") or None, doc.get("bed_texture") or None,
            doc.get("hotend_model") or None, self._repo_path(path))

    def machines_of(self, printer_model: str) -> list[FlatPreset]:
        return [
            p for p in self._flat("machine")
            if p.values.get("instantiation") == "true" and p.values.get("printer_model") == printer_model
        ]

    def instantiable(self, kind: str) -> list[FlatPreset]:
        if kind not in ("process", "filament"):
            raise GeneratorError(f"instantiable: вид {kind!r} не поддерживается, ждали process или filament")
        return [p for p in self._flat(kind) if p.values.get("instantiation") == "true"]

    def vendor_file(self, file_name: str) -> tuple[bytes, str]:
        path = self._vendor_root / file_name
        if self._vendor_root.resolve() not in path.resolve().parents:
            raise GeneratorError(f"файл производителя {file_name!r} вне каталога {self._vendor_dir}")
        try:
            data = path.read_bytes()
        except OSError as exc:
            raise GeneratorError(f"файл производителя {file_name!r} не читается: {exc}") from exc
        return data, self._repo_path(str(path))
