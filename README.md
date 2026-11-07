# Protype Slicer engine source

This repository holds the source code of the slicing engine behind Protype Slicer:
the `protype-slicer-daemon` program and the Rust crates it is built from. It is
published under the [GNU Affero General Public License, version 3 or any later
version](LICENSE) (AGPL-3.0-or-later), so that everyone who receives Protype
Slicer can get, study, change and rebuild the engine that ships with it.

The engine uses libslic3r from our fork of OrcaSlicer,
[`dinamicby/OrcaSlicer`](https://github.com/dinamicby/OrcaSlicer). The fork is
the Git submodule `upstream/libslic3r`, pinned to one exact commit per release.

## What a tag is

Every release of Protype Slicer has one tag here:

- `slicer-vX.Y.Z` is the source of the engine that shipped in Protype Slicer
  X.Y.Z.
- The tag and its commit are named
  `Protype Slicer X.Y.Z: engine source (daemon <40 hex>, libslic3r <40 hex>)`.
  The same two hashes are in the file [`REVISION`](REVISION): the first is the
  commit of the engine's development repository the tree was taken from, the
  second is the libslic3r commit it is built against.
- `protype-slicer-daemon --version` of that release prints the same two hashes.

History here is a straight line of releases, one commit per release. It does not
contain the development history of the engine.

## Building

[`BUILD.md`](BUILD.md) has the build steps for macOS on Apple Silicon and for
Windows x64, and tells how to check that what you built carries the hashes from
`REVISION`.

## What this tree contains

The tree is the source needed to build, install, run and change the engine that
shipped: every crate the daemon is built from, the build scripts, the patches,
the presets and their generator. The engine's tests, examples and benchmarks
are not part of the shipped program and are not published here.

## Licenses

[`LICENSE`](LICENSE) is the AGPL-3.0 text for the engine. [`NOTICE`](NOTICE) lists
the third-party components the engine is built with and their licenses.

## Trademarks

"Protype" and the Protype logo are trademarks of Protype. The AGPL licenses the
code, not the name: as section 7(e) of the license allows, no rights are granted
to use the Protype name or logo. A modified version you distribute has to carry
a name of its own and must not be presented as Protype Slicer.

## Contributions

This repository is a mirror of released source, written by a script when a
release is made. Pull requests and direct changes are not accepted here. You are
free to fork it under the AGPL.

## Layout

| Path | Content |
|---|---|
| `crates/` | The engine, a Cargo workspace |
| `patches/` | Historical patches for the libslic3r fork, not applied by the release build |
| `profiles/`, `machines/` | Bundled presets and machine descriptions |
| `scripts/` | The scripts that build libslic3r and the daemon, and the generator of the bundled presets |
| `upstream/libslic3r` | The libslic3r submodule (the OrcaSlicer fork) |
| `REVISION` | The two commit hashes this tree stands for |
