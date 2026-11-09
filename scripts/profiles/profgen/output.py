"""Выход генератора: единственная сериализация JSON и запись каталога."""
import json
import os
import tempfile
from pathlib import Path
from typing import TYPE_CHECKING, Any

from .errors import GeneratorError

if TYPE_CHECKING:
    from .package import VendorBuild

PACKAGES_DIR = "packages"
REPORT_FILE = "report.json"
TRANSLATION_FILE = "agent_to_engine.json"


def dump_json(obj: Any) -> bytes:
    """JSON с сортировкой ключей, отступом 1, UTF-8 и переводом строки в конце."""
    return (json.dumps(obj, ensure_ascii=False, indent=1, sort_keys=True, allow_nan=False) + "\n").encode("utf-8")


def _write_atomic(path: Path, data: bytes) -> None:
    """Временный файл рядом, затем `os.replace`; при сбое временный файл удаляется."""
    path.parent.mkdir(parents=True, exist_ok=True)
    handle, temp_name = tempfile.mkstemp(dir=path.parent, prefix=f".{path.name}.", suffix=".tmp")
    try:
        with os.fdopen(handle, "wb") as temp:
            temp.write(data)
        os.replace(temp_name, path)
    except BaseException:
        Path(temp_name).unlink(missing_ok=True)
        raise


def _refuse_non_files(packages_dir: Path) -> None:
    if not packages_dir.is_dir():
        return
    for entry in sorted(packages_dir.iterdir()):
        if entry.is_symlink() or not entry.is_file():
            raise GeneratorError(
                f"{entry}: в packages/ должны лежать только обычные файлы, каталог или ссылку удалите сами")


def write_output(build: "VendorBuild", out_dir: Path) -> list[Path]:
    """Пишет `packages/<ключ>.json`, `report.json` и `agent_to_engine.json`; из `packages/` убирает всё, чего не писал.

    Всё прочее в `out_dir` не трогается. Возвращает записанные пути по возрастанию.
    """
    out_dir = Path(out_dir)
    packages_dir = out_dir / PACKAGES_DIR
    _refuse_non_files(packages_dir)

    written = [packages_dir / f"{key}.json" for key in build.packages]
    for key, path in zip(build.packages, written):
        _write_atomic(path, dump_json(build.packages[key]))
    report_path = out_dir / REPORT_FILE
    _write_atomic(report_path, dump_json(build.report))
    translation_path = out_dir / TRANSLATION_FILE
    _write_atomic(translation_path, dump_json(build.agent_to_engine))

    keep = set(written)
    for entry in packages_dir.iterdir():
        if entry not in keep:
            entry.unlink()
    return sorted([*written, report_path, translation_path])
