#!/usr/bin/env python3
"""CLI генератора профилей: дерево Orca по тегу -> пакеты моделей и отчёт."""
import argparse
import os
import sys
from collections.abc import Sequence
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

from profgen import orca_source  # noqa: E402
from profgen.errors import GeneratorError  # noqa: E402
from profgen.hand_kit import committed_location, load_hand_kit  # noqa: E402
from profgen.keymap import KeyMap  # noqa: E402
from profgen.limits import load_limits  # noqa: E402
from profgen.orca_tree import VendorTree  # noqa: E402
from profgen.output import write_output  # noqa: E402
from profgen.package import _ru_count, build_vendor  # noqa: E402
from profgen.vendors import qidi  # noqa: E402

VENDORS = {"qidi": qidi}
EXIT_OK = 0
EXIT_ERROR = 2
CACHE_SUBDIR = "protype-profiles"
PACKAGE_WORDS = ("пакет", "пакета", "пакетов")
PRESET_WORDS = ("пресет", "пресета", "пресетов")


def default_cache_dir() -> Path:
    xdg = os.environ.get("XDG_CACHE_HOME")
    base = Path(xdg) if xdg else Path.home() / ".cache"
    return base / CACHE_SUBDIR


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="generate.py", description="Профили производителя из OrcaSlicer -> пакеты моделей Protype")
    parser.add_argument("--vendor", required=True, choices=sorted(VENDORS))
    parser.add_argument("--orca-tag", required=True, metavar="TAG")
    parser.add_argument("--schema", required=True, type=Path, metavar="PATH")
    parser.add_argument("--libslic3r-keys", required=True, type=Path, metavar="PATH")
    parser.add_argument("--out", required=True, type=Path, metavar="DIR")
    parser.add_argument("--limits", type=Path, metavar="PATH",
                        help="по умолчанию limits/<vendor>.json рядом со скриптом")
    parser.add_argument("--hand-kit", type=Path, metavar="PATH",
                        help="ручной комплект (Д3); по умолчанию kits/<vendor>.json рядом со скриптом, если он есть")
    parser.add_argument("--cache-dir", type=Path, metavar="DIR",
                        help="по умолчанию $XDG_CACHE_HOME/protype-profiles или ~/.cache/protype-profiles")
    return parser


def _load_kit(path: Path):
    """Комплект с местом в репозитории: файл читается до git, чтобы отсутствующий назвался по имени."""
    if not path.is_file():
        raise GeneratorError(f"ручной комплект {path} не найден")
    return load_hand_kit(path, *committed_location(path))


def hand_kit_path(vendor_id: str, given: Path | None) -> Path | None:
    """Названный файл ручного комплекта либо `kits/<vendor>.json`, если он есть; иначе None."""
    if given is not None:
        return given
    default = SCRIPT_DIR / "kits" / f"{vendor_id}.json"
    return default if default.is_file() else None


def _summary(vendor_id: str, build, out: Path) -> str:
    counts = build.report["counts"]
    kinds = ", ".join(f"{kind} {counts[kind]}" for kind in ("machine", "extruder", "process", "filament"))
    return (f"{vendor_id}: {_ru_count(len(build.packages), PACKAGE_WORDS)}, "
            f"{_ru_count(sum(counts.values()), PRESET_WORDS)} ({kinds}) → {out}")


def _run(args: argparse.Namespace) -> None:
    vendor = VENDORS[args.vendor]
    limits_path = args.limits or SCRIPT_DIR / "limits" / f"{args.vendor}.json"
    cache_dir = args.cache_dir or default_cache_dir()
    try:
        keymap = KeyMap.load(args.schema, args.libslic3r_keys)
        limits = load_limits(limits_path)
    except OSError as error:
        raise GeneratorError(f"не прочитан входной файл {error.filename}: {error.strerror}") from error
    kit_path = hand_kit_path(args.vendor, args.hand_kit)
    hand_kits = [_load_kit(kit_path)] if kit_path is not None else []
    source = orca_source.fetch_vendor_tree(args.orca_tag, vendor.VENDOR_DIR, cache_dir)
    build = build_vendor(vendor, VendorTree(source, vendor.VENDOR_DIR), keymap, limits, hand_kits=hand_kits)
    write_output(build, args.out)
    print(_summary(vendor.VENDOR_ID, build, args.out))


def main(argv: Sequence[str] | None = None) -> int:
    try:
        args = _parser().parse_args(argv)
    except SystemExit as exit_request:
        return EXIT_OK if exit_request.code in (0, None) else EXIT_ERROR
    try:
        _run(args)
    except GeneratorError as error:
        print(f"ошибка: {error}", file=sys.stderr)
        return EXIT_ERROR
    except OSError as error:
        print(f"ошибка: {error.filename or ''}{': ' if error.filename else ''}{error.strerror or error}".replace("\n", " "),
              file=sys.stderr)
        return EXIT_ERROR
    return EXIT_OK


if __name__ == "__main__":
    sys.exit(main())
