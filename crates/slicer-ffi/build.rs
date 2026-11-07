// Build script: compiles the C++ slicer bridge and emits the cxx-generated
// header that the bridge .cpp includes.
//
// With `--features with-libslic3r`, the bridge .cpp gets a `WITH_LIBSLIC3R`
// preprocessor define and we link the static archives produced by
// `scripts/build_libslic3r.sh` (Unix) or `scripts/build_libslic3r_win.ps1`
// (Windows/MSVC). Without the feature, the bridge stays self-contained and
// emits deterministic stub G-code.
//
// Set SLICER_FFI_REQUIRE_REAL=1 (release/publish path) to turn an accidental
// stub build into a hard error instead of a silently-degraded daemon.

use std::env;
use std::path::PathBuf;

fn main() {
    // Release/publish builds set SLICER_FFI_REQUIRE_REAL=1 to guarantee the
    // real libslic3r backend is linked. Without the `with-libslic3r` feature
    // the bridge silently falls back to stub G-code (STL-only, no real
    // slicing) — that stub daemon shipped in 0.1.1–0.1.3 and broke STEP
    // import on auto-update. Fail the build loudly here rather than let a
    // stub daemon get bundled and pushed to the CDN.
    println!("cargo:rerun-if-env-changed=SLICER_FFI_REQUIRE_REAL");
    let require_real = env::var("SLICER_FFI_REQUIRE_REAL")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false);
    if require_real && !cfg!(feature = "with-libslic3r") {
        panic!(
            "SLICER_FFI_REQUIRE_REAL is set but the `with-libslic3r` feature is \
             off — this build would link the FFI stub (STL-only, no real \
             slicing). Rebuild with `--features with-libslic3r` and a prebuilt \
             libslic3r (scripts/build_libslic3r.sh), or unset \
             SLICER_FFI_REQUIRE_REAL for an intentional stub build."
        );
    }

    let mut build = cxx_build::bridge("src/lib.rs");
    build
        .file("cpp/protype_slicer_bridge.cpp")
        // Project root and crate root, both useful for include resolution.
        .include(".")
        .include("..")
        .flag_if_supported("-std=c++17") // GCC / Clang / Apple Clang
        .flag_if_supported("/std:c++17") // MSVC (cl.exe)
        .flag_if_supported("/EHsc") // MSVC: enable C++ exception unwinding (warning C4530)
        .flag_if_supported("/bigobj") // MSVC: libslic3r headers blow past the 64k section limit
        .flag_if_supported("-Wno-unused-parameter");

    if cfg!(feature = "with-libslic3r") {
        let workspace_root = workspace_root();
        let orca = workspace_root.join("upstream/libslic3r");
        let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
        let is_windows = target_os == "windows";

        // Allow CI / packagers to point at pre-built artifacts to skip the
        // 30-90-min source build. Layout under LIBSLIC3R_DIR mirrors the
        // local build: <DIR>/deps_prefix and <DIR>/slicer_build.
        //
        // Default layout differs by OS because the build trees differ:
        //   * Unix  : deps/build/<arch>/OrcaSlicer_dep/usr/local + build/ (.a)
        //   * Windows: deps/build/OrcaSlicer_dep/usr/local       + build/ (.lib,
        //              with per-config Release/ subdirs in the MSVC layout)
        let (deps_prefix, slicer_build) = match env::var("LIBSLIC3R_DIR").ok() {
            Some(dir) => {
                let base = PathBuf::from(dir);
                (base.join("deps_prefix"), base.join("slicer_build"))
            }
            None => {
                let slicer_build = orca.join("build");
                if is_windows {
                    (
                        orca.join("deps/build/OrcaSlicer_dep/usr/local"),
                        slicer_build,
                    )
                } else {
                    let arch = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
                        Ok("aarch64") => "arm64".to_string(),
                        Ok(other) => other.to_string(),
                        Err(_) => "arm64".to_string(),
                    };
                    (
                        orca.join("deps/build")
                            .join(arch)
                            .join("OrcaSlicer_dep/usr/local"),
                        slicer_build,
                    )
                }
            }
        };

        // The libslic3r static archive: MSVC emits `libslic3r.lib` under a
        // per-config `Release/` dir; Unix emits `liblibslic3r.a`.
        let libslic3r_archive = if is_windows {
            slicer_build.join("src/libslic3r/Release/libslic3r.lib")
        } else {
            slicer_build.join("src/libslic3r/liblibslic3r.a")
        };

        // Sanity-check artifacts the helper script produces. If they're
        // missing we fail fast with an actionable message rather than
        // letting the linker dump a 200-line "undefined symbol" wall.
        if !libslic3r_archive.exists() {
            let helper = if is_windows {
                "pwsh scripts/build_libslic3r_win.ps1"
            } else {
                "scripts/build_libslic3r.sh"
            };
            panic!(
                "with-libslic3r enabled but {} not found.\n  \
                 Run: {}\n  \
                 (deps phase is 30-90 min on first run; slicer phase is ~15 min.)",
                libslic3r_archive.display(),
                helper
            );
        }
        if !deps_prefix.join("include").exists() {
            panic!(
                "with-libslic3r enabled but deps prefix include dir missing: {}\n  \
                 Run the deps phase of the libslic3r build helper.",
                deps_prefix.join("include").display()
            );
        }

        // Includes and defines mirror what OrcaSlicer's CMake feeds to its
        // own libslic3r .o files (see the generated libslic3r flags /
        // libslic3r.vcxproj). We pull the same set so any libslic3r header we
        // include from bridge.cpp resolves and instantiates identically.
        let deps_src = orca.join("deps_src");
        // Shared project-tree + vendored-source includes (same on every OS).
        for inc in &[
            // libslic3r headers + cmake-generated headers
            orca.join("src/libslic3r"),
            slicer_build.join("src/libslic3r"),
            slicer_build.join("src"),
            slicer_build.join("src/dev-utils/platform"),
            // libslic3r's vendored / project-tree headers
            orca.join("src"),
            orca.join("src/eigen"),
            orca.join("src/libigl"),
            // deps_src vendored sources. The parent dir itself is also on
            // the include path so vendored headers can reference siblings
            // as e.g. `#include "semver/semver.h"`.
            deps_src.clone(),
            deps_src.join("admesh"),
            deps_src.join("eigen"),
            deps_src.join("libigl"),
            deps_src.join("libnest2d/include"),
            deps_src.join("clipper"),
            deps_src.join("glu-libtess/include"),
            deps_src.join("semver"),
            deps_src.join("miniz"),
            deps_src.join("nanosvg"),
            // deps prefix root (Boost, TBB, OpenVDB, ... headers)
            deps_prefix.join("include"),
        ] {
            build.include(inc);
        }
        // deps-prefix include layout differs between the MSVC and Unix deps
        // trees: Boost lands in a versioned `boost-1_84/` dir and OCCT headers
        // in `occt/` on Windows, vs `eigen3/` + `opencascade/` on macOS.
        if is_windows {
            for inc in &[
                deps_prefix.join("include/boost-1_84"),
                deps_prefix.join("include/occt"),
                deps_prefix.join("include/OpenEXR"),
            ] {
                build.include(inc);
            }
        } else {
            for inc in &[
                deps_prefix.join("include/eigen3"),
                deps_prefix.join("include/opencascade"),
                deps_prefix.join("include/OpenEXR"),
            ] {
                build.include(inc);
            }
        }

        // ABI defines: MUST match what libslic3r itself was built with, or
        // template instantiations diverge silently. The two sets are extracted
        // from the respective libslic3r build flags (macOS:
        // build/src/libslic3r/CMakeFiles/libslic3r.dir/flags.make; Windows:
        // build/src/libslic3r/libslic3r.vcxproj Release|x64 defines).
        if is_windows {
            for def in &[
                "WITH_LIBSLIC3R",
                // Boost: auto-link is disabled (BOOST_ALL_NO_LIB), so we link
                // the decorated libs explicitly below.
                "BOOST_ALL_NO_LIB",
                "BOOST_ATOMIC_NO_LIB",
                "BOOST_ATOMIC_STATIC_LINK",
                "BOOST_CHRONO_NO_LIB",
                "BOOST_CHRONO_STATIC_LINK",
                "BOOST_CONTAINER_NO_LIB",
                "BOOST_CONTAINER_STATIC_LINK",
                "BOOST_DATE_TIME_NO_LIB",
                "BOOST_DATE_TIME_STATIC_LINK",
                "BOOST_FILESYSTEM_NO_LIB",
                "BOOST_FILESYSTEM_STATIC_LINK=1",
                "BOOST_IOSTREAMS_NO_LIB",
                "BOOST_IOSTREAMS_STATIC_LINK",
                "BOOST_LOCALE_NO_LIB",
                "BOOST_LOCALE_STATIC_LINK",
                "BOOST_LOG_NO_LIB",
                "BOOST_LOG_STATIC_LINK",
                "BOOST_NOWIDE_NO_LIB",
                "BOOST_PROGRAM_OPTIONS_NO_LIB",
                "BOOST_PROGRAM_OPTIONS_STATIC_LINK",
                "BOOST_RANDOM_NO_LIB",
                "BOOST_RANDOM_STATIC_LINK",
                "BOOST_SERIALIZATION_NO_LIB",
                "BOOST_SERIALIZATION_STATIC_LINK",
                // Windows-specific Boost knobs (IOCP, not kqueue; UTF-8 paths).
                "BOOST_SYSTEM_USE_UTF8",
                "BOOST_THREAD_NO_LIB",
                "BOOST_THREAD_STATIC_LINK",
                "BOOST_THREAD_USE_LIB",
                "BOOST_USE_WINAPI_VERSION=0x602",
                // Netfabb STL-fixing service is compiled in on Windows.
                "HAS_WIN10SDK",
                "HAVE_FREETYPE",
                "LIBNEST2D_GEOMETRIES_libslic3r",
                "LIBNEST2D_OPTIMIZER_nlopt",
                "LIBNEST2D_STATIC",
                "LIBNEST2D_THREADING_tbb",
                "OPENVDB_ABI_VERSION_NUMBER=8",
                "OPENVDB_OPENEXR_STATICLIB",
                "OPENVDB_STATICLIB",
                "TBB_USE_CAPTURED_EXCEPTION=0",
                "USE_TBB",
                "UNICODE",
                "_UNICODE",
                "_USE_MATH_DEFINES",
                "_SILENCE_CXX17_ADAPTOR_TYPEDEFS_DEPRECATION_WARNING",
                "_SCL_SECURE_NO_WARNINGS",
                "_CRT_SECURE_NO_WARNINGS",
                "_CRT_NONSTDC_NO_DEPRECATE",
            ] {
                if let Some((k, v)) = def.split_once('=') {
                    build.define(k, v);
                } else {
                    build.define(def, None);
                }
            }
        } else {
            for def in &[
                "WITH_LIBSLIC3R",
                "BOOST_ASIO_DISABLE_KQUEUE",
                "BOOST_ATOMIC_NO_LIB",
                "BOOST_ATOMIC_STATIC_LINK",
                "BOOST_CHRONO_NO_LIB",
                "BOOST_CHRONO_STATIC_LINK",
                "BOOST_CONTAINER_NO_LIB",
                "BOOST_CONTAINER_STATIC_LINK",
                "BOOST_DATE_TIME_NO_LIB",
                "BOOST_DATE_TIME_STATIC_LINK",
                "BOOST_FILESYSTEM_NO_LIB",
                "BOOST_IOSTREAMS_NO_LIB",
                "BOOST_IOSTREAMS_STATIC_LINK",
                "BOOST_LOCALE_NO_LIB",
                "BOOST_LOCALE_STATIC_LINK",
                "BOOST_LOG_NO_LIB",
                "BOOST_LOG_STATIC_LINK",
                "BOOST_NOWIDE_NO_LIB",
                "BOOST_NO_CXX98_FUNCTION_BASE",
                "BOOST_PROGRAM_OPTIONS_NO_LIB",
                "BOOST_PROGRAM_OPTIONS_STATIC_LINK",
                "BOOST_RANDOM_NO_LIB",
                "BOOST_RANDOM_STATIC_LINK",
                "BOOST_SERIALIZATION_NO_LIB",
                "BOOST_SERIALIZATION_STATIC_LINK",
                "BOOST_THREAD_NO_LIB",
                "BOOST_THREAD_STATIC_LINK",
                "BOOST_THREAD_USE_LIB",
                "LIBNEST2D_GEOMETRIES_libslic3r",
                "LIBNEST2D_OPTIMIZER_nlopt",
                "LIBNEST2D_STATIC",
                "LIBNEST2D_THREADING_tbb",
                "OCC_CONVERT_SIGNALS",
                "OPENVDB_OPENEXR_STATICLIB",
                "OPENVDB_STATICLIB",
                "TBB_USE_CAPTURED_EXCEPTION=0",
                "USE_TBB",
                "_HAS_AUTO_PTR_ETC=0",
            ] {
                if let Some((k, v)) = def.split_once('=') {
                    build.define(k, v);
                } else {
                    build.define(def, None);
                }
            }
            build.define("BOOST_FILESYSTEM_STATIC_LINK", "1");
            build.define("OPENVDB_ABI_VERSION_NUMBER", "8");
        }

        if is_windows {
            link_windows(&deps_prefix, &slicer_build);
        } else {
            link_unix(&deps_prefix, &slicer_build);
        }
    }

    build.compile("protype_slicer_bridge");

    println!("cargo:rerun-if-changed=src/lib.rs");
    println!("cargo:rerun-if-changed=cpp/protype_slicer_bridge.cpp");
    println!("cargo:rerun-if-changed=cpp/protype_slicer_bridge.h");
    println!("cargo:rerun-if-env-changed=LIBSLIC3R_DIR");
}

