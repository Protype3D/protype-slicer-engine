#!/usr/bin/env bash
# Build libslic3r as a static archive that slicer-ffi can link against.
#
# Two phases mirror OrcaSlicer's own build_release_macos.sh:
#   1. deps  -> upstream/libslic3r/deps/build/$ARCH/OrcaSlicer_dep/usr/local
#                (Boost, TBB, Eigen, OpenVDB, OpenCASCADE, NLopt, …)
#   2. slicer -> upstream/libslic3r/build/                static libs + GUI
#
# We invoke the slicer build with the headless target so the wxWidgets GUI
# pieces don't pull in a window-server dependency. The -DCMAKE_POLICY_VERSION_MINIMUM=3.5
# flag works around CMake 4.x rejecting OrcaSlicer's older
# `cmake_minimum_required` declarations in vendored dep trees.
#
# Usage:
#   scripts/build_libslic3r.sh           # full build (deps + slicer)
#   scripts/build_libslic3r.sh deps      # deps only
#   scripts/build_libslic3r.sh slicer    # slicer only (deps must already exist)
set -euo pipefail
cd "$(dirname "$0")/.."

ROOT="$(pwd)"
ORCA="$ROOT/upstream/libslic3r"
ARCH="${ARCH:-$(uname -m)}"
DEPS_BUILD="$ORCA/deps/build/$ARCH"
DEPS_PREFIX="$DEPS_BUILD/OrcaSlicer_dep/usr/local"
SLICER_BUILD="$ORCA/build"
NCPU="$(sysctl -n hw.ncpu 2>/dev/null || nproc 2>/dev/null || echo 4)"
PHASE="${1:-all}"

export CMAKE_POLICY_VERSION_MINIMUM=3.5

build_deps() {
    echo ">>> Building libslic3r vendored deps for $ARCH (this is the slow part: 30-90 min on first run)"
    mkdir -p "$DEPS_BUILD/OrcaSlicer_dep"
    cd "$DEPS_BUILD"
    if [[ ! -f CMakeCache.txt ]]; then
        cmake "$ORCA/deps" \
            -G "Unix Makefiles" \
            -DDESTDIR="$DEPS_BUILD/OrcaSlicer_dep" \
            -DOPENSSL_ARCH=darwin64-arm64-cc \
            -DCMAKE_BUILD_TYPE=Release \
            -DCMAKE_OSX_ARCHITECTURES:STRING="$ARCH" \
            -DCMAKE_OSX_DEPLOYMENT_TARGET=11.3 \
            -DCMAKE_POLICY_VERSION_MINIMUM=3.5
    fi
    make -j"$NCPU"
    cd "$ROOT"
    echo ">>> Deps built: $DEPS_PREFIX"
}

build_slicer() {
    if [[ ! -d "$DEPS_PREFIX" ]]; then
        echo "ERROR: deps prefix not found at $DEPS_PREFIX"
        echo "Run: scripts/build_libslic3r.sh deps"
        exit 1
    fi
    echo ">>> Configuring libslic3r build"
    mkdir -p "$SLICER_BUILD"
    cd "$SLICER_BUILD"
    cmake .. \
        -G "Unix Makefiles" \
        -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_PREFIX_PATH="$DEPS_PREFIX" \
        -DCMAKE_INSTALL_PREFIX="$DEPS_PREFIX" \
        -DCMAKE_OSX_ARCHITECTURES:STRING="$ARCH" \
        -DCMAKE_OSX_DEPLOYMENT_TARGET=11.3 \
        -DSLIC3R_GUI=OFF \
        -DSLIC3R_FHS=ON \
        -DSLIC3R_STATIC=ON \
        -DCMAKE_POLICY_VERSION_MINIMUM=3.5
    echo ">>> Building libslic3r target only (skipping GUI binaries)"
    make -j"$NCPU" libslic3r
    cd "$ROOT"
    echo ">>> libslic3r static archive at: $SLICER_BUILD/src/libslic3r/liblibslic3r.a"
}

case "$PHASE" in
    deps)   build_deps ;;
    slicer) build_slicer ;;
    all)    build_deps; build_slicer ;;
    *) echo "Unknown phase: $PHASE (use deps|slicer|all)"; exit 1 ;;
esac
