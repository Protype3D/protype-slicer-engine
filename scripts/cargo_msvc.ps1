# Run a cargo command inside the MSVC (VS2022) build environment.
#
# The slicer-ffi build script compiles cpp/protype_slicer_bridge.cpp against the
# OrcaSlicer/libslic3r headers and links the static archives. That needs cl.exe,
# link.exe and the Windows SDK on PATH/INCLUDE/LIB. cargo/cc can usually auto-
# detect MSVC, but importing vcvars64 makes it deterministic (and matches the
# toolset the deps were built with).
#
# IMPORTANT: build with --release. The libslic3r static lib and its deps are
# compiled with the *release* CRT (/MD). A debug cargo build would compile the
# bridge with /MDd and the link fails with _ITERATOR_DEBUG_LEVEL / RuntimeLibrary
# mismatches (LNK2038).
#
# OpenCASCADE is built as DLLs on Windows (OrcaSlicer's deps recipe), so a
# libslic3r-linked binary imports TK*.dll at runtime. -RuntimeDllPath prepends
# the OCCT (and any sibling) DLL directories to PATH so the test/daemon exes can
# load them; pass the deps-prefix bin dirs.
#
# Args are passed as a single -CargoLine string (so tokens like `-p slicer-ffi`
# and `--` survive PowerShell's parameter binding intact).
#
# Usage:
#   pwsh scripts/cargo_msvc.ps1 -CargoLine "test --release -p slicer-ffi --features with-libslic3r"
#   pwsh scripts/cargo_msvc.ps1 -CargoLine "build --release -p slicer-daemon --features with-libslic3r"
param(
    [Parameter(Mandatory)][string]$CargoLine,
    [string]$RuntimeDllPath
)
$ErrorActionPreference = 'Stop'

$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$vsPath = & $vswhere -latest -products * `
    -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
    -property installationPath
if (-not $vsPath) { throw "VS C++ toolset (VC.Tools.x86.x64) not found." }
$vcvars = Join-Path $vsPath 'VC\Auxiliary\Build\vcvars64.bat'
Write-Host "==> Importing MSVC environment from $vcvars"
cmd /c "`"$vcvars`" >nul 2>&1 && set" | ForEach-Object {
    if ($_ -match '^([^=]+)=(.*)$') { Set-Item -Path "Env:\$($matches[1])" -Value $matches[2] }
}

if ($RuntimeDllPath) {
    $env:PATH = "$RuntimeDllPath;" + $env:PATH
    Write-Host "==> Prepended runtime DLL path: $RuntimeDllPath"
}

$repo = (Resolve-Path "$PSScriptRoot\..").Path
Push-Location $repo
try {
    Write-Host "==> cargo $CargoLine"
    # Split on whitespace; the cargo args here never contain embedded spaces.
    $cargoArgs = $CargoLine -split '\s+' | Where-Object { $_ -ne '' }
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try { & cargo @cargoArgs } finally { $ErrorActionPreference = $prev }
    exit $LASTEXITCODE
} finally { Pop-Location }
