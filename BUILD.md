# Building the engine

These are the steps that the release builds of Protype Slicer run to produce
`protype-slicer-daemon`, the engine binary: one for macOS on Apple Silicon
(arm64) and one for Windows x64. They are the daemon steps of Protype Slicer's
macOS and Windows release workflows. No other platform has build scripts here.

Plan on several GB of disk space. Building libslic3r and its dependencies takes
most of the time: the dependencies alone take 30 to 90 minutes on macOS, and
hours on Windows from a cold start.

## 1. Get the source of a release

Every release is a tag `slicer-vX.Y.Z`. Clone it together with its submodule,
the libslic3r fork `upstream/libslic3r`, which `.gitmodules` points at the
public repository `https://github.com/dinamicby/OrcaSlicer.git`:

```sh
git clone --branch slicer-vX.Y.Z --recurse-submodules \
  https://github.com/dinamicby/protype-slicer-engine.git
cd protype-slicer-engine
```

`REVISION` names the two commits this tree stands for. Check that the submodule
is on the second one (the command prints the same hash as the `libslic3r=` line):

```sh
cat REVISION
git -C upstream/libslic3r rev-parse HEAD
```

## 2. No patches are applied

The release build uses the unmodified `upstream/libslic3r` checkout at the
pinned commit. `patches/` and `scripts/apply-patches.sh` are historical: they
stay in the tree, but the release build does not apply them, and neither should
you (`patches/README.md` describes an older workflow). The park-and-wait work of
`patches/0004-park-and-wait.patch` is already part of the pinned fork; the only
other patch, `0001`, merely silences one log line printed at start-up.

## 3a. macOS, Apple Silicon (arm64)

You need:

- an Apple Silicon Mac with the Xcode command line tools (`xcode-select --install`),
- CMake (for CMake 4.x the script sets `CMAKE_POLICY_VERSION_MINIMUM=3.5`),
- `git`, `bash`, and Rust installed with [rustup](https://rustup.rs). The
  repository's `rust-toolchain.toml` selects the stable toolchain.

`scripts/build_libslic3r.sh` works on macOS only, and on Apple Silicon only (it
passes `OPENSSL_ARCH=darwin64-arm64-cc`).

```sh
bash scripts/build_libslic3r.sh deps      # libslic3r's dependencies: the slow part
bash scripts/build_libslic3r.sh slicer    # libslic3r itself
SLICER_FFI_REQUIRE_REAL=1 cargo build --release -p slicer-daemon --features with-libslic3r
```

`SLICER_FFI_REQUIRE_REAL=1` makes the build fail, instead of silently producing
a stub daemon that does not really slice, if the `with-libslic3r` feature is off.
The binary is `target/release/protype-slicer-daemon`.

## 3b. Windows x64

You need:

- Visual Studio 2022 (version 17.x) with the "Desktop development with C++"
  workload,
- CMake **3.31.x** (the release workflow uses 3.31.12). OrcaSlicer's build
  refuses CMake 4.0 and newer on Windows, and the CMake that comes with Visual
  Studio may be too new, so put a 3.31.x ahead of it on `PATH`,
- [Strawberry Perl](https://strawberryperl.com) in `C:\Strawberry` (the OpenSSL
  dependency does not build with the Perl that comes with Git),
- Git for Windows (to clone the repository),
- Rust installed with [rustup](https://rustup.rs), with the
  `x86_64-pc-windows-msvc` target.

From the repository root, in PowerShell:

```powershell
.\scripts\build_libslic3r_win.ps1         # dependencies, then libslic3r
$env:SLICER_FFI_REQUIRE_REAL = '1'        # stops a stub build, changes nothing else
.\scripts\cargo_msvc.ps1 -CargoLine "build --release -p slicer-daemon --features with-libslic3r"
```

(The release workflow for Windows does not set `SLICER_FFI_REQUIRE_REAL`; it
only makes the build fail when the `with-libslic3r` feature is off.) The binary is `target\release\protype-slicer-daemon.exe`.

On Windows OpenCASCADE is built as DLLs, so the daemon needs them to start. Put
them on `PATH` before running it:

```powershell
$bin = "$PWD\upstream\libslic3r\deps\build\OrcaSlicer_dep\usr\local\bin"
$env:PATH = "$bin\occt;$bin;$env:PATH"
```

(Or copy `TK*.dll` from `$bin\occt` and `freetype.dll` from `$bin` next to the
exe, which is what the installer build does.)

## 4. Check that you built this revision

The daemon reports the source it was built from:

```sh
target/release/protype-slicer-daemon --version
```

```
protype-slicer-daemon <crate version> (daemon <40 hex>, libslic3r <40 hex>)
```

The program may print one more line before this one, for example
`[2026-10-21 17:49:19.423244] [0x00000001fa01dd80] [trace]   Initializing StaticPrintConfigs`:
libslic3r logs it to standard output while the program starts, before `--version`
is handled. Ignore it. The version line is the one that begins with
`protype-slicer-daemon`, and exactly one line has that shape.

The two hashes must be exactly the ones in `REVISION`, and the version line must
not end in `, dirty`. A tree with a valid `REVISION` reports it whatever its own Git
history says, so this check shows that the build started from this release's
`REVISION`; it cannot notice files you changed after cloning.

```sh
daemon=$(sed -n 's/^daemon=//p' REVISION)
libslic3r=$(sed -n 's/^libslic3r=//p' REVISION)
target/release/protype-slicer-daemon --version | grep -F "(daemon $daemon, libslic3r $libslic3r)" && echo "revision matches"
```

On Windows, compare the output of `.\target\release\protype-slicer-daemon.exe --version`
with the two hashes in `REVISION` by eye, or with `Select-String -SimpleMatch`.
