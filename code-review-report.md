# Super Shuckie stability review

Date: 2026-09-16. Branch: `replay-bookmarks` at `e6725e9` plus the uncommitted working tree.
Line numbers refer to the working tree at review time.

## Scope and method

Every source file in the ten workspace crates and the Qt frontend was read in full by six
parallel reviewers, one per area, and the higher-severity claims were then re-checked by hand
against the source (and, for the mGBA and SDL items, against the vendored library sources).
Nothing was modified. The vendored emulator cores (melonDS, mGBA, SameBoy) were only consulted
to confirm how the bindings drive them.

Test baseline, run from PowerShell with the MSYS2 UCRT64 toolchain:

| Crate | Result |
|---|---|
| supershuckie-replay-recorder | 63 passed |
| supershuckie-memory-tools | 21 passed |
| supershuckie-frame-server | 6 passed |
| supershuckie-frontend-webserver | 2 passed |
| supershuckie-pokeabyte-integration | 0 tests exist |
| supershuckie-core (19 tests), supershuckie-frontend (14 tests) | **cannot be built by `cargo test`**: the test binary fails to link `melonDS::bios_arm7_bin` and the C++ runtime because only the CMake build supplies `libcore.a` / `libstdc++`. Those 33 tests are currently unrunnable outside a hand-assembled link line (see T1). |

Severity scale: **Critical** = process abort or silent data loss reachable in ordinary use.
**High** = crash or corruption reachable with a plausible input or environment. **Medium** =
wrong behaviour, hang, or latent unsoundness. **Low** = edge cases, leaks, spec drift, hygiene.

Totals: 6 critical, 9 high, 22 medium, 30 low.

---

## Critical

### C1. Core-thread `expect`/`todo!` panics turn ordinary failures into a process abort
- `supershuckie-core/src/thread.rs:1106` (`start_recording_replay(...).expect(...)`), `:1115` (`resume_recording_replay(...).expect(...)`), `supershuckie-core/src/lib.rs:1210` (`todo!("can't go to 0th keyframe")`), `:1295` (`todo!("can't go to {frame}")`), `supershuckie-core/src/emulator/game_boy_advance.rs:60` and `nintendo_ds.rs:37` (`Core::new(...).expect("failed to make a core (TODO: HANDLE THIS ERROR)")`).
- The workspace builds with `panic = "abort"`, and even where it did not, every wrapper method on `ThreadedSuperShuckieCore` does `.expect("... the core thread has crashed")`, so one panic on the core thread aborts the app on the next UI tick.
- Triggers, all confirmed reachable from the frontend:
  1. Replays directory on a full or removed drive: the header write in `ReplayFileRecorder::new_with_metadata` fails, `start_recording_replay` panics.
  2. A truncated or damaged replay. `ReplayFilePlayer::new(.., override_errors = true)` deliberately admits truncated files (`stream_truncated`), and the REST `/load-replay`, resume and export paths always pass `true`. Resuming such a file panics at `thread.rs:1115`; seeking to a blob that fails to decompress or apply hits the `todo!`s.
  3. Any ROM file the C core rejects (empty, wrong format) makes `Core::new` return `None` and the `expect` fires on the UI thread.
- Fix: return `Result` through `ThreadCommand` replies; make `go_to_replay_frame_inner` set `replay_stalled` like `go_to_replay_keyframe` already does; make `new_from_rom` return `Result`; refuse resume/export when `stream_truncated()`.

### C2. Taking a save state (or hard reset, or RAM edit) while paused during a recording corrupts the timeline and can abort the app
- `supershuckie-core/src/lib.rs:1118` sets `mid_frame = time.frames == 0`. For GBA and NDS `run()` returns `RunTime::NONE` whenever wall-clock time has not reached the next frame (`game_boy_advance.rs:67-72`), which is most calls, so a paused core is almost always flagged mid-frame.
- `finish_current_frame` (`lib.rs:452`) then emulates a whole extra frame while paused. It is called by `ThreadCommand::CreateSaveState` (`thread.rs:1146`), `hard_reset` (`lib.rs:801`) and RAM-tools edits/freezes (`memory_monitor.rs:812`).
- That frame runs `do_frame_timekeeping` with a wall-clock timestamp taken while the timer is paused (inflated by the pause length). On unpause, `unpause_timer` re-bases from the previous frame's time, so the next frame's timestamp is smaller than the one already recorded.
- Consequences, in order of badness:
  - `ReplayFileRecorder::next_frame` returns `BadInput` ("time went backwards") and the frontend silently `force_stop_recording_replay()`s. Repro: record a GBA game, pause, wait a few seconds, press save state, unpause. The recording ends with no clear cause.
  - If a keyframe is due on that frame, `insert_keyframe_with` (`replay_file/record.rs:426`) uses `assert!` instead of returning an error. On the recorder thread that panic is swallowed while the `Mutex` guard is held, the mutex is poisoned, every later command is dropped (`let _ = sender.send`), and `close()` at `record/thread.rs:93` panics on `into_inner().expect(..)`, killing the core thread and then the app (C1).
  - Rapid-fire and frame-skip logic share the same `mid_frame` conflation (see M1, M4).
