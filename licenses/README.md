# licenses/

Inputs for `scripts/make-attributions.py`, which assembles the `libraries-and-attributions/`
folder that is distributed next to `supershuckie.exe` (the `supershuckie` CMake target runs it;
see `supershuckie-qt/CMakeLists.txt`, option `SUPERSHUCKIE_ATTRIBUTIONS`).

Most license texts are copied at build time from where they authoritatively live: this repo
(`COPYING`, `bootrom/`), the emulator submodules, the cargo registry (every crate in the
`supershuckie-frontend-c` dependency graph, via `cargo metadata`) and the MSYS2 prefix
(`share/licenses/` of Qt, SDL3, Qt's external libraries and the MinGW runtime, plus the Rust
standard library's `share/doc/rustc/`). This directory holds only what none of those provide:

- `notices/` — hand-written notices for third-party code that only carries its license in
  source-file headers: the code bundled inside melonDS (xxHash, SHA-1, FreeBIOS, ...) and
  mGBA (inih), and the provenance of the embedded GBA BIOS (VBA-M / Normmatt, GPL-2.0-or-later;
  byte-identical to `bios/gba_bios.bin` in https://github.com/Nebuleon/ReGBA).
- `qt/THIRD-PARTY-NOTICES.txt` and `qt/third-party/` — the third-party components compiled
  into the Qt libraries themselves (double-conversion, MD4C, D3D12MemoryAllocator, Wintab,
  Unicode data, ...). MSYS2's static Qt package ships only Qt's own license texts, so this is
  derived from the `qt_attribution.json` manifests in qtbase 6.11.1, restricted to what a
  Windows build of Core/Gui/Widgets/Svg/OpenGL and the plugins we import contains. Re-check it
  when the Qt package is upgraded to a new minor version.
- `texts/GPL-2.0.txt` — for the GBA BIOS.
- `crates/<name>/` — license files for crates that don't include one in their published
  package (`alloc-stdlib`, whose license is in its sibling crate's repository). The script
  errors out on any other crate without a license file; add an override here after checking
  the crate's repository.

When adding a dependency, nothing needs doing unless the script fails. When changing which
Qt modules/plugins are linked, or the MSYS2 packages the static link pulls in
(`MSYS2_PACKAGES` in the script mirrors the link line in `supershuckie-qt/CMakeLists.txt`),
update the script and this directory accordingly.
