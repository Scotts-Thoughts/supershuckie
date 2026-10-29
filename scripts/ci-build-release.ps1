<#
.SYNOPSIS
  Build the standalone release exe from a clean checkout (what .github/workflows/release.yml runs).

.DESCRIPTION
  Everything from scratch into build/: mGBA, melonDS (patched, LTO, no PGO), Azahar (cloned at the
  pinned commit and patched), then the statically linked app with -DSUPERSHUCKIE_RELEASE_BUILD=ON,
  so the Updates menu treats it as the published build of -Version. The version is written into
  Cargo.toml first (scripts/bump-version.sh), so the exe's own version always matches its release.

  Output in dist-release/: supershuckie.exe (stripped; what the Updates menu downloads),
  supershuckie-<version>-win64.zip (the exe plus libraries-and-attributions/, for people) and
  supershuckie-<version>-symbols.zip (supershuckie.debug, the symbols, for debugging).

  Needs an MSYS2 UCRT64 environment with: gcc cmake ninja pkgconf qt6-static sdl3 rust python
  libwebp libtiff (see the workflow). Runs locally too, but uses build/ - use a clean worktree.

.EXAMPLE
  .\scripts\ci-build-release.ps1 -Version 0.5.2 -Msys D:\a\_temp\msys64\ucrt64
#>
param(
    [Parameter(Mandatory = $true)][string]$Version,
    [string]$Msys = "C:\msys64\ucrt64",
    [string]$Repo = "Scotts-Thoughts/supershuckie",
    # The Azahar commit the patches in azahar-rs/patches apply to (third-party/azahar is not a
    # submodule). Keep in step with the local clone when moving Azahar forward.
    [string]$AzaharCommit = "ce0486d4640cada26d45cf3ab5263f07a652c3de"
)

# Native tools write progress to stderr; exit codes are checked instead.
$ErrorActionPreference = "Continue"
$msysRoot = Split-Path -Parent $Msys
$env:PATH = "$Msys\bin;$msysRoot\usr\bin;" + $env:PATH
$env:CC = "gcc"
$env:CXX = "g++"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

function Check([string]$what) {
    if ($LASTEXITCODE -ne 0) { throw "$what failed (exit code $LASTEXITCODE)" }
}

Write-Host "== version $Version =="
& "$msysRoot\usr\bin\bash.exe" -c "cd '$($root -replace '\\','/')' && scripts/bump-version.sh '$Version'"
Check "setting the version"

Write-Host "== mGBA =="
& cmake -G Ninja ./mgba-rs/mgba -B build/mgba -DLIBMGBA_ONLY=ON -DDISABLE_FRONTENDS=ON -DCMAKE_BUILD_TYPE=Release
Check "mGBA configure"
& cmake --build build/mgba -j
Check "mGBA build"

Write-Host "== melonDS =="
& "$PSScriptRoot\build-melonds.ps1" -BuildDir build/melonDS -Msys $Msys
Check "melonDS build"

Write-Host "== Azahar $AzaharCommit =="
$azahar = "third-party/azahar"
if (-not (Test-Path "$azahar/.git")) {
    New-Item -ItemType Directory -Force -Path $azahar | Out-Null
    & git -C $azahar init -q
    & git -C $azahar remote add origin https://github.com/azahar-emu/azahar.git
}
& git -C $azahar fetch -q --depth 1 origin $AzaharCommit
Check "fetching Azahar"
& git -C $azahar checkout -q --force FETCH_HEAD
Check "checking out Azahar"
& git -C $azahar submodule update --init --recursive --depth 1 --jobs 8
if ($LASTEXITCODE -ne 0) {
    # A shallow fetch of a submodule commit that is not a branch tip can be refused.
    & git -C $azahar submodule update --init --recursive --jobs 8
    Check "fetching Azahar's submodules"
}
foreach ($patch in Get-ChildItem -Path "azahar-rs/patches" -Filter *.patch | Sort-Object Name) {
    & git -C $azahar apply --check --reverse --ignore-whitespace $patch.FullName 2>&1 | Out-Null
    if ($LASTEXITCODE -eq 0) { continue }  # already applied
    & git -C $azahar apply --ignore-whitespace $patch.FullName
    Check "applying $($patch.Name)"
}
& "$PSScriptRoot\build-azahar-spike.ps1" -BuildDir build/azahar -Msys $Msys
Check "Azahar build"