- Fix: have cores report mid-frame explicitly (or only treat `frames == 0` as mid-frame when `frame_period_microseconds()` is `None`); use the paused-clock timestamp while paused; replace the assert with `BadInput`; recover a poisoned recorder mutex in `close()`.

### C3. mGBA: an undersized `.sav` makes the save-data pointer NULL, then it is written through
- `mgba-rs/interface.cpp:85-89` hands the user's `.sav` bytes to `VFileFromMemory`, whose `truncate` cannot grow (`_vfmTruncateNoExpand`) and whose `map` returns 0 when asked for more than it holds (`mgba/src/util/vfs/vfs-mem.c:277`).
- `GBASavedataInitFlash` (`mgba/src/gba/savedata.c:279-293`) computes `end = vf->size`, calls `truncate(flashSize)` (no-op), `data = map(flashSize)` (NULL), then `memset(&data[end], 0xFF, ...)`. Same shape for SRAM.
- Trigger: a FLASH1M game (Emerald, FireRed, LeafGreen) with a 64 KiB save from another emulator, or any `.sav` shorter than the detected type. The frontend passes the file unchanged (`supershuckie-frontend/src/lib.rs:930`). Segfault on the first save access.
- Fix: pad the buffer with 0xFF to at least 128 KiB before building the VFile, or use the growable `VFileMemChunk`.

### C4. `settings.json` is written non-atomically and any parse error aborts startup
- `supershuckie-frontend/src/lib.rs:1569-1575` `write_config` truncates and rewrites in place. `lib.rs:172-175` `.expect("failed to init user_dir")` on the loader; `settings.rs:42` turns any serde error into that failure.
- `write_config` now runs synchronously on every bookmark-type change (`bookmarks.rs:748`, including REST-driven ones) and at exit, so a crash or power loss mid-write leaves a half file that prevents the app from ever starting. The same happens for a hand edit, an unknown enum variant from a newer build, an out-of-range value, or a missing `color` on a bookmark type. `fs::create_dir` (not `create_dir_all`) also panics if the parent of the data dir is missing.
- Fix: write to `settings.json.tmp` and rename; on parse failure keep a `.bad` copy, fall back to defaults and report.

### C5. Replay parser allocates from an untrusted length, and a panic in the decompression worker hangs the app
- `supershuckie-replay-recorder/src/packet/io.rs:196-204`: `Vec<T>::read_all` does `Vec::with_capacity(len)` with `len` read straight from the file before any element is read. A `CompressedBlob` keyframe count encoded as a 9-byte varint of `0x7FFF_FFFF_FFFF_FFFF` gives a `capacity overflow` panic; `2^40` gives an allocation abort. Same for `DeltaKeyframe.diff`, `KeyframeMetadata.counters` and `CompressedBlob.bookmarks`. `allow_some_corruption` catches `Err`, not panics. `bookmarks.rs:312/318` already guard with `.min(from.len())`, so this is an omission.
- `replay_file/playback.rs:680-711`: `decompress_immediately` busy-spins on `InProgress` with no yield, timeout or dead-worker detection. If the threaded decompression of the next blob panics (from the above, or any future panic), the UI/core thread spins at 100% CPU forever. Even in the normal case it burns a core for the 100-175 ms an NDS blob takes.
- Fix: `Vec::new()` or `with_capacity(len.min(what.len()))`; report worker completion through a channel/`JoinHandle` and block on it instead of spinning, falling back to main-thread decompression if the worker died.

### C6. `SDL_Quit()` runs before `MainWindow` is destroyed, so the audio stream is destroyed twice
- `supershuckie-qt/src/main.cpp:47-58`: `MainWindow window;` is a stack object that outlives `SDL_Quit()`. `AudioOutput::open` uses `SDL_OpenAudioDeviceStream` (`audio_output.cpp:28`), which creates a *simplified* stream; SDL 3.x's `SDL_QuitAudio` destroys every simplified stream itself. `~MainWindow` then calls `AudioOutput::close` → `SDL_DestroyAudioStream(this->stream)` (`audio_output.cpp:47-54`) on freed memory.
- Trigger: exit with audio enabled. Heap corruption or crash during shutdown (may be silent because the process is exiting, but it can also lose the settings write that happens in the destructor path).
- Fix: put `Theme` and `MainWindow` in an inner scope so they are destroyed before `SDL_Quit()`.

