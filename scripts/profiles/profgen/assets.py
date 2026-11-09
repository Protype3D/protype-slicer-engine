"""Ассеты стола: STL-модель и SVG-разметка из машинной модели Orca и значения, которые на них ссылаются."""
import hashlib
from collections.abc import Sequence
from dataclasses import dataclass
from typing import Any

from .errors import GeneratorError
from .orca_tree import MachineModel, VendorTree

MAX_ASSET_BYTES = 8 * 1024 * 1024
ASSET_REF_LENGTH = 32

# расширение -> (kind, mime, ключ значения машины)
_KINDS = {
    ".stl": ("bed_model", "model/stl", "bed_visual_model_asset"),
    ".svg": ("bed_overlay", "image/svg+xml", "bed_visual_overlay_asset"),
}
_KIND_ORDER = ("bed_model", "bed_overlay")


@dataclass(frozen=True)
class BedAsset:
    kind: str
    asset_ref: str
    mime: str
    data: bytes
    repo_path: str
    value_key: str


def asset_ref(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()[:ASSET_REF_LENGTH]


def _extension(file_name: str) -> str:
    dot = file_name.rfind(".")
    return file_name[dot:].lower() if dot >= 0 else ""


def _load(tree: VendorTree, file_name: str, spec: tuple[str, str, str]) -> BedAsset:
    data, repo_path = tree.vendor_file(file_name)
    if len(data) > MAX_ASSET_BYTES:
        raise GeneratorError(
            f"файл стола {file_name!r} весит {len(data)} байт, предел {MAX_ASSET_BYTES}")
    kind, mime, value_key = spec
    return BedAsset(kind, asset_ref(data), mime, data, repo_path, value_key)


def bed_assets(tree: VendorTree, model: MachineModel, model_key: str) -> tuple[list[BedAsset], list[dict]]:
    assets: list[BedAsset] = []
    skipped: list[dict] = []

    def skip(file_name: str, reason: str) -> None:
        skipped.append({"model": model_key, "file": file_name, "reason": reason})

    for file_name in (model.bed_model, model.bed_texture):
        if not file_name:
            continue
        extension = _extension(file_name)
        if extension in _KINDS:
            assets.append(_load(tree, file_name, _KINDS[extension]))
        elif extension == ".png":
            skip(file_name, "png texture")
        else:
            skip(file_name, "unsupported type")
    if model.hotend_model:
        skip(model.hotend_model, "hotend model")
    assets.sort(key=lambda a: _KIND_ORDER.index(a.kind))
    return assets, skipped


def bed_values(assets: Sequence[BedAsset], printable_area: Sequence[Sequence[float]]) -> dict[str, Any]:
    values: dict[str, Any] = {}
    for asset in assets:
        values[asset.value_key] = asset.asset_ref
        if asset.kind == "bed_overlay":
            values["bed_visual_overlay_enabled"] = True
        elif asset.kind == "bed_model":
            if not printable_area:
                raise GeneratorError("printable_area пуст: центр стола не посчитать")
            xs = [float(p[0]) for p in printable_area]
            ys = [float(p[1]) for p in printable_area]
            values["bed_visual_model_offset_x"] = (min(xs) + max(xs)) / 2
            values["bed_visual_model_offset_y"] = (min(ys) + max(ys)) / 2
            values["bed_visual_model_offset_z"] = 0.0
    return values
