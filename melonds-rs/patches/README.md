# Local melonDS patches

melonDS is a git submodule pinned to an upstream commit, so Super Shuckie's own changes to it
live here as patch files. `scripts/build-melonds.ps1` / `scripts/build-melonds.sh` apply any
that are not applied yet before building; to do it by hand:

    git -C melonds-rs/melonDS apply --ignore-whitespace ../patches/0001-supershuckie-skip-drawing-and-zeroed-gxfifo-padding.patch

Patches must keep emulation bit-exact: replays recorded before and after them have to play back
identically (check with `nds_bench --verify`, see `supershuckie-core/examples/nds_bench.rs`).

## 0001 — skip drawing of undisplayed frames; zeroed GX FIFO entries

* `GPU::SkipDrawing`: a presentation hint. When set, `GPU2D_Soft::DrawScanline` does not
  composite the line (BG/OBJ layers, display modes, master brightness) and instead runs
  `SkipScanline_BGOBJ`, which applies exactly the unit-state side effects drawing has — the
  horizontal window activity bits, the affine BG reference registers every affine-class BG
  drawer advances, and the BG mosaic line counter — so save states come out byte-identical to a
  drawn frame. Lines with display capture latched are always drawn (capture reads the composited
  line back into VRAM). The 3D scanline handshake (`GetLine`) and sprite pre-rendering are
  untouched. Set through `melonds_rs_core_set_skip_drawing`; the frontend draws one frame in
  `floor(speed)` from 2x up, which is worth about a quarter of each skipped frame's time.
* `CmdFIFOEntry` is a `union { u64; struct { u32 Param; u8 Command; } }`, so three padding
  bytes per entry were uninitialised stack bytes that `DoSavestate` copied into every save state
  (and every replay keyframe). The two constructors now value-initialise the entry.
