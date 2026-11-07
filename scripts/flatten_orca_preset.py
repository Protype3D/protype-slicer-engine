#!/usr/bin/env python3
"""Flatten an OrcaSlicer machine+process+filament preset triplet into one JSON.

OrcaSlicer profiles use an `inherits` chain (e.g. `Bambu Lab X1 Carbon 0.4
nozzle` → `fdm_bbl_3dp_001_common` → `fdm_machine_common`). libslic3r's
`DynamicPrintConfig::load_from_json` does NOT walk that chain itself — it
only loads flat keys from the file you hand it. Calling `Print::process()`
on a partially-populated config SIGSEGVs.

This script walks the chain off-line and writes a single resolved JSON
that bridge.cpp can feed straight into libslic3r at runtime.

Merge order (last wins): machine root → process root → filament root, then any
`--override KEY=VALUE`. Within each chain: parents → leaf.

Usage:
    flatten_orca_preset.py \\
        --orca-profiles /Applications/OrcaSlicer.app/Contents/Resources/profiles/BBL \\
        --machine "Bambu Lab X1 Carbon 0.4 nozzle" \\
        --process "0.20mm Standard @BBL X1C" \\
        --filament "Bambu PLA Basic @BBL X1C" \\
        --output profiles/protype-default-x1c-pla-0.20.json
"""
from __future__ import annotations

import argparse
import json
import os
import sys
from typing import Any

# JSON keys that are bookkeeping metadata (skipped by libslic3r's loader and
# meaningless to slicing). We strip them from the flattened output so the
# resulting file is purely engineering settings.
META_KEYS = {
    "type",
    "name",
    "inherits",
    "from",
    "description",
    "setting_id",
    "filament_id",
    "instantiation",
    "version",
    "is_custom",
    "url",
    "renamed_from",
}


def resolve_preset(profile_dir: str, leaf_dir: str, name: str) -> str:
    """Path of the `name` preset, looked up in profile_dir then leaf_dir.

    Vendors differ on layout: BBL keeps every filament preset directly under
    `filament/`, while Qidi nests per-series ones under `filament/X4/` and lets
    them inherit a common parent that sits beside them rather than in the kind's
    root. So a bare parent name is also tried next to the leaf that named it.
    When the leaf is unnested both candidates coincide and this is a no-op.
    """
    candidates = (
        os.path.join(profile_dir, name + ".json"),
        os.path.join(leaf_dir, os.path.basename(name) + ".json"),
    )
    for path in candidates:
        if os.path.exists(path):
            return path
    raise SystemExit("missing preset file, tried: " + ", ".join(repr(c) for c in candidates))


def load_chain(profile_dir: str, leaf: str) -> dict[str, Any]:
    """Load `leaf.json` from profile_dir, then walk `inherits` upward.

    `leaf` may carry a vendor subdirectory, e.g. `X4/Generic PLA @X-Max 4`.

    Returns a single dict where the leaf's keys override its parents.
    """
    leaf_dir = os.path.join(profile_dir, os.path.dirname(leaf))
    chain: list[dict[str, Any]] = []
    cur = leaf
    seen: set[str] = set()
    while cur:
        if cur in seen:
            raise SystemExit(f"inherits cycle detected at {cur!r} in {profile_dir}")
        seen.add(cur)
        with open(resolve_preset(profile_dir, leaf_dir, cur), encoding="utf-8") as f:
            j = json.load(f)
        chain.append(j)
        cur = j.get("inherits", "")

    # Merge root-first so leaf overrides.
    merged: dict[str, Any] = {}
    for j in reversed(chain):
        for k, v in j.items():
            merged[k] = v
    return merged


def strip_meta(d: dict[str, Any]) -> dict[str, Any]:
    return {k: v for k, v in d.items() if k not in META_KEYS}


def parse_override(spec: str) -> tuple[str, str]:
    """Split a `KEY=VALUE` override spec, rejecting anything malformed."""
    key, sep, value = spec.partition("=")
    if not sep or not key.strip():
        raise SystemExit(f"malformed --override {spec!r}, expected KEY=VALUE")
    return key.strip(), value


def apply_overrides(flat: dict[str, Any], specs: list[str]) -> dict[str, Any]:
    """Return a copy of `flat` with each `KEY=VALUE` override applied.

    A key the vendor chain never set is treated as a typo rather than a new
    setting: libslic3r would quietly fall back to its own default and the
    profile would advertise a value it does not actually carry.

    Per-extruder keys hold one entry per extruder, so a scalar override is
    broadcast across the existing arity instead of collapsing the array.
    """
    if not specs:
        return flat

    overridden = dict(flat)
    for key, value in (parse_override(s) for s in specs):
        if key not in overridden:
            raise SystemExit(f"--override {key!r}: not set by the preset chain, check the spelling")
        current = overridden[key]
        overridden[key] = [value] * max(len(current), 1) if isinstance(current, list) else value
    return overridden


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--orca-profiles", required=True, help="Path to OrcaSlicer profiles vendor dir, e.g. .../profiles/BBL")
    ap.add_argument("--machine", required=True, help='Leaf machine preset name (no .json), e.g. "Bambu Lab X1 Carbon 0.4 nozzle"')
    ap.add_argument("--process", required=True, help='Leaf process preset name, e.g. "0.20mm Standard @BBL X1C"')
    ap.add_argument("--filament", required=True, help='Leaf filament preset name, e.g. "Bambu PLA Basic @BBL X1C"')
    ap.add_argument("--output", required=True, help="Output flat JSON path")
    ap.add_argument(
        "--override",
        action="append",
        default=[],
        metavar="KEY=VALUE",
        help="Override one merged key after flattening, e.g. --override retraction_speed=50. "
        "Repeatable; the key must already be set by the preset chain. Recorded in "
        "protype_source_overrides so the file stays reproducible from the command line.",
    )
    args = ap.parse_args()

    machine = strip_meta(load_chain(os.path.join(args.orca_profiles, "machine"), args.machine))
    process = strip_meta(load_chain(os.path.join(args.orca_profiles, "process"), args.process))
    filament = strip_meta(load_chain(os.path.join(args.orca_profiles, "filament"), args.filament))

    # Final merge: machine ← process ← filament. In practice these three
    # touch disjoint keys (printer geometry vs slicing params vs material),
    # so collisions are rare; documented order keeps the output stable.
    merged: dict[str, Any] = {}
    merged.update(machine)
    merged.update(process)
    merged.update(filament)

    # Deliberate deviations from the vendor chain, applied last so they win.
    flat = apply_overrides(merged, args.override)

    # Provenance — readable in the file but ignored by libslic3r's loader.
    # Must be string-shaped (not nested object/dict): libslic3r's
    # load_from_json walks every top-level key, and an unknown object value
    # trips its scalar/array deserializer with a hard parse error.
    flat["protype_source_machine"] = args.machine
    flat["protype_source_process"] = args.process
    flat["protype_source_filament"] = args.filament
    flat["protype_source_vendor_root"] = args.orca_profiles
    flat["protype_source_tool"] = "scripts/flatten_orca_preset.py"
    if args.override:
        flat["protype_source_overrides"] = ",".join(
            sorted(f"{k}={v}" for k, v in (parse_override(s) for s in args.override))
        )

    os.makedirs(os.path.dirname(args.output) or ".", exist_ok=True)
    with open(args.output, "w", encoding="utf-8") as f:
        json.dump(flat, f, indent=2, sort_keys=True)
        f.write("\n")

    print(
        f"wrote {args.output} ({len(flat)} keys) — machine={args.machine!r} "
        f"process={args.process!r} filament={args.filament!r}",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
