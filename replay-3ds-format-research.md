# 3DS replay format: what else can be squeezed out (research, 2026-09-26)

Question: how far can the Nintendo 3DS replay format (v8, `replay-3ds-spec.md` §7) go on file
size, timeline (seek) speed and keyframe generation cost?

**Data.** Everything below is measured on real play, not the mash script: the user's in-app
recording `Pokemon Alpha Sapphire … .3ds-data/replays/default-0.replay` (2 h 0 min, 429,666
frames, 684 MB, 897 keyframes, Old 3DS mode). The first ~20 minutes are busy play (area changes,
menus); the remaining ~100 minutes are a steady, quiet stretch at a constant rate (the game
idling or doing the same thing over and over). Where the emulator was needed, the replay was
played back headlessly through the real core. Numbers are for that file unless stated.

Tools (all new, research only, nothing in the app changed):

| Tool | What it does |
|---|---|
| `supershuckie-replay-recorder/examples/n3ds_breakdown.rs` | bytes per packet kind; thumbnail/input encodings |
| `…/n3ds_keyframe_lab.rs` | decodes every stored keyframe from the file, re-encodes each consecutive pair 10 ways |
| `…/n3ds_topology_lab.rs` | chain vs anchor ("star") deltas, RAM vs archive split, zstd levels for full keyframes |
| `…/n3ds_rom_match_lab.rs` | how much changed data is a verbatim copy of ROM bytes; recompresses without it (`N3DS_L0=1`: a full keyframe) |
| `…/n3ds_history_lab.rs` | reference = previous state + bytes overwritten by earlier deltas, capped or reset per segment |
| `…/n3ds_chain_bench.rs` | cost of one chain step: decompress / copy / apply |
| `supershuckie-core/examples/n3ds_replay_lab.rs` | plays a replay through the core: keyframe size at other cadences, seek timing and exactness (heap + both screens) |

## 1. Where the 684 MB go today

| Part | Bytes | Share |
|---|---|---|
| Keyframes (4 full, 55 level-1, 838 level-2) | 402 MB | 59 % |
| **Thumbnails** (7,161 pairs, one a second) | **274 MB** | **40 %** |
| `ChangeInput` (one every frame) + `NextFrame` | 7.7 MB | 1 % |

- Thumbnails are each zstd'd alone. Compressed against the previous picture they are 10x
  smaller, and the bottom screen is byte-identical to the previous one 6,418 times of 7,161.
- The 3DS core writes a `ChangeInput` every frame although 97 % repeat the previous one (33
  distinct values in two hours); v8 has no blobs, so nothing compresses them (7.7 MB → 0.14 MB
  with zstd).
- The busy first 10 minutes cost 121 MB of keyframes; each quiet 10 minutes about 17 MB (plus a
  14 MB full keyframe every 30 minutes).

## 2. Keyframe codec

All 896 consecutive keyframe pairs, re-encoded (the file's own frames for the same keyframes:
389 MB):

| Encoding of each delta | Total | Worker time / keyframe | Decode |
|---|---|---|---|
| Today: region diff, zstd 3, prefix = previous payload (levels as in the file) | 389 MB | ~20 ms | 1 ms |
| Same, but always against the previous keyframe (no levels) | 330 MB | 5 ms + 15 ms diff | 1 ms |
| XOR against the old bytes | 30 % worse (changed data is not small edits) | | |
| zstd 19 instead of 3 (first 11 keyframes) | −14 % | 2.5 s | |
| **Prefix = the whole reference state** (the decoder holds it anyway) | **250 MB (−36 %)** | 64 ms | 0.8 ms |
| Whole changed 4 KB pages (what write-watch gives, no compare) + reference-state prefix | 243 MB | 65 ms | 1.1 ms |
| Reference state + 64 MB of bytes earlier deltas overwrote ("history") | **128 MB (−67 %)** | 94 ms | 0.6 ms |
| Changed pages never seen in any earlier keyframe, reference-state prefix (lower bound for global dedupe) | 125 MB | | |
| **Reference state + ROM references** (§2.1) | **140 MB (−64 %)** | | |
| History + ROM references | **108 MB (−72 %)** | | |
| Anchor ("star") deltas: every keyframe against an anchor every 8 / 15 / 30 keyframes | 315 / 399 / 474 MB (worse) | | |