/// OpenCASCADE static archives — STEP/IGES/STL importers reference these. The
/// full set is linked because the TK libs cross-reference each other and the
/// linker resolves the cycle by having all of them on the line.
const OCCT_TK_LIBS: &[&str] = &[
    "TKBO",
    "TKBRep",
    "TKBin",
    "TKBinL",
    "TKBinXCAF",
    "TKBool",
    "TKCAF",
    "TKCDF",
    // Fillets, chamfers (TKFillet) and shelling/lofting (TKOffset) are not
    // needed to read STEP, so the deps build leaves them out unless
    // `BUILD_ADDITIONAL_TOOLKITS` names them. Listing them here without
    // rebuilding the deps prefix is a link error, not a silent no-op.
    "TKFillet",
    "TKOffset",
    "TKG2d",
    "TKG3d",
    "TKGeomAlgo",
    "TKGeomBase",
    "TKHLR",
    "TKIGES",
    "TKLCAF",
    "TKMath",
    "TKMesh",
    "TKPrim",
    "TKRWMesh",
    "TKSTEP",
    "TKSTEP209",
    "TKSTEPAttr",
    "TKSTEPBase",
    "TKSTL",
    "TKService",
    "TKShHealing",
    "TKTopAlgo",
    "TKV3d",
    "TKVCAF",
    "TKVRML",
    "TKXCAF",
    "TKXDEIGES",
    "TKXDESTEP",
    "TKXSBase",
    "TKXml",
    "TKXmlL",
    "TKXmlXCAF",
    "TKernel",
];

