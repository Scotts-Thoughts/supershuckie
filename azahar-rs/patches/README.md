# Super Shuckie patches to Azahar

`third-party/azahar` is a pristine clone; these patches are applied on top of it (same
convention as `melonds-rs/patches`).

- `0001-supershuckie-deterministic-fs-delays.patch` — with `deterministic_async_operations`
  on, Azahar still computes the emulated delay of a file read or open as
  `emulated delay − host time the operation took`, so the emulated completion time of every ROM
  read depends on disk speed. Two machines replaying the same inputs then see file reads finish
  at different emulated times and desync during loads (measured: heaps diverged in the first
  10 s of Pokémon X's boot and sometimes stayed diverged). The patch uses the full emulated
  delay in deterministic mode. Found by the spike (`azahar-3ds-spike-report.md`).
- `0002-supershuckie-raw-state-buffers.patch` — `System::SaveStateRawInto(buffer, capacity,
  valid_len)`, `System::SaveStateRaw(std::vector<u8>&)` and `System::LoadStateRaw(std::span<const
  u8>)`: the state with no zstd, written straight into / read straight from the caller's buffer
  (Azahar's own path makes three copies and compresses). The raw layout is a 32-byte header
  (`SSRAW002`, RAM generation, sizes), the RAM regions back to back, then the boost archive of
  everything else. A buffer that still holds a recent raw state of the process (the header says
  which) gets only the RAM pages written since (see 0004); the archive part is rewritten. The
  replay recorder keeps its own delta codec and compression, so this is what it wants.
- `0003-supershuckie-skip-drawing.patch` — `VideoCore::g_skip_drawing`: the OpenGL rasterizer
  accepts every draw batch and draws nothing. Emulated memory is unaffected (heap hashes identical
  with and without it); Pokémon X runs at 738 fps instead of 336–423, which is what replay seeks
  use.
- `0004-supershuckie-page-table-runs.patch` — memory, two things. `PageTable::serialize`
  writes the 2^20-entry page reference array as runs of consecutively mapped pages (a few dozen
  for a game) and the page attributes as one block; the old form serialised a million tracked
  `MemoryRef`s and was 60 ms of every save state (first reached through the CPU core's page table
  pointer). Adds `MemoryRef::GetBackingMem/GetOffset`. And the RAM regions (FCRAM, VRAM, New 3DS
  extra RAM, DSP RAM) live in one host block allocated with Windows write watch
  (`MEM_WRITE_WATCH`/`GetWriteWatch`), kept across save-state loads; `MemorySystem::RawRegionIo`
  lets a raw save copy only the pages written since the destination buffer's previous state
  (generations of written-page bitmaps, the last 512 kept) on a few threads, and a raw load copy
  the regions straight in. Elsewhere than Windows every raw save copies all of it. Measured on
  Pokémon X (Old 3DS mode): ~1100 pages (4 MB) change per 8-second keyframe, the copy is a
  millisecond, and the incremental state is byte-identical to a full one (`azahar_spike
  --dirty-bench`). Changes the save-state format (our builds only).
- `0005-supershuckie-sparse-handle-table-and-codeset-registry.patch` — the kernel handle table
  writes only its occupied slots; `CodeSet` writes a key into a process-wide registry of loaded
  program binaries instead of the binary itself (30 MB and 4 ms per save; a state can then only
  be loaded in a process that has loaded the same game, which the replay player guarantees).
  Changes the save-state format (our builds only).
- `0006-supershuckie-serialize-timing.patch` — with `SUPERSHUCKIE_SERIALIZE_TIMING=1` in the
  environment, prints where a save state's time goes (system components, the APT module, the
  process, and what the rasterizer flush before a save downloads); 0004 and 0005 print the page
  table's steps, the RAM copy's mode, and the handle table's objects by type under the same
  variable. Measurement only; the state format is unchanged by it.
- `0007-supershuckie-rom-read-log-and-stale-draw-tracking.patch` — two things for format-v9
  replays. (1) A log of the game's RomFS reads (`FileSys::SetRomReadLog`/`TakeRomReads`): every
  read a `DirectRomFSReader` serves of the game file itself (reached through plain `SubIOFile`
  windows, so the offset is the byte offset in the `.3ds`/`.cci` on disk), merged when
  consecutive. The recorder looks the changed bytes of a keyframe up in what was read since its
  reference keyframe and stores ROM copies as references (`stored_keyframe.rs`). (2) Skipped-draw
  tracking for seeks (`VideoCore::g_stale_surfaces`): while `g_skip_drawing` is set, a skipped
  draw marks its colour buffer stale (with the frame, `g_draw_frame`); a memory fill of the whole
  buffer makes it fresh again, a display transfer or texture copy carries its source's staleness
  to its destination, and a drawn draw that samples a stale texture makes its target stale. A
  drawn draw never makes a buffer fresh (it may redraw only part of it). The price is that a
  game drawing its targets without fills (ORAS double-buffers both screens' targets and never
  fills them) has 15–35% of its seeks redone drawn. Measured on Alpha Sapphire: every seek
  showed exactly the picture of the same walk drawn frame by frame. The binding clears
  the list on every state load and reports the oldest stale frame
  (`azahar_rs_core_stale_frames_ago`); the core redoes a seek's walk drawn from there when it is
  not empty. Neither changes emulation or the state format.
- `0008-supershuckie-console-clock-and-language.patch` — the 3DS date and language settings.
  (1) `SharedPage::Handler` (class version 1) serialises its clock start (`init_time`), so a
  save state and every replay keyframe keep the console time they were made with; before, a
  loaded state took the start from the settings of the moment, and a replay played with another
  date setting read other times. (2) The console time no longer depends on the host's time zone:
  Azahar measured it from `mktime(2000-01-01 00:00)`, the *host's* local midnight, so the same
  fixed `init_time` gave different console times on machines in different zones (on a UTC−5
  machine the clock stood at 2000-01-01 00:00:00 for the first ~6 hours of play). `init_time` is
  now the console's own clock reading (seconds since 1970-01-01 on that clock) against the
  constant 946684800. States of class version 0 (every build before this patch) load with the
  old start (946681277) and the old host-zone epoch, so existing replays play back exactly as
  they were recorded. (3) `Service::CFG::g_system_language_override`: the system language the
  config savegame gets as it loads (in memory only), before the region choice adjusts it to one
  the game's region has. The language is part of the CFG module's state already. Changes the
  save-state format (our builds only; older states still load).
- `0009-supershuckie-vram-access-tracking.patch` — `VideoCore::g_vram_access_tracking` /
  `g_vram_first_access`: while tracking is on, what touched each 4 KB page of VRAM first since
  the record was last taken (nothing, a read, a write). Reads: textures the rasterizer cache looks
  up (cube faces and shadow maps included), display transfer and texture copy sources, vertex and
  index data, command lists, and HLE block reads of the VRAM mapping: what carries a page's
  content into something that outlives the frame. Blending, depth tests and the screen showing a
  framebuffer only affect that frame's picture, which a replay seek redraws several times before
  showing it, so they do not count. Writes: draw colour and depth targets, transfer, copy and
  fill destinations. A write covering only part of a page counts as a read. CPU reads through the
  JIT's fast path are not seen. Format-v10 replays leave out of a keyframe the VRAM pages that the
  interval after it writes before reading (render targets, display framebuffers): about 40% of
  every 3DS delta keyframe on Pokémon Omega Ruby (287 of 290 changed pages in a typical one).
  Measurement only; emulation and the state format are unchanged.
- `0010-supershuckie-gl-surface-recycling-and-clear-leak.patch` — two OpenGL texture-cache
  bugs that cost most of the frame time on Pokémon Sun battles and every save state's worth of
  video memory. (1) Recycling never happened with OpenGL: the runtime's free tick was the
  current frame, so a discarded surface could only be reused from the next frame on, but the
  collector destroyed it at the next frame first; render-to-texture effects re-created ~2
  textures and ~1.5 framebuffers every frame, and in NVIDIA's threaded driver every `glGen*`
  waits for the driver thread (~28% of the emulation thread's samples). The free tick is now
  one ahead (OpenGL orders reuse after the commands still using a texture) and discarded
  surfaces wait 60 frames (at most 512 of them) for a `CreateSurface` with the same parameters;
  a recycled surface gets a new surface's flags. (2) `ClearAll`, run on every save state and
  state load (so every replay keyframe and seek), dropped the page table without unregistering
  the surfaces, which were then never freed: ~45 MB of video memory per keyframe (1,404 surfaces
  after 12 keyframes on Sun); a recording at 4x takes a keyframe a second, and the leaked
  textures then spill into system memory. It now unregisters them, so the collector frees them
  or the next frame recycles them. Also `VideoCore::g_cache_stats`, counters of what the cache
  does (surfaces created, recycled, unregistered, destroyed, framebuffers created, uploads,
  downloads, CPU-write invalidations, draws) for `n3ds_perf_lab`. Measured with
  `supershuckie-core/examples/n3ds_perf_lab.rs`: Sun 192 → 385 fps, Omega Ruby 262 → 478, Alpha
  Sapphire 230 → 399 (every picture read back, keyframes every 240 frames); the game heap, the
  linear heap and both screens hash identically with and without the patch at every 240th frame
  on all three. Emulation and the state format are unchanged.
