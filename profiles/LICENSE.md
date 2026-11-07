# Bundled preset license

Each flat preset JSON in this directory is a build-time-resolved merge of
three OrcaSlicer vendor profiles.

`protype-default-x1c-pla-0.20.json` — Bambu Lab X1 Carbon, PLA, 0.20 mm:

| Triplet element | Source preset | Source path under OrcaSlicer.app/Contents/Resources/profiles |
|---|---|---|
| machine  | `Bambu Lab X1 Carbon 0.4 nozzle` | `BBL/machine/Bambu Lab X1 Carbon 0.4 nozzle.json` (+ `inherits` chain) |
| process  | `0.20mm Standard @BBL X1C`        | `BBL/process/0.20mm Standard @BBL X1C.json` (+ `inherits` chain) |
| filament | `Bambu PLA Basic @BBL X1C`        | `BBL/filament/Bambu PLA Basic @BBL X1C.json` (+ `inherits` chain) |

`protype-qidi-xmax4-pla-0.20.json` — QIDI X-Max 4, PLA, 0.20 mm:

| Triplet element | Source preset | Source path under OrcaSlicer.app/Contents/Resources/profiles |
|---|---|---|
| machine  | `Qidi X-Max 4 0.4 nozzle` | `Qidi/machine/Qidi X-Max 4 0.4 nozzle.json` (+ `inherits` chain) |
| process  | `0.20mm Standard @X-Max 4` | `Qidi/process/0.20mm Standard @X-Max 4.json` (+ `inherits` chain) |
| filament | `Generic PLA @X-Max 4`     | `Qidi/filament/X4/Generic PLA @X-Max 4.json` (+ `inherits` chain) |

`protype-qidi-xmax4-pla-matte-0.20.json` — QIDI X-Max 4, QIDI PLA Matte Basic, 0.20 mm:

| Triplet element | Source preset | Source path under OrcaSlicer.app/Contents/Resources/profiles |
|---|---|---|
| machine  | `Qidi X-Max 4 0.4 nozzle` | `Qidi/machine/Qidi X-Max 4 0.4 nozzle.json` (+ `inherits` chain) |
| process  | `0.20mm Standard @X-Max 4` | `Qidi/process/0.20mm Standard @X-Max 4.json` (+ `inherits` chain) |
| filament | `QIDI PLA Matte Basic @X-Max 4` | `Qidi/filament/X4/QIDI PLA Matte Basic @X-Max 4.json` (+ `inherits` chain) |

The merge is performed by `scripts/flatten_orca_preset.py` and walks each
leaf's `inherits` chain to root before merging machine → process → filament
into one flat `DynamicPrintConfig`-shaped JSON. See the script's docstring
for the merge order and the canonical regenerate command.

## Upstream license

OrcaSlicer is distributed under the **GNU Affero General Public License v3.0
or later** (`AGPL-3.0-or-later`). The vendor profile JSONs are part of the
OrcaSlicer source tree and therefore inherit the same license:

> SPDX-License-Identifier: AGPL-3.0-or-later
> Copyright (c) the OrcaSlicer project contributors
> See: https://github.com/SoftFever/OrcaSlicer/blob/main/LICENSE

The Bambu Lab and QIDI Tech vendor settings encoded in these JSONs were
originally contributed to OrcaSlicer by those vendors and the OrcaSlicer
community.

## Compatibility with this repository

`protype-slicer-daemon` is licensed `AGPL-3.0-or-later` (matching libslic3r
upstream). Redistributing OrcaSlicer profiles unmodified under the same
license is compatible — no relicensing is involved.

## How to regenerate

```bash
python3 scripts/flatten_orca_preset.py \
  --orca-profiles "/Applications/OrcaSlicer.app/Contents/Resources/profiles/BBL" \
  --machine "Bambu Lab X1 Carbon 0.4 nozzle" \
  --process "0.20mm Standard @BBL X1C" \
  --filament "Bambu PLA Basic @BBL X1C" \
  --output profiles/protype-default-x1c-pla-0.20.json
```

```bash
python3 scripts/flatten_orca_preset.py \
  --orca-profiles "/Applications/OrcaSlicer.app/Contents/Resources/profiles/Qidi" \
  --machine "Qidi X-Max 4 0.4 nozzle" \
  --process "0.20mm Standard @X-Max 4" \
  --filament "X4/Generic PLA @X-Max 4" \
  --override "retraction_speed=50" \
  --override "default_acceleration=20000" \
  --output profiles/protype-qidi-xmax4-pla-0.20.json
```

```bash
python3 scripts/flatten_orca_preset.py \
  --orca-profiles "/Applications/OrcaSlicer.app/Contents/Resources/profiles/Qidi" \
  --machine "Qidi X-Max 4 0.4 nozzle" \
  --process "0.20mm Standard @X-Max 4" \
  --filament "X4/QIDI PLA Matte Basic @X-Max 4" \
  --override "retraction_speed=50" \
  --override "default_acceleration=20000" \
  --output profiles/protype-qidi-xmax4-pla-matte-0.20.json
```

The QIDI filament leaf carries an `X4/` prefix because that vendor nests its
per-series filament presets in a subdirectory; the script resolves a parent
named by such a leaf next to the leaf itself before falling back to the kind's
root directory.

The two `--override` values are deliberate: OrcaSlicer's Qidi vendor presets are
more conservative here than QIDI's own slicer, which ships 50 mm/s retraction and
20000 mm/s² default acceleration for this machine (both well inside the printer's
Klipper `max_accel: 30000`). Overrides are applied after the merge and recorded in
the output's `protype_source_overrides` key, so the file stays reproducible from
this command alone.
