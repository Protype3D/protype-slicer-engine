# Build libslic3r as a static lib (libslic3r.lib) that slicer-ffi can link
# against on Windows / MSVC. This is the VS2022 (v143) counterpart to the
# Unix scripts/build_libslic3r.sh.
#
# Two phases mirror OrcaSlicer's own build_release_vs2022.bat:
#   1. deps   -> upstream/libslic3r/deps/build/OrcaSlicer_dep/usr/local
#                (Boost, TBB, OpenVDB, OpenCASCADE, NLopt, qhull, libnoise, ...)
#   2. slicer -> upstream/libslic3r/build/  (the libslic3r static lib + the
#                vendored static libs it pulls: clipper, admesh, miniz, ...)
#
# We build ONLY the `libslic3r` CMake target with SLIC3R_GUI=OFF, so the
# wxWidgets GUI is never compiled/linked. CMAKE_POLICY_VERSION_MINIMUM=3.5
# works around CMake 4.x rejecting the older cmake_minimum_required() in
# OrcaSlicer's vendored dep trees.
#
# Toolchain it needs on the machine:
#   * Visual Studio 2022 with the "Desktop development with C++" workload
#     (cl.exe + Windows SDK). Located via vswhere.
#   * CMake >= 3.22 (a standalone install; the VS-bundled CMake works too).
#   * Strawberry Perl (native Windows perl) for the OpenSSL dep's Configure;
#     MSYS/Git perl tends to fail the VC-WIN64A build.
#
# Usage (from the daemon repo root, any shell):
#   pwsh scripts/build_libslic3r_win.ps1            # full build (deps + slicer)
#   pwsh scripts/build_libslic3r_win.ps1 -Phase deps
#   pwsh scripts/build_libslic3r_win.ps1 -Phase slicer
param(
    [ValidateSet('deps', 'slicer', 'all')]
    [string]$Phase = 'all'
)
$ErrorActionPreference = 'Stop'

# cmake (and the tools it drives) write progress/warnings to stderr. Under
# $ErrorActionPreference='Stop', PowerShell 5.1 wraps any native-command stderr
# line in a terminating NativeCommandError even when the exe exits 0. Route
# native calls through this helper: it neutralizes that behavior and judges
# success solely by the process exit code.
function Invoke-Native {
    param([Parameter(Mandatory)][string]$What, [Parameter(Mandatory)][string[]]$Args)
    $exe, $rest = $Args[0], $Args[1..($Args.Count - 1)]
    # Native tools (cmake, MSBuild, cl) write progress/warnings to stderr.
    # Under ErrorActionPreference='Stop', PowerShell wraps any such line in a
    # terminating NativeCommandError even on exit 0. Flip to Continue around
    # the call and judge success solely by the process exit code. Output flows
    # straight to the (redirected) console; no in-process pipe to deadlock.
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try { & $exe @rest } finally { $ErrorActionPreference = $prev }
    if ($LASTEXITCODE -ne 0) { throw "$What failed (exit $LASTEXITCODE)" }
}

$Root        = (Resolve-Path "$PSScriptRoot\..").Path
$Orca        = Join-Path $Root 'upstream\libslic3r'
$DepsBuild   = Join-Path $Orca 'deps\build'
$DepsDestdir = Join-Path $DepsBuild 'OrcaSlicer_dep'
$DepsPrefix  = Join-Path $DepsDestdir 'usr\local'
$SlicerBuild = Join-Path $Orca 'build'

# CMake 4.x rejects the old `cmake_minimum_required` in vendored deps.
$env:CMAKE_POLICY_VERSION_MINIMUM = '3.5'

