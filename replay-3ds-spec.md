# Replay format for the Nintendo 3DS core (Azahar) — design, 2026-09-25

Goal: replays of 3DS games (Pokémon X/Y/OR/AS/S/M/US/UM through Azahar) that fit **5 GB for
3 hours** and have a **scrubbable timeline**. Priority order: timeline first, size second, but
5 GB per 3 h is a hard ceiling.

Everything below is measured on Pokémon X in the overworld with continuous walking + dialogue
(`azahar_spike --mash`), New 3DS mode, RTX 3080 Ti, with the three Azahar patches in
`azahar-rs/patches/` applied. The spike, its flags and raw numbers: `azahar-3ds-spike-report.md`,
`azahar-spike-results/x-delta/*.txt`.

## 1. What makes this different from the DS

| | melonDS (DS) | Azahar (3DS) |
|---|---|---|
| Raw state | 18.8 MiB | 308 MB New 3DS mode / 170 MB Old 3DS mode (all of FCRAM is serialised) |
| Save / load a raw state | ms | 80 / 230 ms New 3DS, 70 / 200 ms Old 3DS (`SaveStateRaw`/`LoadStateRaw`, patch 0002; the load is dominated by Azahar's kernel and GPU reload, not FCRAM size) |
| Bytes that change between keyframes | a few hundred KB per 2 s | **30–60 MB per keyframe, about the same at 2 s, 10 s and 30 s** (heap 5–7 MB, linear heap 17–26 MB, other FCRAM 5 MB, VRAM/DSP/GPU 3–10 MB) |
| One keyframe delta, zstd on its own | ~1 MiB | 10–13 MB (as big as a full state) |
| One keyframe delta with the previous keyframe's delta as reference | — | **1.6 MB at 2 s, 2.5 MB at 6 s, 3.2 MB at 10 s, 4.2 MB at 30 s** |
| Transient-buffer masks | GP3D masked, cosmetic only | **not possible**: a state with the linear heap taken from 2 s earlier crashes the game (undefined instruction in the main thread: the game keeps code and data it reads there) |
| Re-emulation speed for seeks | ~450 fps | 336–423 fps drawing; **738 fps with drawing skipped** (patch 0003; heap identical) |
| Determinism | proven | proven across processes, from power-on and from loaded states, **only** with the pinned settings + patch 0001 and the JIT (the interpreter diverges from the JIT in gameplay) |

The single most important measurement: the bytes that change are largely *the same bytes moving
around* (double-buffered command lists, vertex data, streamed assets), so a keyframe compressed
with zstd's 128 MiB window and long-distance matching and the **previous keyframe's payload as a
reference prefix** is 7x smaller than the same keyframe compressed alone. Reference to two
keyframes back adds only 5 % more. Cross-keyframe context is where all the size comes from, and it
is available on both sides: the recorder holds the previous payload, and a player materialising
keyframe k has just produced payload k−1.

## 2. Budget arithmetic

3 hours = 648,000 frames. Per-keyframe cost is the reference-prefixed size above.

| Keyframe cadence | Keyframes / 3 h | Keyframes alone | Worst exact seek (re-emulate at 738 fps) |
|---|---|---|---|
| 2 s | 5,400 | 8.6 GB | 0.16 s |
| 4 s | 2,700 | ~5.7 GB | 0.33 s |
| 6 s | 1,800 | 4.5 GB | 0.49 s |
| **8 s** | **1,350** | **~3.9 GB** | **0.65 s** |
| 10 s | 1,080 | 3.4 GB | 0.81 s |
| 30 s | 360 | 1.5 GB | 2.4 s |

On top of keyframes: one full state per blob (14 MB compressed) and thumbnails (§3.4, ~100 MB).
**8 s (480 frames) is the densest cadence that leaves margin under 5 GB** on this workload; the
recorder also gets a byte-rate governor (§3.6) because this was measured on 60–90 s of one game
and a 3-hour session with battles, menus and area changes will differ.

Seek latency has three parts: materialising the keyframe state from the file (§3.3), loading it
into Azahar (230 ms New 3DS / 200 ms Old 3DS), and re-emulating from the keyframe to the exact
frame (≤ 0.65 s at 8 s cadence, average half that). Expect **about 1 s for a cold exact seek, 0.6 s
warm**, against 0.35 s on the DS. The timeline stays responsive because dragging never seeks (§3.4).

## 3. Design

### 3.1 Console type, state and settings

- `ReplayConsoleType::Nintendo3DS`. The keyframe state is Azahar's raw serialisation
  (`SaveStateRaw`), 308 MB (New 3DS) or 170 MB (Old 3DS). Old 3DS mode is the default for games
  that support it (all of gen 6/7): it halves the state and the memory of every buffer below and
  changes nothing about delta sizes (unused FCRAM is zero; measured 2.5 MB per 6-s keyframe in
  both modes). It does not buy much time: raw load 200 ms vs 230 ms, save 70 ms vs 80 ms.
- The core pins what determinism needs and the header records it: fixed init clock + init ticks,
  `deterministic_async_operations`, synchronous FS, JIT on, audio stretching off, static mic, no
  async shaders, plus the Azahar revision (the state format is tied to it). A replay whose header
  disagrees is refused, like a BIOS mismatch today.
- The clock's start date and the system language are user settings (Settings › Nintendo 3DS),
  applied when the core starts. Both are part of every state (patch 0008 serialises the clock
  start; the language lives in the CFG block the state already carried), so a replay plays with
  the date and language it was recorded with whatever the settings are now, and the console time
  no longer depends on the host's time zone.
- Sync hashes (Play Together, `--verify`) hash the process heap walked from the kernel's VMA map,
  never whole FCRAM (kernel-owned regions flip between runs even when the game is identical).

### 3.2 Keyframes: reference-prefixed region deltas, two levels

Format v7 adds one packet kind:

```
RefDeltaKeyframe {
    metadata: KeyframeMetadata,   // as today
    level: u8,                    // 1 = segment keyframe, 2 = fine keyframe
    ref_kind: u8,                 // 0 = the blob's full keyframe, 1 = previous keyframe of this level,
                                  // 2 = the level-1 keyframe this segment starts with
    state_len: u64,
    frame: bytes                  // one zstd frame: region_diff(reference state, this state)
}                                 //   (control ‖ data), windowLog 27, LDM, compressed with
                                  //   refPrefix = the reference keyframe's own (control ‖ data)
```

- The delta codec is the existing `region_diff` (4-byte granularity, LEB128 control, raw data).
- Each keyframe is its own zstd frame, so a keyframe can be read without decoding everything
  before it in the blob; the v6 per-keyframe offset table now holds compressed byte offsets.
- The reference prefix for decompression is the reference keyframe's *payload* (control ‖ data),
  which the player has just decoded when it materialises the chain in order. The blob's full
  keyframe has no payload, so level-1 keyframes referencing it use no prefix (costs ~11 MB, once
  per blob).
- Levels bound the chain a seek has to walk. Blob = 30 min as today. **Level 1 every 2 min**
  (diff vs the previous level-1 state, or the full keyframe), **level 2 every 8 s** (diff vs the
  previous level-2 state, or the level-1 state that starts its segment). Worst chain to any
  keyframe: 14 level-1 + 14 level-2 = 28 applies; average 14.
- Estimated storage per 30 min: 14 MB full + 14 × ~5 MB level-1 + 210 × 2.9 MB level-2 ≈
  700 MB → **4.2 GB per 3 h** before thumbnails. (Level-1 keyframes at 2 min were not measured
  directly; 30 s cost 4.2 MB and the change set saturates, so ~5 MB is the expectation. Verify.)

### 3.3 Player: materialising a keyframe

`state(k)` = full keyframe of the blob, then apply level-1 deltas up to k's segment, then
level-2 deltas up to k. One apply = zstd-decompress with the reference payload as prefix
(≈50 MB out, 25–40 ms) + `apply_region_diff_in_place` (~10 ms). The player keeps three
materialised states per attached replay, each with its payload: the blob's full keyframe, the
last level-1 state, the last level-2 state (3 × 170 MB in Old 3DS mode). A seek inside the current
2-minute segment therefore applies ≤ 14 deltas (≤ 0.5 s, typically 0.2 s); a jump to another
segment re-walks from the full keyframe (≤ 1.1 s). Scrubbing back and forth near one spot costs
one level-2 chain per step, not a full walk.

The core's seek is then `LoadStateRaw(state)` + `run_unlocked_hidden` with `g_skip_drawing` set
until the target frame, then one drawn frame. No post-load mask frames are needed (nothing is
masked).

### 3.4 Timeline: thumbnails while dragging, exact seek on release

The v6 behaviour (snap to a keyframe and show the emulator while dragging) would cost ~0.5 s per
drag step here. Instead the file carries a preview strip:

```
Thumbnail { frame: u64, elapsed_millis, top: bytes, bottom: bytes }   // every 60 frames
```

top 200×120 and bottom 160×120, RGB565, zstd (≈ 8–12 KB per pair; ~110 MB per 3 h). While the
timeline is dragged the game view shows the nearest thumbnail scaled up, with no core involvement;
on release the core does the exact seek (§3.3) and the live picture replaces the preview. The
same packet is useful for every console (a 3-hour DS replay would gain ~100 MB) and for the
bookmark list, but only the 3DS core needs it to feel responsive. Captured on the core thread from
the frame it already has (a 400×240 → 200×120 box filter is ~0.2 ms); encoded on the recorder's
worker (§3.5).

### 3.5 Recorder pipeline

The DS pipeline (state → diff → packet, all on the core thread, blob compressed at close) cannot
carry 170–308 MB states. For the 3DS core:

1. Core thread, every 480 frames: `SaveStateRaw` into a pooled buffer (80 ms New 3DS, 70 ms Old
   3DS; a real stall at 1x, 4–5 dropped frames every 8 s, the price of Azahar serialising the
   whole system) and hand it to the keyframe worker with the metadata.
2. Keyframe worker (one thread per recording): `region_diff` against the reference state
   (60–130 ms), zstd with the reference payload as prefix (~100 ms), write the packet through the
   existing sink. Keeps: reference states for level 1 and level 2, their payloads, a pool of three
   raw buffers. Memory ≈ 5 states (0.85 GB Old 3DS / 1.5 GB New 3DS) — another argument for Old
   3DS mode.
3. Blobs are no longer compressed at close (each keyframe is already a zstd frame); the inputs and
   other small packets between keyframes are compressed per blob as today. `NextFrame`/input
   packets stay on the core thread.
4. Keyframe ordering: the worker consumes in order; a seek that lands on a keyframe still in the
   worker's queue waits for it (same rule as today's "keyframe not yet written").

