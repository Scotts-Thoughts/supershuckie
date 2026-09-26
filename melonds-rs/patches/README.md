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

## 0002 — loads with stale polygon RAM; JIT fault handler on foreign threads

Masked replay keyframes (`supershuckie-replay-recorder/src/keyframe_masks.rs`) carry the chain
restart's copy of `VertexRAM`/`PolygonRAM` while the render list and the polygons already
submitted for the next flush are the keyframe's own. Loading one used to crash the 3D render
thread (a render-list slot that was empty in the copy has no vertices; `SetupPolygon` read
through a null vertex) and could crash VBlank or overrun `RenderPolygonRAM` (the copy's
`Translucent` flags disagree with the live `NumOpaquePolygons`).

* `GPU3D::DoSavestate` (load) keeps vertex indices in range and marks a polygon degenerate
  when it has no vertices, more than ten, a `VTop`/`VBottom` past its vertices or a null vertex
  the renderer would read. States melonDS itself writes never contain one.
* `GPU3D::DiscardGeometryOnLoad` (not serialised): set around a load whose polygon RAM may be
  stale. Every restored polygon is marked degenerate (nothing is drawn from them; polygons the
  game submits afterwards draw normally), and the pending polygons' `Translucent` flags are
  made to agree with `NumOpaquePolygons`. `melonds_rs_core_load_save_state_discarding_geometry`
  sets it; `melonds-rs/interface.cpp` then follows, frame by frame, whether the picture still
  lacks geometry (`melonds_rs_core_shows_discarded_geometry`).
* `ARMJIT_Memory` fault handlers return early when `NDS::Current` is null. The Windows vectored
  handler used to dereference it for an access violation on any thread that is not running an
  emulator (the render thread, the UI), fault again inside itself and recurse until the stack
  overflowed, so every such crash was reported as a stack overflow in `ExceptionHandler`.

Emulation is unaffected (only non-serialised flags, and bytes of polygon RAM a masked keyframe
already holds stale, change): `nds_bench --verify` from a stale keyframe and from boot, and
`supershuckie-core/examples/scrub_check.rs` against plain playback.