Findings:

- The single cheapest win is the zstd reference. Today's prefix (the previous delta's payload)
  only catches data that moved *within the last change*; the whole reference state catches data
  that moved from anywhere in RAM. The player already holds that state when it applies the
  delta, so it costs nothing on the read side (decode is even slightly faster). On the write side
  zstd indexes the 150 MB prefix each time: ~60 ms of worker CPU per keyframe.
- The game keeps re-loading what it had before (areas, menus, battle assets, music): the "history"
  reference halves the size again. It is decodable only if the decoder has the same history, i.e.
  it walked the chain; see §2.2.
- Chains beat anchors by 25–90 %: what changes between two keyframes is mostly the same memory
  churning, not a growing set.
- Full keyframes: zstd 3 → 13.4 MB, 9 → 12.9 MB, 15 → 12.6 MB (10x the time). Levels do not
  matter; ROM references do (−36 %, §2.1).
- zstd's multithreaded mode had no effect (the bundled zstd-sys is built without it).

### 2.1 ROM references: half of what changes is already on the cartridge

Every 64-byte-aligned, non-constant block of changed data was hashed; the 2 GB ROM was scanned
with a rolling hash at every byte offset (6 s on 12 threads).

- **52 % of all changed (non-constant) bytes over the two hours are verbatim ROM bytes**; 37 % in
  the busy first 20 minutes. Even the quiet stretch keeps re-reading the ROM (a quiet keyframe
  goes from 132 KB to 54 KB); streamed music is the likely cause.