# ---- Locate and import the MSVC build environment (cl, link, lib, nmake) ----
function Import-VcEnv {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path $vswhere)) { throw "vswhere.exe not found at $vswhere" }
    # Constrained to the 17.x (2022) series on purpose: the CMake invocations
    # below hardcode the "Visual Studio 17 2022" generator, and that is the
    # newest generator CMake 3.31 knows — which is itself the newest CMake
    # OrcaSlicer accepts on Windows. A bare `-latest` happily returns Visual
    # Studio 18 on a machine that has it, and the mismatch surfaces as the
    # thoroughly unhelpful
    #     Generator Visual Studio 17 2022 could not find any instance of
    #     Visual Studio.
    # even though VS is plainly installed. Asking for 17.x keeps the toolchain
    # and the generator string in agreement, and fails with a clear message.
    $vsPath = & $vswhere -products * -version '[17.0,18.0)' `
        -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
        -property installationPath | Select-Object -First 1
    if (-not $vsPath) {
        throw ("No Visual Studio 2022 (17.x) install with the C++ toolset " +
               "(VC.Tools.x86.x64) was found. libslic3r must be built with VS 2022: " +
               "OrcaSlicer caps CMake at 3.31.x on Windows, and that CMake has no " +
               "generator for Visual Studio 18. Install the VS 2022 'Desktop " +
               "development with C++' workload (a newer VS can stay installed " +
               "alongside).")
    }
    $vcvars = Join-Path $vsPath 'VC\Auxiliary\Build\vcvars64.bat'
    if (-not (Test-Path $vcvars)) { throw "vcvars64.bat not found at $vcvars" }
    Write-Host "==> Importing MSVC environment from $vcvars"
    # Run vcvars in a child cmd and capture the resulting environment.
    cmd /c "`"$vcvars`" >nul 2>&1 && set" | ForEach-Object {
        if ($_ -match '^([^=]+)=(.*)$') {
            Set-Item -Path "Env:\$($matches[1])" -Value $matches[2]
        }
    }
    # Pin CMake to the VS-bundled copy. The deps build tree gets configured by
    # whichever cmake runs first; a *standalone* CMake 4.x re-processing a tree
    # that an older (VS-bundled 3.x) cmake generated dies with
    # "No preprocessor test for 'Renesas'" during compiler-id detection. Putting
    # the VS-bundled cmake at the front of PATH makes every phase use one
    # consistent version, regardless of any standalone CMake on the system PATH.
    #
    # ...but ONLY while that copy is < 4.0. VS2022 now ships CMake 4.3.1-msvc1,
    # and OrcaSlicer's top-level CMakeLists hard-fails on Windows at >= 4.0
    # ("Only cmake versions between 3.13.x and 3.31.x is supported on
    # windows"). Blindly pinning the VS copy therefore turned this guard from
    # "guarantee a 3.x" into "guarantee a 4.x": the deps build (~2h) would
    # succeed and the slicer configure would die instantly right after. When
    # the bundled copy is too new we leave PATH alone so an explicitly
    # installed 3.31.x (CI pins one; devs can use chocolatey/winget) wins.
    $vsCmakeBin = Join-Path $vsPath 'Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin'
    $vsCmakeExe = Join-Path $vsCmakeBin 'cmake.exe'
    if (Test-Path $vsCmakeExe) {
        $vsCmakeVersion = Get-CMakeVersion $vsCmakeExe
        if ($vsCmakeVersion -and $vsCmakeVersion.Major -ge 4) {
            Write-Warning ("VS-bundled CMake is $vsCmakeVersion (>= 4.0); OrcaSlicer rejects it on Windows. " +
                           "Leaving PATH alone so a 3.31.x install can be used instead.")
        } else {
            $env:PATH = "$vsCmakeBin;" + $env:PATH
            Write-Host "==> Pinned CMake to VS-bundled $vsCmakeBin ($vsCmakeVersion)"
        }
    }
}

# Parse `cmake --version` into a [version]. Returns $null if it can't be read
# (caller then falls back to its previous behaviour rather than hard-failing).
function Get-CMakeVersion {
    param([Parameter(Mandatory)][string]$CMakeExe)
    $out = & $CMakeExe --version 2>$null | Select-Object -First 1
    # "cmake version 4.3.1-msvc1" -> 4.3.1 (the -msvc suffix is not a [version]).
    if ($out -match 'cmake version (\d+)\.(\d+)\.(\d+)') {
        return [version]"$($matches[1]).$($matches[2]).$($matches[3])"
    }
    Write-Warning "Could not parse CMake version from: $out"
    return $null
}

# Fail early and loudly rather than after a multi-hour deps build.
function Assert-CMakeSupported {
    $cmake = Get-Command cmake -ErrorAction SilentlyContinue
    if (-not $cmake) { throw "cmake not found on PATH." }
    $v = Get-CMakeVersion $cmake.Source
    if ($v -and $v.Major -ge 4) {
        throw ("CMake $v at $($cmake.Source) is unsupported: OrcaSlicer's CMakeLists rejects >= 4.0 " +
               "on Windows. Install a 3.31.x (e.g. 'choco install cmake --version=3.31.6') and put " +
               "it ahead of Visual Studio's bundled copy on PATH.")
    }
    Write-Host "==> Using CMake $v at $($cmake.Source)"
}

# Put native Strawberry Perl ahead of any MSYS/Git perl for the OpenSSL build.
function Add-StrawberryPerl {
    $candidates = @('C:\Strawberry', "$env:SystemDrive\Strawberry")
    foreach ($c in $candidates) {
        $perlBin = Join-Path $c 'perl\bin'
        if (Test-Path (Join-Path $perlBin 'perl.exe')) {
            # perl\bin first (Configure runner); c\bin at the end for its runtime DLLs.
            $env:PATH = "$perlBin;$(Join-Path $c 'perl\site\bin');" + $env:PATH + ";$(Join-Path $c 'c\bin')"
            Write-Host "==> Using Strawberry Perl at $perlBin"
            return
        }
    }
    Write-Warning "Strawberry Perl not found; the OpenSSL dep may fail with MSYS perl."
}

