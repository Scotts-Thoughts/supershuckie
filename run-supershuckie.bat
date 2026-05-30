@echo off
REM Launch the MinGW/UCRT64-built SuperShuckie. The exe needs the UCRT64
REM runtime DLLs (Qt6, SDL3, libgcc, libstdc++, winpthread) on PATH.
set "PATH=C:\msys64\ucrt64\bin;%PATH%"
start "" "%~dp0build\supershuckie.exe" %*