---

## High

### H1. RAM viewer panics on a region backed by fewer bytes than advertised
- `supershuckie-core/src/memory_monitor.rs:713-727` `copy_mapped_prefix`: `available` correctly saturates to 0 when `offset > data.len()`, but the copy is `&data[offset..offset + n]`, which panics on `offset` beyond `data.len()` even when `n == 0`.
- Trigger: the GBA save-data region is declared 0x20000 long but `get_region(SaveData)` returns an empty or 8 KiB slice until a save type is detected. Open the RAM viewer at 0x0E004000 → core-thread panic → abort (C1).
- Fix: `data.get(offset..offset + n)` and return 0 on `None`.

### H2. A failed final flush loses the whole recording
- `supershuckie-frontend/src/lib.rs:1985-2005` `stop_recording_replay` discards the `bool` from `core.stop_recording_replay()` (the FIXME on line 1990 says so) and always deletes the temp file, which is the crash-safe copy.
- `supershuckie-replay-recorder/src/replay_file/record.rs:521-569, 602-631`: poisoning is sticky and `next_blob` clears `current_blob` before the sink writes succeed, so one failed write on *either* sink (for example the temp file on a full disk) makes `close()` unable to flush up to a full blob (15 min / 1 GiB of frames) to the healthy final sink. `record.rs:785-798` also ignores `flush()` errors in `truncate`/`overwrite_header`, which can leave stale raw packets appended after a blob.
- Fix: keep the temp file when close returns `false` and surface an error; keep the in-progress blob until both writes succeed; track poison per sink; propagate flush errors.

### H3. Poke-A-Byte: a SETUP with zero blocks crashes the server thread and poisons shared memory for the run
- `supershuckie-pokeabyte-integration/src/shared_memory/windows.c:17-43`: `CreateFileMappingA` returns `NULL` on failure, not `INVALID_HANDLE_VALUE`, so the check misses it; `MapViewOfFile` failure returns NULL without setting `*error`. `shared_memory.rs:30` then does `CStr::from_ptr(error)` on a null pointer.
- Trigger: any local UDP client sends SETUP with `block_count = 0` (`lib.rs:181-185` → `memory_size = 0`). Access violation in the server thread; even if survived, `handle` is now non-`INVALID_HANDLE_VALUE`, so every later SETUP fails with "shared memory already created" until restart.
- Related (M): `protocol.rs:434-456` only checks `offset + length` for `usize` overflow, so one packet can request an 8 GiB mapping that is created and `fill(0)`-ed while the core thread holds the session lock. Only macOS caps the size. Also the mapped view is never unmapped between sessions (`windows.c`, `linux.c:629`, `macos.c:698`), and `thread.rs:1008` lets the freeze table grow without bound.
- Fix: check for NULL, check `MapViewOfFile`, initialise `error` on the Rust side, reject `memory_size == 0`, cap the mapping (for example 64 MiB), unmap on close.

### H4. The REST server is on by default, unauthenticated, answers `Access-Control-Allow-Origin: *`, and mutates state on GET
- `supershuckie-frontend-webserver/src/lib.rs:324`, `supershuckie-frontend/src/settings.rs:480` (`enabled: true`), routes at `lib.rs:218-220`, `:2293`.
- Any web page the user visits can issue simple GETs to `127.0.0.1:30158` with no preflight and read the response: `/delete-bookmark`, `/update-bookmark` (rewrites the replay on disk), `/load-rom?path=C:\anything` (reads an arbitrary file, creates data dirs, closes the current game), `/load-replay`, `/set-paused`, `/set-playback-speed`.
- Also (L): `/load-replay?name=../../x` is not sanitised (`lib.rs:391, 1659, 1815`) and explicit names use `File::create`, silently overwriting existing replays and save states.
- Fix: reject requests carrying a browser `Origin` (or require a token), consider defaulting the server off, reject names containing separators or `..`.

### H5. mGBA: loading an empty save state dereferences a NULL VFile
- `mgba-rs/interface.cpp:208-213`: `VFileFromMemory(ptr, 0)` returns NULL (`vfs-mem.c:35`), and `mCoreLoadStateNamed(core, NULL, ..)` seeks through it.
- Trigger: a 0-byte `.save_state` file (interrupted write, disk full) or a 0-length keyframe blob in a corrupt replay. The frontend passes the bytes straight down (`lib.rs:376-377`). melonDS is safe here.
- Fix: return `false` when `data_size == 0` or `vf == nullptr`.