/// Boost components linked explicitly (auto-link is disabled by BOOST_ALL_NO_LIB).
/// `system` and `regex` are header-only in Boost 1.84, so they're omitted.
const BOOST_COMPONENTS: &[&str] = &[
    "filesystem",
    "thread",
    "log",
    "log_setup",
    "locale",
    "chrono",
    "atomic",
    "date_time",
    "iostreams",
    "program_options",
    "nowide",
];

fn link_windows(deps_prefix: &std::path::Path, slicer_build: &std::path::Path) {
    // Static-archive search paths. The MSVC build tree puts each target's lib
    // under a per-config `Release/` subdir; OCCT installs to `lib/occt`.
    for dir in &[
        slicer_build.join("src/libslic3r/Release"),
        slicer_build.join("deps_src/clipper/Release"),
        slicer_build.join("deps_src/admesh/Release"),
        slicer_build.join("deps_src/miniz/Release"),
        slicer_build.join("deps_src/glu-libtess/Release"),
        slicer_build.join("deps_src/qoi/Release"),
        slicer_build.join("deps_src/mcut/Release"),
        slicer_build.join("lib/Release"),
        deps_prefix.join("lib"),
        deps_prefix.join("lib/occt"),
        deps_prefix.join("x64/vc17/staticlib"),
    ] {
        println!("cargo:rustc-link-search=native={}", dir.display());
    }

    // Static archives from the libslic3r build tree.
    for lib in &[
        "libslic3r",
        "libslic3r_cgal",
        "clipper",
        "admesh",
        "miniz_static",
        "glu-libtess",
        "qoi",
        "semver",
        "mcut",
    ] {
        println!("cargo:rustc-link-lib=static={lib}");
    }
    // Static archives from the deps prefix (MSVC-decorated names).
    for lib in &[
        "qhullcpp",
        "qhullstatic_r",
        "libnoise_static",
        "jpeg-static",
        "libopenvdb",
        "libblosc",
        "nlopt",
        // tbb.lib and tbb12.lib are byte-identical (the oneTBB 12-ABI lib with
        // a compat alias); link one. (tbbmalloc is whole-archived below.)
        "tbb",
        "freetype",
        "expat",
        "zlib",
        "libpng16_static",
        "libgmp-10",
        "libmpfr-4",
        // OpenSSL libcrypto: libslic3r's utils.cpp / bbs_3mf.cpp call MD5_* for
        // 3MF hashing. (libslic3r itself isn't linked against OpenSSL on Windows
        // by OrcaSlicer's CMake, because the GUI pulls it transitively via CURL.)
        "libcrypto",
        // OpenVDB pulls the Imath/OpenEXR halves for half-float + EXR I/O.
        "Half-2_5",
        "Iex-2_5",
        "IexMath-2_5",
        "IlmImf-2_5",
        "IlmImfUtil-2_5",
        "IlmThread-2_5",
        "Imath-2_5",
    ] {
        println!("cargo:rustc-link-lib=static={lib}");
    }
    // TBB scalable allocator (scalable_malloc/scalable_free). libslic3r
    // references these from many TUs, but on-demand library scanning doesn't
    // pull tbbmalloc's members reliably once they're bundled into the rlib;
    // whole-archive forces every tbbmalloc object into the final link.
    println!("cargo:rustc-link-lib=static:+whole-archive=tbbmalloc");
    // Boost: decorated MSVC names (toolset vc144, multithreaded, x64, 1.84).
    for comp in BOOST_COMPONENTS {
        println!("cargo:rustc-link-lib=static=libboost_{comp}-vc144-mt-x64-1_84");
    }
    for tk in OCCT_TK_LIBS {
        println!("cargo:rustc-link-lib=static={tk}");
    }
    // Windows system import libs. libslic3r links Psapi explicitly; the rest
    // satisfy Boost (filesystem/log), OCCT, and the Win10 SDK Netfabb path.
    // Add more here as the linker surfaces unresolved externals.
    for sys in &[
        "user32", "gdi32", "shell32", "ole32", "oleaut32", "advapi32", "ws2_32", "crypt32",
        "bcrypt", "dbghelp", "psapi", "comdlg32", "winmm", "iphlpapi", "rpcrt4", "shlwapi",
        "version", "wbemuuid",
    ] {
        println!("cargo:rustc-link-lib=dylib={sys}");
    }
}