Import-VcEnv
Add-StrawberryPerl

# Force synchronous PDB writes for every cl.exe invocation. cl reads the CL env
# var and prepends it to its command line. /FS routes .pdb writes through
# mspdbsrv so concurrent/retried compiles can't trip `fatal error C1033: cannot
# open program database` (hit by OpenSSL's shared ossl_static.pdb otherwise).
$env:CL = '/FS'

# Confirm the toolchain is reachable.
foreach ($tool in 'cmake', 'cl', 'perl') {
    $resolved = (Get-Command $tool -ErrorAction SilentlyContinue)
    if (-not $resolved) { throw "$tool not found on PATH after environment setup." }
    Write-Host ("    {0,-5} -> {1}" -f $tool, $resolved.Source)
}

# Check the CMake version BEFORE the deps build, not after. OrcaSlicer's
# version guard sits in the top-level CMakeLists, which is only reached at the
# slicer configure step -- i.e. ~2h of deps building later. Failing here turns
# a 125-minute dead end into a 10-second one.
Assert-CMakeSupported

function Build-Deps {
    Write-Host "==> Building libslic3r vendored deps (slow: 30-90+ min on first run)"
    New-Item -ItemType Directory -Force -Path $DepsBuild | Out-Null
    Push-Location $DepsBuild
    try {
        if (-not (Test-Path (Join-Path $DepsBuild 'CMakeCache.txt'))) {
            Invoke-Native 'deps configure' @(
                'cmake', (Join-Path $Orca 'deps'), '-G', 'Visual Studio 17 2022', '-A', 'x64',
                "-DDESTDIR=$DepsDestdir",
                '-DCMAKE_BUILD_TYPE=Release',
                '-DDEP_DEBUG=OFF',
                '-DCMAKE_POLICY_VERSION_MINIMUM=3.5')
        }
        # NOTE: top-level build is intentionally SERIAL (no `-m`). OrcaSlicer's
        # deps/CMakeLists.txt recommends `-j 1` at the top level; each dep still
        # parallelizes internally via its own BUILD_COMMAND. Building all heavy
        # deps concurrently caused PDB/mspdbsrv contention -> `fatal error C1033:
        # cannot open program database` in the OpenSSL build.
        Invoke-Native 'deps build' @('cmake', '--build', '.', '--config', 'Release', '--target', 'deps')
        # OrcaSlicer's deps CMakeLists deliberately leaves dep_Qhull out of the
        # aggregate `deps` target on MSVC (their official Windows flow ships a
        # prebuilt deps bundle that already contains qhull). We build from
        # source, and libslic3r links qhull, so build that target explicitly.
        Invoke-Native 'dep_Qhull build' @('cmake', '--build', '.', '--config', 'Release', '--target', 'dep_Qhull')
    } finally { Pop-Location }
    Write-Host "==> Deps installed at: $DepsPrefix"
}

function Build-Slicer {
    if (-not (Test-Path $DepsPrefix)) {
        throw "deps prefix not found at $DepsPrefix. Run: build_libslic3r_win.ps1 -Phase deps"
    }
    Write-Host "==> Configuring libslic3r build (GUI OFF, STATIC)"
    New-Item -ItemType Directory -Force -Path $SlicerBuild | Out-Null
    Push-Location $SlicerBuild
    try {
        if (-not (Test-Path (Join-Path $SlicerBuild 'CMakeCache.txt'))) {
            Invoke-Native 'slicer configure' @(
                'cmake', $Orca, '-G', 'Visual Studio 17 2022', '-A', 'x64',
                '-DCMAKE_BUILD_TYPE=Release',
                "-DCMAKE_PREFIX_PATH=$DepsPrefix",
                "-DCMAKE_INSTALL_PREFIX=$DepsPrefix",
                '-DSLIC3R_GUI=OFF',
                '-DSLIC3R_FHS=ON',
                '-DSLIC3R_STATIC=ON',
                '-DCMAKE_POLICY_VERSION_MINIMUM=3.5')
        }
        Write-Host "==> Building the libslic3r target only (skipping GUI binaries)"
        Invoke-Native 'libslic3r build' @('cmake', '--build', '.', '--config', 'Release', '--target', 'libslic3r', '--', '-m')
    } finally { Pop-Location }
    Write-Host "==> libslic3r static lib under: $SlicerBuild\src\libslic3r\Release\"
}

switch ($Phase) {
    'deps'   { Build-Deps }
    'slicer' { Build-Slicer }
    'all'    { Build-Deps; Build-Slicer }
}
Write-Host "==> Done ($Phase)."
