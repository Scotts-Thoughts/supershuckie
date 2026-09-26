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