### 3.6 Byte-rate governor

The recorder tracks compressed keyframe bytes per hour. Above 1.4 GB/h it stretches the level-2
cadence (480 → 600 → 720 → 960 frames) until the rate is back under; below 1.0 GB/h it shortens
again down to 480. Cadence changes take effect at the next level-1 boundary and are recorded in
each keyframe's metadata (the player never assumes a fixed interval; it never did). This keeps a
3-hour file under 5 GB on workloads heavier than the one measured, at the cost of seek time in
those stretches.

### 3.7 Inputs

3DS input is not a 12-button bitfield: circle pad and C-stick are analog, ZL/ZR exist, touch is
320×240. `Input` grows `circle: (i16, i16)`, `c_stick: (i16, i16)`, `zl`, `zr`, and touch becomes
`(u16, u16)`; the replay `ChangeInput` packet gets a v7 encoding with those fields (older consoles
keep writing the old encoding; `encode_input` is per core already). Out of scope for this
document beyond noting it is on the path.

## 4. What this costs and what it buys

| | DS today | 3DS with this design |
|---|---|---|
| 3-hour file | ~150 MiB | ~4.3 GB (keyframes 4.2 GB, thumbnails 0.1 GB) |
| Drag step | ~70 ms (live emulator) | instant (thumbnail) |
| Exact seek on release | ~0.35 s | ~0.6 s warm, ~1 s cold, ≤ 2 s worst |
| Keyframe granularity | 2 s | 8 s (governor may stretch to 16 s) |
| Recording stall per keyframe | ~ms | 70–80 ms every 8 s |
| RAM while recording / playing | ~100 MiB | ~1 GB / ~0.6 GB (Old 3DS mode) |