Write-Host "== Super Shuckie (static) =="
$msysCmake = $Msys -replace '\\', '/'
& cmake ./supershuckie-qt -B build -G Ninja -DCMAKE_BUILD_TYPE=Release `
    -DSCRIPT_BUILD=ON -DSUPERSHUCKIE_STATIC=ON -DSUPERSHUCKIE_RELEASE_BUILD=ON `
    "-DSUPERSHUCKIE_UPDATE_REPO=$Repo" `
    -DCMAKE_C_COMPILER=gcc -DCMAKE_CXX_COMPILER=g++ `
    "-DRust_COMPILER=$msysCmake/bin/rustc.exe" "-DRust_CARGO=$msysCmake/bin/cargo.exe" `
    "-DQt6_DIR=$msysCmake/qt6-static/lib/cmake/Qt6" `
    "-DCMAKE_PREFIX_PATH=$msysCmake/qt6-static;$msysCmake"
Check "app configure"
& cmake --build build --target supershuckie
Check "app build"

$exe = "build\supershuckie.exe"
$systemDllPrefixes = @(
    "api-ms-win-", "ADVAPI32", "AUTHZ", "comdlg32", "d3d9", "d3d11", "d3d12",
    "dwmapi", "DWrite", "dxgi", "GDI32", "IMM32", "KERNEL32", "kernel32",
    "NETAPI32", "ntdll", "ole32", "OLEAUT32", "RPCRT4", "SETUPAPI", "SHCORE",
    "SHELL32", "SHLWAPI", "USER32", "USERENV", "UxTheme", "VERSION", "WINMM",
    "WS2_32", "WTSAPI32", "bcryptprimitives", "bcrypt", "CRYPT32", "OPENGL32",
    "WINHTTP", "IPHLPAPI", "PSAPI", "WINSPOOL"
)
$dllNames = & objdump -p $exe | Select-String "DLL Name:\s+(\S+)" |
    ForEach-Object { $_.Matches[0].Groups[1].Value } | Select-Object -Unique
$nonSystem = $dllNames | Where-Object { $name = $_; -not ($systemDllPrefixes | Where-Object { $name -like "$_*" }) }
if ($nonSystem) {
    throw "The exe imports non-system DLLs, so it would not run on its own: $($nonSystem -join ', ')"
}
Write-Host "Only Windows system DLLs are imported."

Write-Host "== packaging =="
$out = "dist-release"
Remove-Item -Recurse -Force $out -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Path $out | Out-Null
# The symbols are half the exe and only a debugger reads them: ship the exe stripped and the
# symbols as their own download. The debuglink lets gdb find supershuckie.debug next to the exe.
& objcopy --only-keep-debug $exe "$out\supershuckie.debug"
Check "extracting the symbols"
& objcopy --strip-all --add-gnu-debuglink="$out\supershuckie.debug" $exe "$out\supershuckie.exe"
Check "stripping the exe"
$stripped = Join-Path $root "$out\supershuckie.exe"
Add-Type -AssemblyName System.IO.Compression, System.IO.Compression.FileSystem
$level = [IO.Compression.CompressionLevel]::Optimal
$symbols = [IO.Compression.ZipFile]::Open((Join-Path $root "$out\supershuckie-$Version-symbols.zip"), [IO.Compression.ZipArchiveMode]::Create)
try {
    [void][IO.Compression.ZipFileExtensions]::CreateEntryFromFile($symbols, (Join-Path $root "$out\supershuckie.debug"), "supershuckie.debug", $level)
}
finally {
    $symbols.Dispose()
}
Remove-Item "$out\supershuckie.debug"
$zip = "$out\supershuckie-$Version-win64.zip"
$archive = [IO.Compression.ZipFile]::Open((Join-Path $root $zip), [IO.Compression.ZipArchiveMode]::Create)
try {
    [void][IO.Compression.ZipFileExtensions]::CreateEntryFromFile($archive, $stripped, "supershuckie.exe", $level)
    $attributions = Join-Path $root "build\libraries-and-attributions"
    if (-not (Test-Path $attributions)) { throw "build\libraries-and-attributions is missing" }
    $base = (Get-Item $attributions).Parent.FullName.TrimEnd('\') + '\'
    Get-ChildItem $attributions -Recurse -File | ForEach-Object {
        $entry = $_.FullName.Substring($base.Length).Replace('\', '/')
        [void][IO.Compression.ZipFileExtensions]::CreateEntryFromFile($archive, $_.FullName, $entry, $level)
    }
}
finally {
    $archive.Dispose()
}
Get-ChildItem $out | ForEach-Object { Write-Host ("{0} ({1:N1} MB)" -f $_.Name, ($_.Length / 1MB)) }
