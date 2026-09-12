<#
.SYNOPSIS
  Build the melonDS static library for Super Shuckie, with profile-guided optimisation and LTO.

.DESCRIPTION
  The Nintendo DS core runs the interpreter (the JIT is not reproducible across save-state loads,
  so replays require it off), and an interpreter gains 12-18% from a training profile plus LTO
  with no change in emulation behaviour. This script does the three-step PGO build:

    1. configure + build melonDS instrumented (-fprofile-generate) into the build directory
    2. link the headless benchmark (supershuckie-core/examples/nds_bench.rs) against it and run
       the training workload: every replay given with -Replay, plus a direct boot of the ROM
    3. reconfigure the same directory with -fprofile-use -flto=auto and rebuild

  Without -Rom the profile step is skipped and a plain LTO build is produced.

  The result is <BuildDir>/src/libcore.a, which the app build picks up through -DMELONDS_CORE
  (default build/melonDS/src/libcore.a, see README "Building"). Re-run whenever melonDS sources
  change; the profile is only as good as the workload, so train on the games you actually run.

.EXAMPLE
  .\scripts\build-melonds.ps1 -Rom "C:\roms\HeartGold.nds" -Replay "UserData\HeartGold.nds-data\replays\run.replay"

.EXAMPLE
  .\scripts\build-melonds.ps1 -Rom "C:\roms\Platinum.nds" -Replay a.replay, b.replay -Frames 20000
#>
param(
    [string]$BuildDir = "build/melonDS",
    [string]$Rom,
    [string[]]$Replay = @(),
    [int]$Frames = 18000,
    [string]$Msys = "C:\msys64\ucrt64"
)

# Native tools (cmake, cargo, git) write progress and warnings to stderr, which Windows PowerShell
# would turn into terminating errors under "Stop"; exit codes are checked explicitly instead.
$ErrorActionPreference = "Continue"
$env:PATH = "$Msys\bin;C:\msys64\usr\bin;" + $env:PATH
$env:CC = "gcc"
$env:CXX = "g++"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

# Super Shuckie's local changes to melonDS (see melonds-rs/patches/README.md) are kept as patch
# files because melonDS is a submodule; apply whichever are not applied yet.
# (--ignore-whitespace: the checkout may be CRLF, the patches are LF.)
foreach ($patch in Get-ChildItem -Path "melonds-rs/patches" -Filter *.patch | Sort-Object Name) {
    & git -C melonds-rs/melonDS apply --check --reverse --ignore-whitespace $patch.FullName 2>&1 | Out-Null
    if ($LASTEXITCODE -eq 0) { continue }  # already applied
    & git -C melonds-rs/melonDS apply --ignore-whitespace $patch.FullName 2>&1 | Write-Host
    if ($LASTEXITCODE -ne 0) { throw "could not apply $($patch.Name) to melonds-rs/melonDS" }
    Write-Host "applied $($patch.Name)"
}

$common = @(
    "-G", "Ninja", "./melonds-rs/melonDS", "-B", $BuildDir,
    "-DENABLE_JIT=ON", "-DENABLE_OGLRENDERER=OFF", "-DENABLE_GDBSTUB=OFF", "-DBUILD_QT_SDL=OFF",
    "-DCMAKE_BUILD_TYPE=Release"
)

function Configure([string]$flags) {
    & cmake @common "-DCMAKE_CXX_FLAGS=$flags" "-DCMAKE_C_FLAGS=$flags" 2>&1 | Write-Host
    if ($LASTEXITCODE -ne 0) { throw "cmake configure failed" }
    & cmake --build $BuildDir -j 2>&1 | Write-Host
    if ($LASTEXITCODE -ne 0) { throw "melonDS build failed" }
}

# LTO objects are "fat" so that a linker without the GCC plugin (rustc's direct link of the
# benchmark, for instance) still gets ordinary code; the app link through g++ uses the plugin.
$lto = "-flto=auto -ffat-lto-objects"

if (-not $Rom) {
    Write-Host "== melonDS: LTO build (no profile; pass -Rom/-Replay for PGO) =="
    Configure $lto
    Write-Host "done: $BuildDir/src/libcore.a"
    exit 0
}

Write-Host "== 1/3 melonDS: instrumented build =="
Get-ChildItem -Path $BuildDir -Recurse -Filter *.gcda -ErrorAction SilentlyContinue | Remove-Item
Configure "-fprofile-generate -fprofile-update=atomic"

Write-Host "== 2/3 training: linking nds_bench against the instrumented library =="
$link = @()
foreach ($a in @(
    "-Wl,--start-group",
    "$BuildDir/src/libcore.a", "$BuildDir/src/teakra/src/libteakra.a", "build/mgba/libmgba.a",
    "-lstdc++", "-lshlwapi", "-lws2_32", "-lmingwex", "-lmingw32", "-lmsvcrt", "-lucrt",
    "-lkernel32", "-luser32", "-lgcc", "-lgcc_eh",
    "-Wl,--end-group",
    "-fprofile-generate", "-lgcov",
    "-Wl,--start-group", "-lmingwex", "-lmingw32", "-lmsvcrt", "-lucrt", "-lkernel32", "-luser32", "-lgcc", "-lgcc_eh", "-Wl,--end-group"
)) { $link += @("-C", "link-arg=$a") }
& cargo rustc --release -p supershuckie-core --example nds_bench '--' @link 2>&1 | Write-Host
if ($LASTEXITCODE -ne 0) { throw "nds_bench link failed" }
$bench = "target/release/examples/nds_bench.exe"

# The instrumented core runs ~10x slower than normal; budget about a minute per 3,000 frames.
foreach ($r in $Replay) {
    Write-Host "== training on $r ($Frames frames) =="
    # --present-every 4: the mix the app runs at 4x (one frame in four composited), which is what
    # the profile should reflect; one drawn frame in four is still plenty of samples for drawing.
    & $bench $Rom --replay $r --frames $Frames --warmup 0 --keyframes 120 --present-every 4 2>&1 | Write-Host
    if ($LASTEXITCODE -ne 0) { throw "training run failed for $r" }
}
Write-Host "== training on a direct boot of the ROM =="
& $bench $Rom --frames 2000 --warmup 0 2>&1 | Write-Host
if ($LASTEXITCODE -ne 0) { throw "training run failed (direct boot)" }

$gcda = (Get-ChildItem -Path $BuildDir -Recurse -Filter *.gcda | Measure-Object).Count
if ($gcda -eq 0) { throw "no profile data was written (expected .gcda files under $BuildDir)" }
Write-Host "collected $gcda profile files"

Write-Host "== 3/3 melonDS: optimised rebuild with the profile + LTO =="
Configure "-fprofile-use -fprofile-correction -fprofile-partial-training -Wno-missing-profile $lto"

Write-Host "done: $BuildDir/src/libcore.a (PGO + LTO). Rebuild the app (cmake --build build) to pick it up."