Why 8 s and not 2 s: at 2 s the keyframes alone are 8.6 GB. The information content of the
change set (1.6 MB per 2 s after every trick above) is the floor; no codec change was found that
beats reference-prefixed zstd by more than a few percent (level 9: −2 %, level 19 untested but the
DS data says −30 % for 20x the CPU, which the worker could afford — worth one experiment).

## 5. Verification when implemented

- Record 3 hours of real play (not the mash script) and check: file size, bytes/hour trace, the
  governor's cadence changes, RAM.
- Seek latency distribution with the `seek_e2e`-style bench: cold, warm, within-segment, across
  blobs; and that every seek reproduces the live heap hash (`--verify` equivalent).
- Recording pacing (`record_pacing_smoke`) with the 70–80 ms keyframe stall at 1x and at 4x.
- Thumbnails: drag across blob boundaries, release, bookmark list previews.
- Determinism across two machines (Play Together) with the heap-based sync hash.

## 6. Implementation order

1. `azahar-rs` crate + `EmulatorCore` for the 3DS (GL context per core thread, raw state API,
   skip-drawing in `run_unlocked_hidden`, input factories, heap sync hash) — everything below
   needs a core to record from. Azahar as a git submodule with the three patches applied by the
   build script.
2. Recorder: `RefDeltaKeyframe`, per-keyframe zstd frames with reference prefixes, two levels,
   keyframe worker thread, compressed offsets in the keyframe table (format v7).
3. Player: chain materialisation with the three cached states; core seek with skip-drawing.
4. Thumbnails: packet, capture, encode, timeline preview in the frontend.
5. Governor, header fields for the pinned Azahar settings, `--verify` for 3DS in `nds_bench`'s
   successor.

## 7. Revision after the first end-to-end run (2026-09-25, later)

Built so far: the `azahar-rs` crate and `Nintendo3DS` core (`supershuckie-core/src/emulator/nintendo_3ds.rs`),
the 3DS console type in the recorder, the `Input` extension (circle pad, C-stick, ZL/ZR, 16-bit touch),
the 480-frame keyframe cadence for 3DS recordings, the merged `libazahar.a` linked into the Qt app,
and a length-tolerant region delta (`region_diff_resizing`, 3DS files only: Azahar's raw states
vary by a few bytes between keyframes and every such change used to force a 170 MB full keyframe).
`supershuckie-core/examples/n3ds_smoke.rs` records, plays back and seeks on Pokémon X through the
existing v7 machinery: playback reproduces the recorded heap, seeks are reproducible, keyframe
stalls are 75–130 ms, and 40 s of title screen came to 22 MB (one full keyframe plus 1.8 MB per
8-s delta).

Two things the v6/v7 machinery cannot do for this core, which is why §3.2/§3.3 change:

- **Blobs buffer their packets uncompressed until they close** (`next_blob` compresses on the
  core thread). A 3DS keyframe is a 44 MB payload; a 3-minute blob is 1 GB of RAM and a 2–3 s
  freeze at every rollover.
- **The player holds every top-level packet, including blob bytes, in RAM** (the file is read
  whole and parsed into `Vec<Packet>`). A 4 GB file would need 4–8 GB.

