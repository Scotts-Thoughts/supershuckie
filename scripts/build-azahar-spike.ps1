<#
.SYNOPSIS
  Configure and build the headless Azahar core plus the Super Shuckie feasibility spike.

.DESCRIPTION
  Builds only Azahar's emulation libraries (no Qt, SDL, room server, web service, audio devices,
  Vulkan) with the MSYS2 UCRT64 toolchain used for melonDS, and the spike executable from
  azahar-rs/spike (hooked into Azahar's configure through CMAKE_PROJECT_citra_INCLUDE, so the
  third-party tree is not patched). Output: <BuildDir>/bin/azahar_spike.exe (or under
  supershuckie-spike/ depending on Azahar's runtime output directory).
#>
param(
    [string]$BuildDir = "build/azahar",
    [string]$Msys = "C:\msys64\ucrt64",
    [switch]$ConfigureOnly
)

$ErrorActionPreference = "Continue"
$env:PATH = "$Msys\bin;C:\msys64\usr\bin;" + $env:PATH
$env:CC = "gcc"
$env:CXX = "g++"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$hook = (Resolve-Path "azahar-rs/spike/hook.cmake").Path -replace '\\', '/'

& cmake -G Ninja -S third-party/azahar -B $BuildDir `
    -DCMAKE_BUILD_TYPE=Release `
    -DCMAKE_C_COMPILER=gcc -DCMAKE_CXX_COMPILER=g++ `
    -DENABLE_QT=OFF -DENABLE_SDL2=OFF -DENABLE_TESTS=OFF `
    -DENABLE_ROOM=OFF -DENABLE_ROOM_STANDALONE=OFF `
    -DENABLE_WEB_SERVICE=OFF -DENABLE_SCRIPTING=OFF -DENABLE_GDBSTUB=OFF `
    -DENABLE_CUBEB=OFF -DENABLE_OPENAL=OFF -DENABLE_LIBUSB=OFF `
    -DENABLE_SOFTWARE_RENDERER=ON -DENABLE_OPENGL=ON -DENABLE_VULKAN=OFF `
    -DCITRA_WARNINGS_AS_ERRORS=OFF -DENABLE_LTO=OFF `
    "-DCMAKE_PROJECT_citra_INCLUDE=$hook" 2>&1 | Write-Host
if ($LASTEXITCODE -ne 0) { throw "cmake configure failed" }
if ($ConfigureOnly) { exit 0 }

& cmake --build $BuildDir --target azahar_spike azahar_bundle -j 2>&1 | Write-Host
if ($LASTEXITCODE -ne 0) { throw "azahar build failed" }
Get-ChildItem -Path $BuildDir -Recurse -Filter azahar_spike.exe | ForEach-Object { Write-Host "built: $($_.FullName)" }
