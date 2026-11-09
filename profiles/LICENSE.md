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
upstream). Redistributing the flat OrcaSlicer presets above (the three `profiles/*.json`
files) unmodified under the same license is compatible — no relicensing is involved.

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

## `catalog/`

Catalogs under `catalog/<vendor>/` are written by `scripts/profiles/generate.py`;
they are not edited by hand.

- Source: OrcaSlicer tag `v2.5.0`, commit `40ecc66f78ed72dc368e0c1ebe40d77f00cc8a19`
  (`resources/profiles/Qidi/` and `resources/profiles/Qidi.json`), licensed
  `AGPL-3.0-or-later` (same terms as above).
- The source of every preset is recorded in its `content.origin`
  (repo, tag, commit, path, license, generator); the full chain of source files
  and the bed assets are listed in `sources.files` of each package.
- **These presets are a modified version of the OrcaSlicer profiles** (AGPL-3.0
  §5(a) notice). They were modified on 2026-11-07 by `scripts/profiles/generate.py`
  from the OrcaSlicer repository <https://github.com/OrcaSlicer/OrcaSlicer>. The
  modifications are:
  - `inherits` chains are flattened into one preset;
  - keys and values are converted to the Protype settings schema v2; keys the
    schema cannot hold are dropped (listed with counts in
    `catalog/<vendor>/report.json`); values libslic3r itself rewrites on load
    are written as it reads them (the pattern name `zig-zag` as `rectilinear`,
    comma-joined `bed_exclude_area` and `thumbnails` entries one per item);
  - machine limits are capped to the manufacturer's `printer.cfg` where it is
    published;
  - QIDI rules: tool-change template guard `{if current_extruder != next_extruder}`,
    the lift before a Box tool change clamped to `max_print_height` (text from
    QIDI Studio <https://github.com/QIDITECH/QIDIStudio>, AGPL-3.0, commit
    `21d47457919212b8af3f97705716c261b97be559`), chamber limits, Q2/Q2C nozzle
    type, exhaust fan off for ABS/ASA on X-Max 4;
  - the template variable `timelapse_type` is replaced by `0` for the engine;
  - stable preset ids and names are assigned.

  `report.json` (`rules`, `limits`, `dropped_keys`, `rejected`) records the
  changes per preset.
- **Exception: the X-Max 4 kit presets keep Protype's own values.** Machine
  `c425afad-…`, extruder `698c21b4-…`, process `607a0051-…` and filament
  `691567e5-…` are Protype's hand tuning on top of the QIDI and OrcaSlicer
  profiles (first exported from QIDI Studio <https://github.com/QIDITECH/QIDIStudio>),
  under the same AGPL-3.0-or-later terms. Their source is the file
  `scripts/profiles/kits/qidi.json` of this repository, which the public engine
  mirror <https://github.com/dinamicby/protype-slicer-engine> publishes: it
  holds the four presets in full, so no other repository is needed to read or
  rebuild them. Their `content.origin` names that file and the generator commit
  that holds it (the mirror's `REVISION` file maps it), and `report.json`
  `hand_kits` lists them with the file's git blob id. The values were copied
  verbatim from Protype Slicer `apps/agent/resources/starter_profiles/presets.json`
  at commit `51127886f4b2285f1e7701a0ff5c87ec69fa341d`. They replace
  OrcaSlicer's "Qidi X-Max 4 0.4 nozzle" machine and extruder, and are added
  beside the OrcaSlicer processes and filaments. They stay as they are until
  they are checked against the hand profile on a real part (decision of
  2026-11-07). The machine alone gets seven display-only keys that are not in
  `presets.json`, listed under `additions` in the same file: `thumbnails` from
  the OrcaSlicer "Qidi X-Max 4 0.4 nozzle" machine it replaces, and the bed
  model and overlay of the OrcaSlicer X-Max 4 with the six `bed_visual_*` keys
  that show them (2026-11-08).
- Rebuilding. Requirements: Python >= 3.11 (standard library only) and git (the
  generator fetches the pinned OrcaSlicer tag with a sparse git checkout on first
  run). The two inputs, `settings_schema.json` and `libslic3r_keys.json`, belong
  to Protype Slicer, a private repository, and are not published. The committed
  JSON catalog is itself the editable form of these presets. A byte-for-byte
  rebuild needs those two snapshots; their sha256 are in `report.json` `inputs`.
  Protype maintainers extract them from the Slicer commit whose files match
  those sha256, with
  `git -C <Slicer> show <commit>:apps/agent/resources/settings_schema.json > settings_schema.json`
  (same for `libslic3r_keys.json`), and run:

```bash
python3 scripts/profiles/generate.py --vendor qidi --orca-tag v2.5.0 \
  --schema settings_schema.json --libslic3r-keys libslic3r_keys.json \
  --out profiles/catalog/qidi
```