So a 3DS file (header version 8; DS/GBA/GB files keep writing 7 and are unchanged) has **no
blobs**. Its stream holds the small packets as usual plus `StoredKeyframe { metadata, level,
state_len, uncompressed_len, frame_len }` immediately followed by `frame_len` bytes: one zstd
frame per keyframe (level 0 = the full state, ≈13 MB; levels 1–2 = the region delta against the
most recent keyframe of a lower-or-equal level, compressed with that reference's payload as
prefix). The parser records where each frame sits and skips over it; frames are read on demand
from the file, memory-mapped by the app. The player keeps three states (one per level) and
folds only the chain a seek needs. Small packets are written straight to both sinks as they
happen, so nothing is buffered and nothing compresses at rollover. Keyframe encoding moves to a
worker thread (§3.5); the core thread only serialises (70 ms) and hands the buffer over.

Thumbnails stay as stream packets (about 110 MB per 3 h in RAM when open, acceptable).

### 7.1 Status (end of 2026-09-25)

Implemented and verified headless on Pokémon X (`supershuckie-core/examples/n3ds_smoke.rs`,
`supershuckie-replay-recorder/examples/replay_probe.rs`):

- `StoredKeyframe` (0xFB) frames read on demand, levels (settings `stored_keyframe_levels`,
  default 15/15), reference-prefixed zstd, no blobs, header version 8 for 3DS files only.
  Recorder tests: 95, DS fixtures byte-identical.
- Player: three level states, level-aware seeks forward and backward, keyframes materialised
  only on seeks (plain playback pays nothing per keyframe), `new_shared` for a memory-mapped file
  (the app maps 3DS files through `open_replay_player`).
- The recorder already runs on its own thread (`NonBlockingReplayFileRecorder`), so §3.5's
  worker exists: diff + zstd never touch the core thread. The core-thread cost per keyframe is
  the raw serialisation into a recycled buffer: 67–70 ms (was 120 ms; the first keyframe 100 ms).
  Stopping a recording is instant (31 ms; was 1.3 s of blob compression).
- `Thumbnail` (0xFC) packets once a second (200×120 + 160×120 RGB565, zstd; ~30 KB a pair on the
  title screen, so ~320 MB per 3 h — more than §3.4 guessed; a cheaper encoding is an option),
  captured in the core, served by the player, and shown by the core thread on the normal screen
  path while the timeline is frozen (dragged) instead of any seek; the exact seek happens on
  release as before. No Qt change was needed.
- Measured on the 40-second title-screen recording: 28.5 MB (one 13 MB full keyframe + five
  8-s deltas of ~2.8 MB + thumbnails); exact seeks 0.8–1.5 s through the core (chain
  materialisation 0.1–0.5 s + 200 ms state load + re-emulation).

Not done: a 3-hour real session (file size, RAM, governor), the byte-rate governor (§3.6),
carrying thumbnails across a resume, the analog input UI, audio, any in-app hand check.

### 7.2 The keyframe stall (2026-09-25, later): under a frame

The 67-70 ms on the core thread per keyframe was Azahar's serialisation, not memory. Timed
component by component (`SUPERSHUCKIE_SERIALIZE_TIMING=1`, patch 0006), a warm 170 MB save was:
page table 60 ms (2^20 tracked `MemoryRef`s, reached first through the CPU core's page table
pointer), HLE services 8 ms (of which the process handle table's 4096 slots 4.5 ms and the
program binary in `CodeSet` 3.9 ms), FCRAM + VRAM copy 11 ms, GPU flush 2 ms, rest under 2 ms.

What changed (all in `azahar-rs/patches`, our builds only):

- Page table as runs of mapped pages, attributes as one block (0004): 60 ms -> 1 ms.
- Handle table writes only occupied slots; program binaries in a registry keyed by content hash,
  a state stores the key (0005): 8 ms -> 2 ms, raw state 170 -> 150 MB. A state only loads in a
  process that has loaded the same game, which the replay player guarantees.
- RAM regions in one write-watched host block (`VirtualAlloc` `MEM_WRITE_WATCH`, `GetWriteWatch`),
  kept across state loads; the raw state is `[32-byte header][RAM regions][boost archive]` and a
  save into a buffer that still holds a recent state of the process copies only the pages written
  since, on a few threads (0004, 0002). The recorder's recycled keyframe buffers are exactly such
  buffers (the core hands them back with their contents; `save_state_into` no longer clears
  them). Measured in gameplay: ~1100 pages (4 MB) change per 8-second keyframe; the copy is
  1 ms instead of 11. The incremental state is byte-identical to a full one
  (`azahar_spike --dirty-bench`, 10 of 10) and loads (`fcram_match=1`). Full copies happen only
  into fresh buffers, which are now made and page-touched on a helper thread when a 3DS recording
  starts (`prime_state_buffers`), since the first write into a fresh 150 MB allocation was
  40-60 ms of page faults.

Warm keyframe on the core thread now (Pokémon X, Old 3DS, `n3ds_smoke`, OBS and the app still
running at 33% CPU): GPU flush 1.8 ms (5 surfaces, 1.3 MB, the readback wait), services 2.3 ms
(boost over ~16 sessions and the objects behind them), DSP 1.2 ms, CPU cores 1.3 ms (page runs
scan 0.7), RAM copy 1.1 ms, rest 0.5 ms: **~8 ms**, and the smoke's frame times show nothing at
the keyframe frames any more (the stalls it lists are elsewhere). The frame after a keyframe
costs ~8 ms more than usual because Azahar drops its rasterizer cache when it saves (every
texture and surface is uploaded again); keeping the cache instead was tried and reverted: the
saved page table then lacks the cached pages' mapping (crash on load), and the flush of a
long-lived cache was 25 ms rather than 2. Both frames stay under 16.7 ms.