- Deltas: 250 MB → 139.7 MB, plus ~94,000 references (~0.6 MB).
- A full keyframe: 13.4 MB → 8.6 MB (31 % of the state's non-constant RAM is ROM data).
- It needs nothing from earlier keyframes, so it survives any seek: the player only needs the ROM
  (which it always has; the header already pins its checksum).

What it would take: the recorder finds matches between changed data and the ROM (a hash index of
ROM blocks: 64-byte stride over 2 GB is 33 M entries, too big to keep; alternatives are a coarser
stride with a rolling hash over the changed data, or indexing only the ROM ranges the game has
actually read, which Azahar's RomFS read path can report). The delta then carries `(ROM offset,
length)` copy runs next to literal data. The player reads the ROM (memory-mapped).

### 2.2 History that a seek can rebuild

A decoder walking a chain from its start sees every byte each delta overwrites, so it can keep
the same "overwritten bytes" history the encoder used, provided the history resets where a seek
may start a walk. Measured in §7: resets every 2 / 4 / 8 minutes give 210 / 182 / 154 MB.

### 2.3 Where the delta bytes are

Of the 250 MB (reference-state prefix): game RAM (FCRAM + VRAM + DSP RAM) 186 MB, Azahar's
serialised "archive" (kernel, services, GPU, DSP state; ~9.75 MB per state) 64 MB. In the quiet
stretch the archive is ~40 % of every keyframe.

- ~520 KB of the archive "changes" per 8-s keyframe by position, but only ~55 KB of it is really
  new: boost serialisation shifts everything after a variable-length object. Compressing the whole
  archive against the previous archive (instead of region-diffing it) saves only ~8 MB of 58 MB,
  because zstd's matcher already follows the shifts.
- The truly new ~50 KB sits near the end of the archive (kernel objects and GPU state: incrementing
  ids, float 1.0 constants), not in audio sample data. Attributing it needs a per-component offset
  print in patch 0006 (follow-up). Anything there that is derived (GPU caches) could be dropped.

## 3. Keyframe cadence on real play

The busy first 10 minutes, keyframes captured during playback, reference-state prefix:

| Interval | Per keyframe | Per hour |
|---|---|---|
| 2 s | 0.71 MB | 1.27 GB |
| 4 s | 0.97 MB | 0.86 GB |
| 8 s (today) | 1.42 MB | 0.62 GB |
| 16 s | 1.81 MB | 0.39 GB |
| 32 s | 2.57 MB | 0.26 GB |

Quartering the interval only doubles the bytes per hour. With the ROM factor (0.56 over the
file) the busiest play would be ~0.35 GB/h at 8 s, ~0.48 GB/h at 4 s, ~0.71 GB/h at 2 s: **2-second
keyframes fit the 5 GB / 3 h ceiling even if a whole session played like the busiest 10
minutes** (~2.2 GB), and quiet play costs a fraction of that.

## 4. Timeline (seek) speed

Seek = rebuild the keyframe state from the file + load it into Azahar + re-emulate to the target.
Measured with `n3ds_replay_lab --seeks 25` (random targets in the first 5 minutes) and
`replay_probe`. The machine was busy during these runs (the app and OBS were running, Dropbox
held ~40 GB of RAM), so compare the two modes against each other, not with earlier numbers:

| | Re-emulation | Heap exact | Top screen | Bottom screen |
|---|---|---|---|---|
| Today: every frame of the walk drawn (`draw_lead_frames() = u64::MAX`) | 5.4 s avg (57 fps) | 25/25 | 25/25 | 25/25 |
| Drawing skipped except the last 3 frames | **1.25 s avg (4.3x)** | 25/25 | 23/25 | 25/25 |

- **Skipping drawing is the biggest seek lever.** It was disabled because the game redraws some
  screen only on some frames; here the top screen was wrong in 2 of 25 seeks. A guard is needed:
  draw a longer tail (e.g. 30 frames), or have the rasterizer report whether a display framebuffer
  was written during a skipped frame and not redrawn since (then redo those frames drawn).
- **Rebuilding the state costs ~75 ms per chain step** today (`replay_probe`: a backward seek
  inside a 2-minute segment walks up to 14 level-2 steps, 70 ms → 1.2 s; add level-1 steps for up
  to ~2.4 s). The player clones the 150 MB reference state into a fresh allocation for every step
  (`playback.rs`, `fold`: `(*reference.state).clone()`), and when the state is a few KB longer the
  `resize` reallocates it again. Measured per step (`n3ds_chain_bench`):

  | | Decompress | Copy reference state | Apply in place |
  |---|---|---|---|
  | busy stretch | 3.1 ms | 25 ms | 1.4 ms |
  | quiet stretch | 0.6 ms | 28 ms | 0.2 ms |

  Applying on one working buffer (with a little slack so a longer state does not reallocate; copy
  only when branching a level) makes a step **1–5 ms**: a 28-step worst case goes from ~2 s to
  ~0.1 s. This is a player-only change; the format does not change.
- Consequence: chain walks become nearly free, so segments can be long (fewer full keyframes)
  and the history reference of §2.2 costs nothing at seek time.

Estimated exact seek with 2-s keyframes, in-place chains and skip-drawing: rebuild ≲ 0.1 s +
load ~0.1–0.2 s + re-emulation ≤ 120 frames (≤ 0.3 s, average half) ≈ **0.3–0.5 s**, against
~1–7 s measured today on this (busy) machine. Drags stay on thumbnails.

## 5. Thumbnails

| Encoding | 2 h | Worst decode |
|---|---|---|
| Today: each picture alone | 274 MB | — |
| Against the previous picture, identical screen = 1 byte | 27 MB | whole chain |
| Groups of 30 (first alone, rest against the previous) | 35 MB | 0.6 ms |
| **Groups of 60** | **31 MB** | 3.2 ms |
| Groups of 120 | 29 MB | 6.8 ms |
| Half resolution, against the previous | 8.8 MB | |

Groups of 60 keep random access for drags (decode at most 60 small frames) at −89 %. At this cost
twice as many thumbnails, or full-resolution top screens, become affordable if the preview
looks too coarse.

## 6. Keyframe generation cost

- Core thread (measured earlier, `replay-3ds-spec.md` §7.2): ~8 ms serialisation into a recycled
  buffer. The frame after a keyframe is ~3 ms slower than the others (6.1 vs 3.0 ms, clean run),
  because Azahar's save drops its rasterizer cache.
- Worker: the 150 MB memcmp is 15 ms; with the write-watch page list Azahar already keeps
  (patch 0004) the recorder could take "whole changed pages" as the payload (same size, §2) and
  skip the compare. zstd with the reference state as prefix is ~60 ms (the prefix indexing),
  ~3 % of one core at a 2-s cadence. ROM matching adds a rolling hash over the changed bytes only.
- The +3 ms frame after a keyframe: Azahar's save unmarks every cached page (`ClearAll`) so the
  serialised page table has no rasterizer-cached entries. Writing cached pages as plain memory in
  the page-table serialiser, without dropping the cache, would remove it; an earlier attempt that
  simply kept the cache crashed on load for exactly that reason (spec §7.2). Untested.

## 7. History with segment resets

`n3ds_history_lab`, all 896 pairs, reference = the reference state plus the bytes earlier deltas
overwrote (capped at 128 MB), history emptied every W keyframes (where a seek could start a walk):

| History | Total |
|---|---|
| none (reference state only) | 250 MB |
| reset every 15 keyframes (2 min) | 210 MB |
| reset every 30 (4 min) | 182 MB |
| reset every 60 (8 min) | 154 MB |
| rolling 64 MB, never reset (not seek-safe) | 128 MB |

The re-used data recurs over many minutes, so resets cost a lot. History pays only with long
segments, which in-place chain walks make affordable (60 steps × ~3 ms). ROM references (140 MB)
get most of the same benefit with no reset at all, so they come first; history on top is the last
~20 % (ROM + rolling history: 108 MB).

## 8. What the format could become

In order of value for effort:

1. **Player: in-place chain application** (no format change). Seek rebuild ~2 s → ~0.1 s.
2. **Core: skip drawing inside seeks with a stale-screen guard** (no format change).
   Re-emulation 4.3x faster.
3. **Thumbnails in groups of 60 + identical-screen flag.** −243 MB on this file (−36 % of it).
4. **Inputs only when they change** (or small packets compressed in chunks). −7 MB.
5. **Keyframe v9: reference-state prefix** (−36 % of deltas), then **ROM references** (−44 %
   more; −36 % on full keyframes), then, optionally, **history within long segments** (§7:
   worth it only at 8+ minutes between resets).
6. With deltas ÷2.5–3.5: **keyframes every 2–4 s** instead of 8 s, and full keyframes every
   60 minutes instead of 30 (chain walks are cheap once in place).

Projected for this 2-hour file at today's 8-s cadence: 684 MB → **~205 MB** with items 3–5 and
no history (deltas ~140 MB, four full keyframes ~35 MB, thumbnails 31 MB), ~175 MB with rolling
history. At 2-s keyframes, the busiest case is ~2.2 GB per 3 h, with exact seeks around
0.3–0.5 s.

## 9. Side findings

- **Playback does not reproduce the recorded keyframes byte for byte** (0 of 68 in the first 10
  minutes). Game RAM differs by only 600–1,200 bytes per keyframe and does not grow; Azahar's
  timing-event queue in the recorded state holds one more event than in headless playback, with
  times shifted by 81 ticks. Seeks load recorded states, so the timeline is unaffected, but some
  event the app schedules differs from the headless path (worth finding before relying on
  playback hashes against recordings).
- Dropbox was holding ~40 GB of RAM during these runs, which distorted the timing runs.
