# Standalone (single-exe) Build

Produces a self-contained `supershuckie.exe` with no DLLs to ship.

## Prerequisites (one-time, MSYS2 UCRT64)

```
pacman -S mingw-w64-ucrt-x86_64-qt6-static mingw-w64-ucrt-x86_64-libwebp mingw-w64-ucrt-x86_64-libtiff
```

You must already have a normal build in `build/` (cores built via the regular flow).

## Build

Run from the PowerShell tool, not Bash (cargo/gcc need it).

```powershell
$env:PATH = "C:\msys64\ucrt64\bin;" + $env:PATH
cmake .\supershuckie-qt -B build-static -G Ninja -DCMAKE_BUILD_TYPE=Release `
  -DSCRIPT_BUILD=ON -DSUPERSHUCKIE_STATIC=ON `
  -DCMAKE_C_COMPILER=gcc -DCMAKE_CXX_COMPILER=g++ `
  -DQt6_DIR="C:/msys64/ucrt64/qt6-static/lib/cmake/Qt6" `
  -DCMAKE_PREFIX_PATH="C:/msys64/ucrt64/qt6-static;C:/msys64/ucrt64" `
  -DMGBA_CORE="<abs>/build/mgba/libmgba.a" `
  -DMELONDS_CORE="<abs>/build/melonDS/src/libcore.a" `
  -DMELONDS_TEAKRA="<abs>/build/melonDS/src/teakra/src/libteakra.a"
cmake --build build-static --target supershuckie
```

Output: `build-static/supershuckie.exe` (~70 MB). Verify: `objdump -p supershuckie.exe | grep "DLL Name"` should show only Windows system DLLs.

## Gotcha

If `corrosion_import_crate` errors on reconfigure, `build-static/_deps/corrosion-src` got emptied (it's accidentally git-tracked). Fix:
```
cp -r build/_deps/corrosion-src/. build-static/_deps/corrosion-src/
```