Not done, in order of value if more is wanted: cache the page-table runs (0.7 ms), the boost cost
of services/DSP (~3.5 ms, needs a different serialiser), an asynchronous readback for the flush
(1-3 ms of GPU wait). On other platforms than Windows every keyframe copies all RAM (parallel
memcpy, ~10 ms) until a dirty-page source exists there (soft-dirty bits, `userfaultfd`).
Loading a raw state is now ~90 ms (was 150-250): the block is reused across loads, so its pages
stay resident; the page-table rebuild (2^20 `MemoryRef` assignments, ~20 ms) is the next lever
for seeks.

### 7.3 First runs in the app (2026-09-25, evening)

Three things the headless smoke could not see, all fixed:

- The core is made on the UI thread and run on the core thread; the WGL context stayed on the
  UI thread and every GL call on the core thread failed until the shader-cache loader threw
  (crash to desktop). The binding now makes the context current on whichever thread calls in.
- Azahar's user directory: `SetUserPath` only replaces the root once the paths exist, and logging
  created them first, so every game's saves and shader cache went to the user's own
  `%APPDATA%\Azahar`. The binding now sets the root and every derived path before anything else.
- Windows' 260-character path limit: the per-ROM directory plus the SD-card layout (110
  characters by itself) put a save file at 251, and Azahar deletes by renaming to a longer name,
  which failed; the game then hung on "additional data is being created", or after Continue.
  All 3DS games now share `UserData/azahar/` (one SD card; saves live under their title id).
- Fast-forward showed black (title) or sky-blue over black (overworld): only one frame in N was
  drawn, and the 3DS shows a frame one VBlank after it is rendered, so the captured frame was
  the cleared framebuffer. `EmulatorCore::draw_lead_frames()` (1 on the 3DS) makes paced runs,
  replay seeks and the frame server draw the frames before a presented one as well. Drawing one
  frame before each presented one still strobed at 3x and 4x: the game redraws the bottom screen
  only on some frames, so any skipped frame can leave a cleared buffer on the screen. The 3DS
  therefore returns `u64::MAX`: every frame is drawn at every speed, and a seek draws its whole
  walk. Measured: 4x still runs at 240 fps and exact seeks cost the same as before.

`supershuckie-frontend/examples/n3ds_frontend_check.rs` drives the frontend the way the app does
(threaded core, seeded settings, `--mash` for A/Start, `--dump` for screen BMPs) and is the check
for this class of problem.

## 8. Format v9 and the seek path (2026-09-26)

The six items of `replay-3ds-format-research.md` §8, implemented. v8 files still play (the player
decodes both); DS/GBA/GB files are untouched (they keep writing v7, byte for byte).

### 8.1 What changed

1. **Player: in-place chains** (`playback.rs`, `StoredBuffers`). One working buffer per level,
   each tagged with the keyframe whose state it holds; a delta is applied in place when its
   reference is the keyframe its own level's buffer holds, and the reference's buffer is copied
   in first only when a walk branches off another level. A seek follows the reference chain back
   to the nearest keyframe any buffer holds (a precomputed table of each stored keyframe's
   reference), so seeking back and forth near one spot re-walks a few steps, and a seek to a
   keyframe a buffer holds costs nothing. Buffers keep 1 MB of slack so a state a few bytes
   longer never reallocates 150 MB.
2. **Seeks skip drawing** except the last 3 frames (`EmulatorCore::seek_draw_tail_frames`; paced
   playback still draws every frame). Azahar patch 0007 tracks what skipping leaves stale: a
   skipped draw marks its colour buffer, a fill of the buffer clears the mark, a transfer or copy
   passes it on, and a drawn draw sampling a stale texture is stale itself. A drawn draw does not
   clear it (it may redraw only part of a buffer), at the price of redoing 15–35% of all seeks:
   ORAS double-buffers both screens' render targets and never fills them. A seek that ends with
   anything marked (`skipped_draws_left_stale`) loads the same keyframe again and redraws from
   the oldest marked frame (`SuperShuckieCore::run_replay_to_complete_picture`), so the picture
   is never worse than drawing the whole walk. `SUPERSHUCKIE_3DS_SEEK_TAIL` overrides the tail.
3. **Thumbnails in groups of 60** (`Packet::Thumbnail::against_previous`, discriminator 0xFD):
   the first picture of a group alone, the rest zstd'd with the previous picture as prefix, an
   unchanged screen stored as nothing. The player decodes from the group start or from the
   picture it decoded last (a drag forward decodes one picture per step).
4. **Inputs only when they change**: a 3DS recorder skips a `ChangeInput` equal to the current
   input (playback restores the input from each keyframe's metadata, so nothing depends on the
   repeats).