### H6. Memory tools panic on non-ASCII input to hex/pattern parsers
- `supershuckie-memory-tools/src/value.rs:488` (`parse_pattern`), `:468` (`parse_hex_bytes`), `table.rs:769` (`parse_tbl`) slice by byte offset after only checking that the byte length is even, so a multi-byte UTF-8 character straddling the boundary panics with "byte index is not a char boundary".
- Trigger: paste `ａａ` into the RAM search pattern field, `aéa` into the viewer paste-hex or a watch freeze value (also via the watch file's `freeze.value`), or a `.tbl` line `aéa=X`. Panic on the UI thread.
- Fix: reject non-ASCII up front, or iterate `as_bytes().chunks(2)`.

### H7. Video export: stderr tail slicing panics on the core thread
- `supershuckie-frontend/src/lib.rs:2538-2542` `read_stderr_tail` does `buf[start..]` at a fixed byte offset. If ffmpeg's stderr exceeds 4 KiB and byte 4096-from-the-end falls inside a multi-byte character (non-ASCII output path, localized messages), the core thread panics and the app aborts (C1).
- Fix: `floor_char_boundary` or trim on bytes and `from_utf8_lossy`.

### H8. Video export: ffmpeg pipes are captured but never drained, so a chatty ffmpeg deadlocks the export
- `supershuckie-frontend/src/lib.rs:2600-2602, 2653, 2535`: stdout and stderr are `Stdio::piped()` but nobody reads them until the error path. ffmpeg blocks on a full pipe, stops reading stdin, `write_all` blocks the core thread, and the cancel flag is never checked. `read_to_string` in the error path also blocks while ffmpeg is alive.
- Trigger: a `Custom` preset with `-loglevel info` or `-progress -`, or any preset that writes to stdout. Combined with M2, the UI freezes too.
- Fix: stdout to `Stdio::null()`, a reader thread with a bounded ring for stderr.

### H9. Bindings: `get_sram()` builds a slice from a NULL pointer, and mGBA leaks the clone each call
- `mgba-rs/src/lib.rs:84-88` + `interface.cpp:188-192`; `melonds-rs/src/lib.rs:95-99` + `interface.cpp:104-107`.
- `_GBACoreSavedataClone` sets `*sram = NULL` when the save size is 0 (any GBA game closed before it first touched its save); an NDS cart with no save memory does the same. `slice::from_raw_parts(null, 0)` is undefined behaviour and trips the debug-assertions precondition check. Separately, mGBA's clone is `malloc`ed and never freed: up to 128 KiB leaked per `save_sram()` (every close and every `SaveSRAM` command).
- Fix: return `&[]` on null/0; `free()` the clone after copying (or read via `getMemoryBlock`).

---

## Medium

### Core
- **M1. Rapid-fire duty cycle advances per `run()` call, not per frame** (`supershuckie-core/src/lib.rs:1120-1122`). On GB `run()` is one CPU step, on GBA/NDS the pacing loop returns NONE many times per frame, so "3 on / 3 off" is effectively random. Advance by `time.frames`.
- **M2. A running export blocks every other core command, including cancel and close** (`thread.rs:656-663, 520-531`; frontend callers `lib.rs:979, 529, 1002, 1999, 2026, 282/295`). `export_frames` runs inside `handle_command` and never polls; unpause, save state, load ROM, or a REST `/mark-start` freezes the UI for the whole export, and `Drop` blocks too. Refuse blocking operations while `current_export.is_some()`, or process commands inside the export loop.
- **M3. `attach_replay_player` failure leaves wrapper, core and frontend disagreeing** (`lib.rs:1173-1174`, `thread.rs:464-487`, frontend `lib.rs:411-470`). The core stops recording and detaches the old player before validating; on `Incompatible`/`MismatchedMetadata` the wrapper's `playback*` fields and the frontend's `current_replay`/`recording_replay_file` are stale, and the frontend has already paused, cleared bookmarks, set the BIOS override and *reinstantiated the core* (live progress since the last SRAM write is lost). Validate against the parsed header first; on error clear the wrapper fields.
- **M4. Frame skipping at 2x and above is defeated for paced cores** (`lib.rs:319`) by the same `mid_frame` conflation as C2. Performance only.
- **M5. `resume_recording_replay` failure drops the source player mid-transition** (`lib.rs:987-1008`); currently masked by the `expect` in C1, but must be cleaned up when that is fixed.

### Frontend
- **M6. Loading a replay or starting a recording while the other is active leaves stale bookkeeping** (`lib.rs:448-452, 1611`). The core silently closes the current recorder, but the frontend keeps `recording_replay_file`/`current_replay`, so memory tools think recording is on, the temp file leaks, and a later `stop_recording_replay` with `frames == 0` can delete the *final* file of the earlier recording. Call `stop_recording_replay()` / `stop_replay_playback()` first.
- **M7. REST commands run long after their client gave up; bursts execute all at once** (`webserver/src/lib.rs:24, 46, 62`; `lib.rs:1275`). One thread per request blocks up to 60 s waiting for `tick()`, which does not run while a modal dialog is open; up to 1024 queued commands (including `LoadROM`, `LoadReplay`, bookmark edits) then execute together. Drop commands whose reply sender is disconnected before applying side effects.
- **M8. `MemoryTools::core_switched` saves the outgoing ROM's watch file under the incoming ROM's identity** (`memory_tools.rs:503` vs `:516`). Next load of ROM A warns the watches were made for a different ROM. Save before reassigning `self.game`.
- **M9. NDS date leap-year logic is inverted** (`settings.rs:623-636`): February is capped at 29 for all years and clamped to 28 *in* leap years, so 2024-02-29 becomes 02-28 and 2023-02-29 reaches melonDS unchanged. Invert the condition.
- **M10. Unbounded growth from REST** (`lib.rs:1292-1296`; `bookmarks.rs:606-609, 748`): every unique `/increment-counter?name=` adds a counter and an unbounded-length packet; every unique `/add-bookmark?type=` appends a settings type and rewrites `settings.json` synchronously on the UI thread. Cap counts/lengths and coalesce `write_config` per tick.

### FFI (frontend-c) and Qt
- **M11. `supershuckie_frontend_load_rom` skips the load when `error_len == 0`** (`supershuckie-frontend-c/src/frontend.rs:136`): `if error_len > 0 && let Err(e) = frontend.load_rom(..)` short-circuits and returns `true`. The header explicitly allows `(NULL, 0)`. Latent because Qt always passes a buffer.
- **M12. Unnamed gamepad crashes on hot-plug** (`supershuckie-qt/src/sdl_event_wrapper.cpp:24-38`; `frontend.rs:1016`). `SDL_GetGamepadName` may return NULL; the C++ passes it to `snprintf("%s")`, `std::string`, and `supershuckie_frontend_connect_controller`, whose `CStr::from_ptr` has no null check. The same block opens the gamepad twice and never closes it on removal (leak per hot-plug).
- **M13. Frontend callbacks re-enter the frontend while Rust holds `&mut self` and the screens mutex** (`supershuckie-frontend/src/lib.rs:1365-1367, 2226-2228`; `frontend-c/src/frontend.rs:32-58`; `main_window.cpp:1970-1989`). `on_change_video_mode` calls five getters on the same object. Today they only read plain fields so nothing deadlocks, but it is an aliasing violation and any future getter that takes the screens lock deadlocks. Copy metadata out, drop the lock, then call back.
- **M14. Bookmark context menu uses a tree item that the 1 ms ticker can delete under it** (`bookmark_window.cpp:411-469`). `menu.exec()` does not stop the ticker; `rebuild()` calls `tree->clear()` when the bookmark generation changes (playback reaching the end, a REST-added bookmark). Choosing "Rename" then edits a freed item. Re-look up by id after `exec()`, or stop the timer around the menu.
- **M15. `argv[1]` is ANSI on MinGW but decoded as UTF-8** (`main.cpp:53-55`, `main_window.cpp:1316-1318`). "Open with" on a path containing `é` throws `filesystem_error` out of `load_rom`, uncaught in `main` → `std::terminate`. Use `QCoreApplication::arguments()`.
- **M16. Controller settings show axis bindings from the button list** (`controller_settings_window.cpp:291-309`): the buffer is sized with `is_axis = true` and filled with `false`. Wrong data only.

### Replay recorder
- **M17. `ReplayHeaderRaw` has `bool` fields and is produced by transmuting raw file bytes** (`replay_file/header.rs:353-357, 470-476`). A byte other than 0/1 at offset 0x0C/0x0D is an invalid `bool` (the comment claiming otherwise is wrong). Store as `u8`.
- **M18. Replay converter's in==out guard is a lexical path compare** (`replay_file/convert.rs:186-206`). `convert foo.replay ./foo.replay --force` (or a case-variant on a case-insensitive FS, or a symlink) truncates the memory-mapped source and, on failure, deletes it. Compare canonical paths / file identity and write via temp + rename.
- **M19. Recorder thread death is silent** (`record/thread.rs:83-96, 171-218`): a panic or poisoned lock exits `run()` with nothing on the error channel, `poll_errors` reports nothing, and `free_buffers` is an unbounded channel that grows by one ~19 MiB NDS state per keyframe if the producer stops draining. Report thread death; bound the channel.

### Bindings and tools
- **M20. mGBA init failures call `std::terminate()`** (`mgba-rs/interface.cpp:67-126`): a user-supplied BIOS mGBA rejects prints "Bad BIOS" and kills the app instead of returning `nullptr`. melonDS's constructor can also throw across `extern "C"`.
- **M21. `create_save_state` reports a size it did not write** (`mgba-rs/interface.cpp:196-206`, `src/lib.rs:102-108`): `size` is returned even when `read_successfully` is false or `size > data_size`, and Rust `set_len`s to it. Not triggerable with today's mem-chunk VFile, but one refactor from exposing uninitialised bytes as a keyframe.
- **M22. `load_save_state`/`create_save_state` mutate through `&self` on `Sync` cores** (`melonds-rs/src/lib.rs:56-57, 128-130`; `mgba-rs/src/lib.rs:50-51, 112-114`) while `get_main_ram(&self) -> &[u8]` can be alive. API unsoundness, no current caller triggers it. Take `&mut self`, drop `Sync`.

---

## Low

### Core
- L1. `game_boy_color.rs:445-448` indexes `input[0]` on a possibly-empty slice from a replay packet (only `debug_assert`ed); GBA and NDS handle it. Use `input.first()`.
- L2. `export.rs:180-239`: a full export from frame 0 emits one frame too few (progress never hits 100%); a mid-loop `replay_stalled` produces a truncated video reported as success; the empty-range branch skips `abort()` on `begin`/`finish` failure.
- L3. `lib.rs:1193` reports the BIOS hash as `loaded` in `ROMChecksumMismatch`.
- L4. `thread.rs:539-544`: a timed-out `bookmark_anchor` still writes an orphan full keyframe later; retries write more.
- L5. `thread.rs:854-866`: a seek and a step in the same loop iteration drop the step.
- L6. `lib.rs:678`, `thread.rs:1006, 1009`: replay `WriteMemory` and Poke-A-Byte addresses are truncated `u64 -> u32`, aliasing low addresses instead of failing.
- L7. `game_boy_color.rs:480-482`: hard-coded SGB save-state offset `state[0x1AB66]` without a bounds check; a SameBoy state-layout change panics every SGB2 hard reset.
- L8. `lib.rs:901`: `expect("NO CONSOLE_TYPE...")` reachable via the public `start_recording_replay` on a null core (the frontend guards it).

### Frontend / webserver
- L9. `lib.rs:472, 482-495`: `save_file` stays `"replay"` after playback stops, so live play after a replay saves to `replay.sav`. Intent unclear.
- L10. `lib.rs:2023-2028` sleeps up to 5 s on the UI thread in `continue_last_replay`; `bookmarks.rs:459` waits up to 10 s per replay switch; `bookmarks.rs:290-300` `reset` does not clear `in_flight`, so after a timeout the next replay's edits are never auto-saved.
- L11. `lib.rs:1102-1105` `set_jit_enabled` leaves the game paused.
- L12. `settings.rs:158`, `lib.rs:743-744`: `max_recent_roms` is never enforced.
- L13. `settings.rs:196-197`: `zstd_compression_level` and speed multipliers are only clamped by setters, not on load; an out-of-range level from the file reaches zstd → recorder creation error → C1.
- L14. `lib.rs:1302-1339`: per-tick REST caches (`stats`, `replays`) are not invalidated after `LoadReplay`/`GoToFrame`/`LoadROM` in the same tick.
- L15. `lib.rs:1327-1335`: `/set-playback-speed` bypasses the "disable speed changes when recording / ignore in replays" settings and accepts `speed=0`.
- L16. `webserver/src/lib.rs:303-334`: disable→enable in quick succession can fail to bind because tiny_http closes the listener asynchronously; no retry.
- L17. Docs/messages: `docs/external_commands.md:311` `mark-end` text is copied from `mark-start`; `/set-paused` missing-param error says "frame"; `/load-rom` missing-param response lacks the CORS header; unknown routes return 400 not 404. `docs/frame_server.md`'s `dup2(2, 1)` claim does not hold for Rust's stdout on Windows.

### FFI (frontend-c)
- L18. `frontend.rs:633, 651`: `is_pokeabyte_enabled` / `get_external_commands_enabled` write `*error = 0` unconditionally (null write or 1-byte overflow with `(buf, 0)`).
- L19. ~16 exports build `from_raw_parts_mut(ptr, 0)` with no null check (`frontend.rs:120, 267, 290, 305, ...`), which is UB even at length 0; the header allows `(NULL, 0)` for `load_replay` and `continue_last_replay`. Route through the guarded `write_error` helper.
- L20. Header/impl mismatches: `frontend.h:918` declares `clear_recent_roms` returning a string array, Rust returns nothing; several const-ness mismatches (h:180, 349, 370, 380, 631); `is_sgb_enabled` declared twice (h:112, 133); `get/set_auto_decompress_replays_upfront_setting` exported but not in the header; `get_current_save_file` returns a non-NUL-terminated pointer undocumented.
- L21. `control_settings.rs:108-118, 150-151`, `frontend.rs:990, 1000`: `panic!` on unknown control/modifier/emulator ids or non-UTF-8 device names, where sibling getters return null/false.
- L22. `memory.rs:426`: pattern length is truncated to `u8` before validation, so a 300-char pattern gives a confusing "pattern is 300 bytes but value is 44" error.

### Qt
- L23. `replay_playback_controls.cpp:113-120, 169-181`: `total_frames` is read uninitialised when no replay is playing (bar is hidden, so harmless today).
- L24. `render_widget.cpp:212-228`: touch hit-test uses `>` instead of `>=`, then narrows to `uint8_t`; one pixel past the right edge sends x = 0.
- L25. `main_window.cpp:293-296`, `memory_tools_controller.cpp:781-826`: `get_custom_setting` pointers (documented as invalid after any further API call) are held across whole window constructions. Latent.
- L26. `main_window.cpp:435-437, 1285-1295`: error boxes and the open-ROM dialog are parentless and shown without `stop_timer()`, so the ticker re-enters under them.
- L27. `watch_edit_dialog.cpp:254-265`: region combo index captured at fill time can exceed `regions().size()` if an external command switches ROMs while the dialog is open.
- L28. `file_rw.cpp`: dead code that would misdecode UTF-8 paths via `fopen` on Windows if ever used.

### Replay recorder
- L29. `keyframe_masks.rs:96-100`: `nds_transient_range` checks 4 bytes then indexes `state[4]`, `state[5]`; a 4-5 byte `MELN` state panics (reachable from verify, re-encode, resume).
- L30. Spec drift and small robustness items: `bookmark_section.rs:204-235` compares the header byte-exactly (spec §5.5 says ignore version/stream end; `same_replay_as` exists unused) and never checks the stream parses to `file_len` before closing a v3/v4 file (§5.7); `resume.rs:215-234` leaves the source's untruncated bookmark table as the final sink's newest snapshot until the first blob flushes (§7.2); `record.rs:452-476` stores fallback full keyframes masked; `resume.rs:224-227` can drop packets between two same-frame keyframes at a blob start; unchecked `+=` on file-derived counts at `playback.rs:372-373`, `resume.rs:379`, `bookmarks.rs:177, 187, 326`, `bookmark_section.rs:217`; `util.rs:141-145` reserves `uncompressed_size` from the header before checking it against the zstd frame.

### Tools
- L31. `supershuckie-memory-tools/src/search.rs:388-423`: the dense bitset is sized `len / alignment` but `end_slot` can be one more, so a region of length 129/257/513 with alignment 2 indexes past `words`. No current console region has such a length.

---

## Test and build hygiene

- **T1.** `cargo test --workspace` cannot link the `supershuckie-core` and `supershuckie-frontend` test binaries (missing `libcore.a`, `libteakra.a`, C++ runtime), so their 33 unit tests never run outside CMake. The frame-server's `build.rs` already knows how to find the CMake outputs via `SUPERSHUCKIE_BUILD_DIR`; reusing that in `melonds-rs`/`mgba-rs` build scripts (or a dev-only feature) would make the whole suite runnable and CI-able.
- **T2.** `supershuckie-pokeabyte-integration` has no tests at all despite parsing untrusted UDP input; H3 and the 8 GiB mapping would be caught by a handful of malformed-packet tests like the frame server's `bad_input_is_an_error_not_a_panic`.
- **T3.** There is no fuzz or corrupt-input test for the replay parser beyond truncation; C5, L29 and M17 are all one-byte corruptions.

## Areas checked and found sound

- Audio ring buffer (overflow drops oldest, underrun partial reads, poisoned-mutex recovery); the SDL audio callback only touches the ring.
- Atomic orderings, lock ordering (core thread only `try_lock`s shared mutexes), `park`/`unpark` wakeups.
- Frame-server protocol: every length bounds-checked, 1 MiB cap, clean errors, broken-pipe handling.
- Poke-A-Byte packet parsing apart from the size cap: block count, address overflow, sender authorisation, socket read timeout.
- Memory-tools region math, watch parsing, pointer-depth cap, brute-force property tests.
- melonDS save-state load/save bounds, audio buffer sizing, JIT invalidation ranges, RTC clamping, no shared static state between instances.
- Replay format: varint/diff/region-diff encode-decode symmetry, keyframe lookup, blob boundaries, verbatim blob copy offsets, bookmark section round-trip, close ordering.
- FFI allocator pairing (Box/CString/Arc) and every borrowed-pointer lifetime at its C++ call site; `#[repr(C)]` layouts match the headers.
- Qt: string-array and JSON ownership, buffer sizes, hex viewer 64-bit range math, tool-window parenting, shortcut uniqueness, settings restore with `ok` checks, action enablement vs FFI preconditions.

## Not assessed

- Whether the `&mut` aliasing in M13 actually miscompiles under the shipped PGO+LTO build.
- Internals of the vendored cores beyond the code paths the bindings exercise.
- tiny_http request-line and header size limits.
- Runtime confirmation of C6 against the packaged SDL 3.4.8 binary (verified against upstream SDL 3 source, where `SDL_QuitAudio` has destroyed simplified streams since 3.0).

## Suggested fix order

1. C1 + C2 + H1 + H7 together: make the core thread unable to die (propagate `Result`, fix `mid_frame`, fix the recorder assert and poisoned-mutex close). This one change removes the largest class of aborts.
2. C4 and H2: atomic settings write with fallback; keep the temp replay on failed close.
3. C3, H5, H9: harden the mGBA shim (pad saves, reject empty states, null-safe SRAM, free the clone).
4. C5 and L29/M17: bound allocations in the replay parser, replace the decompression spin, fix the `bool` transmute.
5. C6, M12, M14, M15: the four Qt crashes.
6. H3, H4, H6, H8, then the mediums.

---

## Status (2026-09-17)

Every finding above was addressed in the working tree on `replay-bookmarks` (uncommitted). The work was done by Sonnet sub-agents in gated waves (leaf crates, then core, frontend, C ABI, Qt), verified per crate, then re-reviewed by two independent agents whose five small regressions were fixed by hand. `cargo test --workspace` passes 177 tests (was 92; the 33 core/frontend tests were previously unrunnable). `build/supershuckie.exe` links.

| Group | Status | Notes |
|---|---|---|
| C1–C6 | Fixed | C1: `Result` replies + `is_alive()`/`CoreThreadDead` on the thread wrapper, `new_from_rom -> Result`. C2: `EmulatorCore::is_mid_frame()` (GB only) + paused-clock timestamps; recorder assert -> `BadInput`; regression test `paused_save_state_and_bookmark_do_not_break_monotone_timestamps`. C3: `VFileMemChunk`. C4: tmp+rename, `.bad` fallback, `clamp()` on load. C5: bounded `with_capacity`, channel-based decompression worker with dead-worker fallback. C6: window destroyed before `SDL_Quit()`. |
| H1–H9 | Fixed | H4 reduced to name sanitising + `create_new` by user decision (server access, CORS and GET side effects unchanged). H2: per-sink failure tracking, `TempSink` non-fatal, temp file kept on a failed close. H8: stdout null, stderr reader thread. H9: mGBA clone freed through `mgba_rs_core_free_sram_clone`. |
| M1–M22 | Fixed | M2 by refusing blocking operations during export plus a cancel-on-drop safety net (export loop unchanged). M13: callback runs after the screens lock is released; `refresh_screens` documented as non-re-entrant. M22: `Sync` removed and `load_save_state(&mut self)`; `create_save_state` deliberately stays `&self`. |
| L1–L31 | Fixed | L9: save file restored after playback (user decision). L26 broadened: every `DISPLAY_ERROR_DIALOG` in the app now goes through `MainWindow::show_error` (parented, timer-guarded). L30(b),(c),(e): spec amendments rather than code, as planned. |
| T1–T3 | Fixed | T1: `link-cores` dev-dependency feature (`SUPERSHUCKIE_BUILD_DIR`, default `build/`). T2: four malformed-packet tests. T3: `corruption_tests.rs` sweeps header, stream and in-blob bytes (~8 s). |

Residual notes from the review pass, all judged acceptable:
- `write_bookmark_section` still relies on callers refusing truncated replays (the frontend does; spec §5.7 now says so).
- `resolve_type`'s by-id branch is not capped at 256 bookmark types (only the by-name branch and `upsert_bookmark_type` are).
- A settings write that fails inside `tick()` is retried on the next change, not every tick.
- The `&mut self` aliasing across the `change_video_mode` callback is documented rather than eliminated.

Manual checks not yet run by anyone (see the plan's verification list): paused save state during a GBA recording, a 64 KiB Emerald save, a truncated `settings.json`, a full-disk recording start, a hand-corrupted replay, exit with audio on, controller hot-plug, bookmark rename after playback ends, "Open with" on a non-ASCII path.