fn link_unix(deps_prefix: &std::path::Path, slicer_build: &std::path::Path) {
    // Static-archive search paths. Order matters for the final link:
    // libslic3r depends on the vendored deps in OrcaSlicer's two prefixes
    // (deps phase install + slicer phase build tree).
    for dir in &[
        slicer_build.join("src/libslic3r"),
        slicer_build.join("deps_src/clipper"),
        slicer_build.join("deps_src/admesh"),
        slicer_build.join("deps_src/miniz"),
        slicer_build.join("deps_src/glu-libtess"),
        slicer_build.join("deps_src/qoi"),
        // mcut's static archive is built under the slicer build tree's
        // deps_src, next to clipper/admesh/miniz/qoi (not in deps_prefix).
        slicer_build.join("deps_src/mcut"),
        slicer_build.join("lib"),
        deps_prefix.join("lib"),
    ] {
        println!("cargo:rustc-link-search=native={}", dir.display());
    }

    // Start with the link-set required to satisfy the current bridge.cpp call
    // surface (Model::read_from_file pulls in admesh, OpenCASCADE for STEP, and
    // 3MF/AMF importers via expat+miniz). New libslic3r calls surface as
    // missing symbols and we extend below.
    for lib in &[
        "libslic3r",
        // MeshBoolean.cpp compiles into libslic3r_cgal and carries BOTH
        // cgal:: and mcut:: symbols in a single .o, so pulling the mcut
        // boolean surface drags CGAL in too — link the whole stack.
        "libslic3r_cgal",
        "clipper",
        "admesh",
        "miniz_static",
        "qhullcpp",
        "qhullstatic_r",
        "semver",
        // FuzzySkin uses libnoise for procedural surface bumps;
        // GCodeThumbnails uses libjpeg for inlined JPG previews.
        "libnoise_static",
        "jpeg",
        // OpenVDB + NLopt land here once Print::process exercises
        // the volumetric / arrange code paths.
        "openvdb",
        "nlopt",
        // Tessellation helper used by Print::Tesselate.
        "glu-libtess",
        // QOI image format for embedded G-code thumbnails.
        "qoi",
        // Phase-2 boolean backend. mcut is the Bambu/BBS mesh-boolean
        // engine behind MeshBoolean::mcut::make_boolean; gmp/mpfr/gmpxx
        // satisfy the CGAL exact-arithmetic symbols the same MeshBoolean
        // .o references. Order is irrelevant here — Apple ld revisits
        // static archives to resolve cross-references.
        "mcut",
        "gmp",
        "mpfr",
        "gmpxx",
    ] {
        println!("cargo:rustc-link-lib=static={lib}");
    }
    // OpenCASCADE static archives — STEP/IGES/STL importers reference these.
    for tk in OCCT_TK_LIBS {
        println!("cargo:rustc-link-lib=static={tk}");
    }
    // Boost link list mirrors OrcaSlicer's find_package(Boost COMPONENTS ...).
    // boost_system and boost_regex are header-only in Boost 1.84.
    for lib in &[
        "tbb",
        "tbbmalloc",
        "boost_filesystem",
        "boost_thread",
        "boost_log",
        "boost_log_setup",
        "boost_locale",
        "boost_chrono",
        "boost_atomic",
        "boost_date_time",
        "boost_iostreams",
        "boost_program_options",
        "boost_nowide",
        // OpenCASCADE's TKService uses FreeType for font helpers.
        "freetype",
        // expat satisfies XML symbols used by 3MF / AMF importers.
        "expat",
        "z",
    ] {
        println!("cargo:rustc-link-lib={lib}");
    }
    if cfg!(target_os = "macos") {
        // Foundation + ModelIO satisfy the Objective-C symbols libslic3r
        // references from ModelIO.mm (NSTemporaryDirectory, NSFileManager,
        // MDLAsset, …) used by the .obj/.usd loader.
        for fw in &[
            "CoreFoundation",
            "Foundation",
            "ModelIO",
            "IOKit",
            "Security",
        ] {
            println!("cargo:rustc-link-lib=framework={fw}");
        }
    }
}

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR points at slicer-ffi/. Workspace root is two up.
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .expect("workspace root not found from slicer-ffi/")
}