5. **Keyframe payload v9** (`stored_keyframe.rs`): the region diff compressed with the
   reference keyframe's whole *state* as zstd prefix (window sized to reach all of it, up to
   2^30), and ROM copies: runs of the diff's data that are verbatim bytes of the game file are
   stored as `(literal gap, ROM offset delta, length)` references. The live recorder looks for
   them only in what the game read since the reference keyframe (Azahar patch 0007 logs every
   RomFS read of the game file, the core passes them at each keyframe: `take_rom_reads` →
   `ReplayFileRecorder::rom_reads`), indexed as 64-byte blocks; the converter, which has no read
   log, indexes the whole ROM at a coarser stride (`set_rom_reads_unknown`). The player needs the
   ROM for such keyframes (`ReplayFilePlayer::set_rom`; the frontend passes the ROM it loaded,
   and `wants_rom` tells a v9 file).
6. **Cadence: a keyframe every 4 s** (240 frames), a level-1 keyframe every 2 minutes and a full
   one every 60 (`NINTENDO_3DS_FRAMES_PER_KEYFRAME`, `NINTENDO_3DS_KEYFRAME_LEVELS` in the
   frontend). A 3DS keyframe also waits a frame while two are still being encoded
   (`MAX_3DS_KEYFRAMES_IN_FLIGHT`), so a busy stretch at high speed stretches the interval
   instead of queueing 150 MB states.

Also: the converter reads and writes 3DS files (`supershuckie-replay-convert --rom <game>`,
`--keyframe-levels`), checks the ROM's checksum against the header, and `--verify` compares
meaning (pixels of timeline pictures, inputs in effect) rather than bytes; resume and the
converter carry timeline pictures over (the resumed part used to have none); resuming a v9 file
copies every packet before the last keyframe at or before the resume point verbatim and
re-encodes only from there (it used to re-encode the whole file).

### 8.2 Measured on the user's 2-hour Alpha Sapphire recording

The file of `replay-3ds-format-research.md` (v8, 684 MB, 897 keyframes at 8 s), same machine,
before and after:

| | v8 player | v9 player (in place) |
|---|---|---|
| Random seek, materialising the keyframe (`n3ds_seek_bench`, 150 seeks) | 791 ms avg, 2.3 s worst | 77 ms avg, 384 ms worst |
| Every keyframe, last to first | 527 ms avg | 20 ms avg |
| Every keyframe, first to last | 53 ms avg | 4 ms avg |

Converted with `supershuckie-replay-convert --verify` (keyframes stay where they were, 8 s):

| | File |
|---|---|
| v8 as recorded | 684 MB |
| v9: state prefix + grouped pictures + inputs on change | 352 MB (1.94x smaller) |
| v9 with ROM copies (whole-ROM search, 256-byte stride) | 241 MB (2.84x smaller) |

Both verified identical to the source: every keyframe state, every frame's packets, every
picture's pixels. The live recorder's ROM copies come from the read log at a 64-byte stride,
which finds shorter copies than the converter's whole-ROM search.

Keyframe cadence with the v9 recorder (real `ReplayFileRecorder`s fed by Azahar's read log,
`n3ds_v9_lab`, the busiest 20 minutes of the file, keyframes only):

| Cadence | Per hour | Per 3 h if all of it were this busy | Encode per keyframe (avg / worst) |
|---|---|---|---|
| 2 s | 957 MB | 2.87 GB | 144 ms / 816 ms |
| 4 s | 538 MB | 1.61 GB | 166 ms / 1.4 s |
| 8 s | 296 MB | 0.89 GB | 184 ms / 727 ms |

(v8 on the same stretch: ~730 MB/h at 8 s.) 4 s was chosen: a third of the 5 GB ceiling even at
the busiest, half the re-emulation of 8 s. 2 s fits too on this data, but with less margin and
no byte-rate governor to fall back on.

Seeks through the core (`n3ds_v9_lab --seeks 40`, the first 10 minutes of the converted file,
8-s keyframes, each target checked against straight playback's heap and both screens; the
machine was short of memory, Dropbox holding 58 GB of 64, so absolute times are inflated):

| | Average | Worst | Screens right | Redone drawn |
|---|---|---|---|---|
| Every frame of the walk drawn (before) | 3.1 s | 7.2 s | 40/40 | — |
| Drawing skipped, 3-frame tail, stale guard | 1.7 s | 5.5 s | 40/40 | 14/40 |

On frames 36,000–54,000 of the same file (40 seeks), every seek showed exactly the picture of
the same walk with every frame drawn (6 needed the redo); against straight playback, however, a
third of the pictures differ there, with every frame drawn just the same: a loaded recorded
keyframe does not render exactly what playback reaches (research §9), which is older than this
change. (One target's heap also differed from playback in both runs of the first range.) Where a seek's time goes
(`--profile-seeks`): state load 150–350 ms, hidden frames 1.5–3 ms each, the first ten drawn
frames after a load ~20 ms each (the rasterizer cache starts empty), later ones 3–5 ms.

Through the app's own code (`n3ds_frontend_check --mash --speed 4 --record-after 30
--record-seconds 20`, threaded core, frontend-supplied ROM): the recording is v9 with a keyframe
every 240 frames, an input only per button change, grouped pictures; Azahar's read log reached
the recorder (e.g. one keyframe: 58 ranges, 3.4 MB read, 2.3 MB of its changes stored as ROM
copies), 19 of 21 keyframes use the ROM, all materialise, and seeks through the frontend land on
their frames. In the lab, the same recorder without the ROM wrote 27% more on busy play.

### 8.3 Not done, and what to check in the app

- No byte-rate governor (§3.6); the 4-s cadence leaves a 3x margin on the busiest stretch
  measured, and only Alpha Sapphire was measured.
- The history reference (research §2.2/§7) is not used.
- The stale-draw guard is conservative (fill-only): on ORAS a third of seeks are redone drawn.
  A cheaper exact rule would need to know when a draw covers a whole buffer.
- A drawn frame right after a state load costs ~20 ms (the rasterizer cache starts empty) and
  ~4 ms after ten frames; a state load itself is 150–350 ms here. Both dominate a seek once the
  walk is skipped, and neither was worked on.
- In the app, by hand: record a 3DS session (4-s keyframes), scrub the timeline (pictures while
  dragging, the exact picture on release), resume a replay, and open an old v8 replay.
- Converting existing v8 replays is a command-line step (`supershuckie-replay-convert <in> <out>
  --rom <game> --verify`); the app's batch converter still skips 3DS files.

## 9. Format v10: keyframes without the VRAM the GPU rewrites (2026-09-26, later)

Question: the user's 11-minute Omega Ruby recording at 4x speed (`default-0-resume.replay`,
155,420 frames = 43 game minutes, 487 MB) is too big; how far down can it go? Measured with
new research tools (`supershuckie-replay-recorder/examples/n3ds_attrib_lab.rs`, which decodes
every keyframe of a file and attributes each delta's *compressed* bytes to memory regions by
recompressing without them; `supershuckie-core/examples/n3ds_mask_lab.rs`, which loads recorded
keyframes with VRAM replaced and plays the recorded inputs forward; `n3ds_v9_lab --out`, which
now re-encodes a replay through the real core and recorder into a playable file;
`thumb_codec_lab`, `packet_seq`).

### 9.1 Where the 487 MB went

| Part | Bytes |
|---|---|
| 626 level-2 keyframes (4 s), 591 KB each | 370 MB |
| 20 level-1 keyframes (2 min), 2.26 MB each | 45 MB |
| 2 full keyframes (the resume repeated one) | 28 MB |
| 2,590 timeline pictures (top screen 34 MB, bottom 8 MB) | 42 MB |
| inputs, frame markers | 1 MB |

Inside a level-2 delta (574 KB of compressed bytes per keyframe attributed, averaged over 63):

| Region | Changed per keyframe | Compressed cost | Share |
|---|---|---|---|
| **VRAM (6 MB)** | 957 KB, all literal | **237 KB** | **41%** |
| Azahar archive (kernel, services, DSP; 9.8 MB) | 710 KB | 98 KB | 17% |
| FCRAM 0-16 MB (the game's heap) | 1.1 MB, 163 KB of it ROM copies | 156 KB | 27% |
| FCRAM 32-56 MB (linear heap: assets, 388 KB of ROM copies) | 708 KB | 53 KB | 9% |
| the rest of FCRAM, DSP RAM, the diff's control stream | | 30 KB | 5% |

VRAM is the render targets and display framebuffers the GPU rewrites every frame: the same
bytes in every keyframe, none of them state the game keeps. The archive's churn is mostly the
DSP HLE's decoded audio (the `dsp` component, 1.45 MB of the archive, sits at offsets
8.75-10.2 MB of it; `SUPERSHUCKIE_SERIALIZE_TIMING` with an archive-offset print located the
components: timing 61-718 B, CPU cores to 1.06 MB, services to 8.4 MB, FS archives to 8.75 MB,
DSP to 10.2 MB, memory 100 KB, kernel 4 KB, GPU 108 KB). ROM copies from the read log are
already finding what there is: a whole-ROM search (the converter) made the file 5% bigger.

### 9.2 Is VRAM needed? Measured on the recording

`n3ds_mask_lab` loads a recorded keyframe with its VRAM replaced by the VRAM of the keyframe 4 s
before it (every changed page 4 s stale), plays the recorded inputs for 240 frames and compares
with the same stretch from the exact keyframe:

| VRAM replaced | Keyframes | Game memory identical at the end | Picture the recording's again after |
|---|---|---|---|
| all of it | 36 | 36/36 | 1 frame (most), 2 frames; **one keyframe never** (a buffer rendered once at a scene change, then sampled as a texture for minutes) |
| only pages that also changed in the interval before | 30 | 30/30 | 1-2 frames; **one never** (the same kind of buffer, rendered in two consecutive intervals) |
| nothing (control) | 12 | 12/12 | 0 |

So the game never reads VRAM on the CPU (memory identical in 78 runs), the GPU redraws its
targets within two frames, and the exception is a buffer rendered once and read for a long time:
no statistical rule finds it, but the GPU knows which pages it *reads*.

### 9.3 What was built

1. **Azahar patch 0009**: while tracking is on, what touched each 4 KB page of VRAM first since
   the record was last taken: nothing, a read (textures the rasterizer cache looks up, transfer
   and copy sources, vertex and index data, command lists, HLE block reads of the VRAM mapping)
   or a write (draw colour and depth targets, transfer, copy and fill destinations). Blending,
   depth tests and the screen showing a framebuffer only affect one frame's picture and do not
   count (counting them kept a framebuffer and a depth buffer per keyframe and halved the
   saving). A write covering part of a page counts as a read. Binding:
   `azahar_rs_core_set_vram_access_tracking`, `take_vram_access`, `vram_raw_offset`.
2. **Core**: `EmulatorCore::set_transient_page_tracking` / `take_transient_page_access` (the 3DS
   core implements them); the record loop hands the report of each keyframe interval to the
   recorder (`ReplayFileRecorder::transient_page_access`) before the keyframe, like ROM reads.
3. **Recorder** (`stored_keyframe_mask_transients`, default on, tied to the DS setting
   *mask transient buffers* in the frontend): a 3DS keyframe is written only when the next one
   arrives, since the report of the interval *after* it decides what it may leave out: a VRAM
   page that changed but is written before anything reads it in that interval (or not touched
   at all) is copied from the reference instead, so the delta skips it, and the state kept as
   the next reference has it stale too, which is exactly what a player rebuilds. The packets
   written meanwhile wait behind the pending keyframe. The last keyframe of a recording leaves
   nothing out. A typical Omega Ruby keyframe leaves out 287 of 290 changed VRAM pages.
4. **Player / seeks**: format v10 (`REPLAY_VERSION_NINTENDO_3DS_STALE_VRAM`); v9 files play as
   before. The player flags every v10 delta keyframe as one with stale transients, the 3DS core
   loads it with `load_save_state_with_stale_output` and reports `shows_stale_output` until it
   has drawn `MASKED_VRAM_SETTLE_FRAMES` = 6 frames; a seek therefore draws its last 6 frames
   instead of 3, and a target closer than that to its keyframe falls back to the keyframe before
   (the existing DS machinery). Resume takes v9 and v10 sources through the verbatim fast path.
5. **Timeline pictures as JPEG** (`stored_thumbnail_jpeg_quality`, default 75, 0 = lossless as
   before; `Thumbnail` with discriminator 0xEF; `jpeg-encoder` to write, `zune-jpeg` to read):
   a pair of screens is 8.6 KB instead of 16, decodes alone in 0.06 ms, and the timeline only
   ever shows them while dragging.

### 9.4 Measured

Re-encoding the whole recording through the real core and recorder (`n3ds_v9_lab --out`, same
cadence and levels, same ROM copies from the read log):

| | Size | Level-2 keyframe |
|---|---|---|
| The recording (v9) | 487 MB | 591 KB |
| v10, VRAM left out | 329 MB | 383 KB |
| **v10, VRAM left out, JPEG pictures** | **309 MB** (pictures 42 -> 22 MB) | 383 KB |

The final file: 610 level-2 keyframes 234 MB, 20 level-1 38 MB, one full 14 MB, 2,587 picture pairs 22 MB (the bottom screen, identical two times in three, is now stored every time: an empty-means-unchanged JPEG screen would save ~6 MB more). Where the remaining bytes of a keyframe go (383 KB): the game's heap 0-16 MB 158 KB (43%), the
archive 90 KB (25%), the linear heap 32-56 MB 90 KB (25%), VRAM 7 KB (2%).

On a 24,000-frame stretch of busy play (from frame 24,000), with VRAM left out:

| | Size | Encode per keyframe |
|---|---|---|
| 4-s keyframes, zstd 3 | 55.4 MB (493 MB/h) | 139 ms avg, 293 worst |
| 8-s keyframes, zstd 3 | 50.6 MB (451 MB/h) | 154 / 295 |
| 4-s keyframes, zstd 9 | 52.7 MB (469 MB/h) | 175 / 755 |

Both include the 14 MB full keyframe. Doubling the interval now saves only 12% of the deltas
(before, the per-keyframe VRAM made 8 s nearly half the size of 4 s), so the 4-s cadence stays;
zstd 9 saves 7% of the deltas for a quarter more encode time and a worse worst case, not
taken.

Correctness: a v10 file recorded through the frontend's own path (`n3ds_frontend_check --mash
--speed 4 --record-after 30 --record-seconds 60`, 61 keyframes, 59 of them using the ROM): 60
random seeks through it, each checked against straight playback of the same file (heap hash,
both screens) and against the same walk drawn frame by frame: **60/60 on all counts, 0 redone**
(`n3ds_v9_lab --seeks 60`). Frontend seeks land on their frames (the check's own report).

### 9.5 Things to know

- A re-encoded copy of an *older* recording is not self-consistent for seeks: its first keyframe
  is a state this build saved (state class 1, patch 0008's clock start) while the source's
  keyframes are class 0, and playback from the two diverges within seconds (the console clock
  reads differ; `hash-source` vs `hash-lab` in the session notes). Files recorded by this build
  are fine (the check above). This affects resuming a pre-0008 3DS replay with this build in
  the same way; not fixed here.
- The tracking misses CPU reads of VRAM through the JIT's fast path. Omega Ruby never reads
  VRAM on the CPU (game memory identical in 78 stale-VRAM runs); a game that does would
  diverge on a seek. `mask transient buffers` off records lossless v10 files.
- Every seek now draws 6 frames instead of 3 (~20 ms each right after a load), and a seek
  landing within 6 frames of a keyframe falls back to the keyframe before (~0.4 s more).
- Not done: the archive's DSP audio churn (a quarter of what is left; PCM-aware coding or a
  smaller HLE state in Azahar), the game heap's own changes (43%; nothing to leave out there),
  a byte-rate governor, hand checks in the app (record, scrub, resume a v10 file; open a v9
  file; the pictures while dragging are now JPEGs).
