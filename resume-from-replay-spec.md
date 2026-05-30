# Resume From Replay — Implementation Spec

**Target:** Claude Code, working in the `SnowyMouse/supershuckie` repository.
**Component:** Super Shuckie 2 replay system (`supershuckie-replay-recorder`, `supershuckie-core`, `supershuckie-frontend`).

---

## 1. Goal

Add a **Resume From Replay** feature with two modes:

1. **Resume from the end** — pick an existing replay, load it, and begin recording a *new* replay that continues from the final frame. The older Super Shuckie had an equivalent feature; this is the parity case, with the new wrinkle that the v3 format stores timing, so the continuation must keep timing perfectly continuous.
2. **Resume from an arbitrary frame `N`** — pick a replay and a frame that is *not* the final frame. Produce a **new file** containing everything from the source replay up to and including frame `N`, then record forward from `N` into that same new file.

Both modes must **never mutate the source replay** — always write a new file.

The hard requirement that shapes the whole design: **timing must continue perfectly.** In the v3 format, time is not implicit — it is stored as per-frame deltas and absolute timestamps (see §3). The resumed timeline must continue from the source's elapsed time at the resume point, not restart at zero.

---

## 2. How the replay system works today (read this first)

Accurate as of the current `master`. File paths are stable; line numbers are not, so they're omitted.

### 2.1 File layout

A replay file is three regions, in order:

1. **Header** — exactly 2048 bytes, `ReplayHeaderRaw` (`replay_file/header.rs`). Fixed `#[repr(C, packed(1))]` struct. Contains: start/end signatures, `replay_version` (current written version is **3**, minimum supported **2**), `console_type`, ROM/BIOS/patch checksums, emulator core name, patch length/format, and the **crop / timing markers**: `crop_start` flag + `crop_start_frame` + `crop_start_millis` + `crop_timer_offset`, and `crop_end` flag + `crop_end_frame` + `crop_end_millis`.
2. **Patch data** — `patch_data_length` bytes immediately after the header (often empty).
3. **Packet stream** — a sequence of top-level packets. In a finished file these are almost all `CompressedBlob` packets; an in-progress (crash-safe temp) file additionally has uncompressed packets at the tail.

> **Note on the `// FIXME: How do we handle resuming?` comment in `header.rs`:** the answer this spec gives is **no format change is required**. A resumed file is an ordinary v3 file. Timing is preserved by carrying the absolute `elapsed_millis` forward and re-emitting identical `NextFrame` deltas (see §4). Do **not** bump `REPLAY_VERSION`. You may replace that FIXME with a one-line pointer to the resume module.

### 2.2 Packets and where timing lives — `packet.rs`, `packet/io.rs`

`Packet` enum variants that matter:

- **`NextFrame { timestamp_delta: TimestampMillis }`** — advance one frame. **This is where time lives.** `timestamp_delta` is the milliseconds elapsed since the previous frame. Total replay time = sum of all `NextFrame` deltas.
- **`Keyframe { metadata, state }`** — full save state plus `KeyframeMetadata { input, speed, elapsed_frames, elapsed_millis, counters }`. `elapsed_millis` is the **absolute** running time at that frame.
- **`DeltaKeyframe { metadata, diff }`** — a keyframe stored as a diff against the previous keyframe. **The player transparently converts these into full `Keyframe`s on load** (both at top level and inside decompressed blobs), so resume code that consumes the player never sees `DeltaKeyframe`.
- **`CompressedBlob { keyframes, bookmarks, compressed_data, uncompressed_size, timestamp_start, timestamp_end, elapsed_frames_start, elapsed_frames_end }`** — a zstd-compressed sub-stream of packets. Carries copies of its keyframe/bookmark metadata for indexing without decompressing. **The decompressed sub-stream always begins with a full `Keyframe`** (enforced on read).
- `ChangeInput { data }`, `ChangeSpeed { speed }`, `WriteMemory { address, data }`, `ResetConsole`, `LoadSaveState { state }`, `Bookmark { metadata }`, `IncrementCounter { name, delta }`, `NoOp`.

Integers use a compact little-endian length-prefixed varint encoding; you do not need to touch serialization.

### 2.3 The recorder — `replay_file/record.rs`

`ReplayFileRecorder<Final: ReplayFileSink, Temp: ReplayFileSink>` is a **pure data writer** — it has no dependency on the emulator. It holds two sinks and writes differently to each:

- `final_sink` ← header + patch + **completed compressed blobs only**.
- `temp_sink` ← header + patch + completed compressed blobs + **the current in-progress blob as uncompressed packets** (the crash-safe running copy).

Internal mechanics relevant to resume:

- `new_with_metadata(metadata, patch_data, settings, starting_timestamp, starting_input, starting_speed, initial_keyframe_state, final_sink, temp_sink)` writes the header + patch to both sinks, then **inserts a keyframe at frame 0** using `initial_keyframe_state` at time `starting_timestamp`. It hardcodes `elapsed_frames = 0`, `elapsed_millis = 0` initially and then applies `starting_timestamp` via that first keyframe.
- `next_frame(timestamp)` computes `timestamp_delta = timestamp - self.elapsed_millis`, errors (`BadInput`) if that subtraction underflows, then increments `elapsed_frames`, sets `elapsed_millis = timestamp`, and writes a `NextFrame`. **It takes an absolute timestamp and derives the delta.**
- `insert_keyframe(state, elapsed_millis)` asserts time does not go backward, records a keyframe (diffed against `last_state_to_diff` when possible), and **auto-splits the blob** (`next_blob()`) when `current_blob_size_undiffed` crosses `settings.minimum_uncompressed_bytes_per_blob`. This means re-feeding keyframes naturally bounds memory.
- `set_input`, `set_speed` (dedups — no-op if speed unchanged), `write_memory`, `reset_console`, `load_save_state`, `add_bookmark`, `change_counter` map 1:1 to the corresponding packets.
- `mark_start(timer_offset)` / `mark_end()` write into the **header** crop fields and re-sync the header to both sinks.
- `close()` flushes the final blob.

`NonBlockingReplayFileRecorder` (`record/thread.rs`) wraps the above on a worker thread and is what the core actually stores. It exposes the same surface plus `close()`.

### 2.4 The player — `replay_file/playback.rs`

`ReplayFilePlayer::new(bytes, allow_some_corruption)` parses header + patch + packets and builds keyframe/bookmark indexes. Key methods:

- `get_total_frames()`, `get_total_milliseconds()`, `get_replay_metadata()`, `get_patch_data()`.
- `go_to_keyframe(frame)` — positions the cursor at a keyframe (errors with `NoSuchKeyframe { best }` if `frame` isn't exactly a keyframe; `best` is the nearest keyframe ≤ `frame`).
- `next_packet()` — yields the next packet in order, **transparently descending into compressed blobs and de-diffing DeltaKeyframes into full Keyframes.** This flat stream is exactly what the resume re-feed will consume.

### 2.5 Core integration & timing — `supershuckie-core/src/lib.rs`

`SuperShuckieCore` owns `total_frames`, `total_milliseconds`, `starting_milliseconds`, an optional `replay_file_recorder` and an optional `replay_player`.

- **Live recording timing** (`do_frame_timekeeping`): per frame it computes `ms = timestamp_provider.now() - starting_milliseconds` and calls `recorder.next_frame(ms)`. So the recorded timeline is wall-clock relative to `starting_milliseconds`.
- `restart_timer()` sets `starting_milliseconds = now`, `total_milliseconds = 0`, `total_frames = 0`. `start_recording_replay()` calls this — which is why a fresh recording always starts at time 0.
- `push_keyframe_if_needed()` inserts a keyframe every `frames_per_keyframe` frames using `total_milliseconds`.
- **Playback** (`handle_replay`): pulls packets; `NextFrame` does `total_milliseconds += timestamp_delta`. `go_to_replay_frame(frame)` / `go_to_replay_frame_inner` seek to a keyframe ≤ `frame`, load that save state, set `total_frames`/`total_milliseconds`/counters/speed from the keyframe metadata, then run **unlocked** to exactly `frame`. **Reuse this to position the emulator at `N`.**
- `attach_replay_player()` enforces ROM/BIOS/core compatibility, then seeks to frame 0. `detach_replay_player()` stops feeding packets but **does not reset the emulator state** — it does call `reset_input()` (enqueues empty input), which resume must compensate for (see §6.4).

The timestamp provider (`std_timestamp_provider`) is backed by `Instant`, epoch = core construction. **Its value at resume time can be much smaller than a long replay's elapsed milliseconds** (you launch, load a ROM, load a replay — only seconds of monotonic time have passed, but the replay may be hours). This drives a required fix in §4.3.

### 2.6 Existing `continue_last_replay` is NOT this feature

`supershuckie-frontend/src/lib.rs` has `continue_last_replay` / `can_continue_last_replay` backed by `last_replay_and_frame: Option<(name, frame)>`. This only **reloads a replay for playback** and seeks to where playback last stopped. It does not record. Mirror its UI/state plumbing pattern, but do not extend it for recording.

---

## 3. The core idea (and why it makes timing correct for free)

**Primary approach — semantic re-feed.** To build the new file's prefix `[frame 0 .. N]`, open the source with a `ReplayFilePlayer` and replay its flat packet stream **back into a fresh `ReplayFileRecorder`**, translating each packet to the matching recorder call, stopping at frame `N`. Then keep that recorder open and hand it to the core to continue live.

Why this nails timing:

- The recorder's `next_frame(timestamp)` recomputes `delta = timestamp - elapsed_millis`. If you feed it a running absolute timestamp that you advance by each source `NextFrame`'s delta, the recorder reproduces **the exact same deltas**. Absolute `elapsed_millis` accumulates identically.
- At the end of the prefix feed, `recorder.elapsed_frames == N` and `recorder.elapsed_millis == <source time at N>` with **zero drift**. Timing continuity is a property of reusing the existing, tested write path — not something you reconstruct by hand.
- It chains correctly: resuming a file that was itself resumed still works, because absolute time is always preserved.

This approach requires **no changes to the file format and no changes to the serialization layer.** It naturally supports **arbitrary `N`** (not just keyframe-aligned frames), because the prefix is fed frame-by-frame and the emulator is positioned with the existing seek logic.

Tradeoff: re-feeding decompresses and re-compresses the prefix once at resume time, and the new file's blob boundaries may differ from the source's. Both are fine — output is a valid, semantically identical `[0..N]` prefix. Memory stays bounded because `insert_keyframe` auto-splits blobs.

> An alternative "verbatim blob copy + split the boundary blob" approach (copy completed blobs byte-for-byte, decompress only the blob containing `N`, keep its packets up to `N` as the new in-progress blob) avoids re-compression and is faster for very large files. It is **out of scope for v1** — see §9. Implement the re-feed first.

---

## 4. Timing model — the crux

### 4.1 Recorder side (handled by the re-feed)

Construct the recorder with `starting_timestamp = <source frame-0 elapsed_millis>` (normally 0, but read it from the source's frame-0 keyframe to support chained resumes). Maintain a local `running_ms`, initialized to that same value. For each source `NextFrame { timestamp_delta }`, do `running_ms += timestamp_delta; recorder.next_frame(running_ms)`. Result: `recorder.elapsed_millis == running_ms == source time at N` exactly.

### 4.2 Core side (live continuation)

After the prefix is built, the core must continue the wall-clock timeline from `N`. Add a timer-prime operation:

```text
resume_timer(resume_millis, resume_frames):
    paused_timer_at      = None
    starting_milliseconds = now().wrapping_sub(resume_millis)   // wrapping!
    total_milliseconds    = resume_millis
    total_frames          = resume_frames
```

Then `do_frame_timekeeping`'s `ms = now - starting_milliseconds` evaluates to `resume_millis + (real elapsed since resume)`, so the first live frame produces a tiny positive delta and the timeline is seamless.

### 4.3 Required fix: wrapping subtraction in the timer (do not skip)

`do_frame_timekeeping` currently does a **plain** subtraction: `now - self.starting_milliseconds.0`. With `resume_timer` setting `starting_milliseconds = now.wrapping_sub(resume_millis)`, if `now < resume_millis` (very common — see §2.5) then `starting_milliseconds` wraps near `u64::MAX` and the plain subtraction **underflows and panics in debug builds**.

**Fix:** change the subtraction in `do_frame_timekeeping` to `now.wrapping_sub(self.starting_milliseconds.0)`. This is consistent with the existing `pause_timer`/`unpause_timer`, which already use wrapping arithmetic, and makes `ms = resume_millis + (now - now_at_resume)` correct under modular arithmetic for all inputs. (`next_frame`'s monotonicity check still holds because real elapsed time is non-negative, so each `ms ≥` the previous `elapsed_millis`.)

---

## 5. Files to touch

| Crate | File | Change |
|---|---|---|
| `supershuckie-replay-recorder` | `src/replay_file/record.rs` (new submodule `record/resume.rs`) | Add `build_resumed_recorder`, `ResumeInfo`, `ResumeCropPolicy`, `ReplayResumeError`; unit tests. |
| `supershuckie-replay-recorder` | `src/replay_file/header.rs` | Replace the `// FIXME: How do we handle resuming?` comment with a pointer; optionally a small `apply_resume_crop_policy` helper on the metadata/header. |
| `supershuckie-core` | `src/lib.rs` | Add `resume_recording_replay(...)`, `resume_timer(...)`; **apply the §4.3 wrapping fix**; add an input-preserving detach path (§6.4). |
| `supershuckie-core` | `src/thread.rs` | If the threaded core wrapper mirrors recording entry points, add a matching `resume_recording_replay` passthrough. |
| `supershuckie-frontend` | `src/lib.rs` | Add `resume_recording_from_replay(name, resume_at_frame)`, file creation, orchestration; surface `can_resume`-style state if useful for UI. |
| `supershuckie-frontend-webserver` / `docs/external_commands.md` | (optional) | Add a `resume-replay` REST command + JS client method + docs. |
| `supershuckie-qt` | (optional) | Menu/UI entry to trigger resume and to pick a frame. |

---

## 6. Detailed implementation

### 6.1 `supershuckie-replay-recorder` — new public API

```rust
/// Counter snapshot + position + input/speed at the resume point.
#[derive(Clone, Debug, Default)]
pub struct ResumeInfo {
    pub elapsed_frames: UnsignedInteger,
    pub elapsed_millis: TimestampMillis,
    pub speed: Speed,
    pub input: InputBuffer,        // encoded input bytes in effect at the resume frame
    pub counters: Vec<Counter>,    // exact counter values at the resume frame
}

/// How to carry the header crop / timing markers into the resumed file.
#[derive(Copy, Clone, PartialEq, Debug, Default)]
pub enum ResumeCropPolicy {
    /// Carry crop_start + crop_timer_offset forward when crop_start_frame <= resume_frame;
    /// always drop crop_end (the run is being extended and will be re-marked).
    #[default]
    PreserveStartDropEnd,
    /// Drop all crop markers; the user re-marks start and end.
    DropAll,
    /// Carry all markers forward verbatim. Only meaningful when resume_frame >= crop_end_frame.
    PreserveAll,
}

#[derive(Clone, Debug)]
pub enum ReplayResumeError {
    /// The source player produced no usable frame-0 keyframe.
    BadSource { explanation: alloc::borrow::Cow<'static, str> },
    /// Writing the resumed prefix failed.
    Write(ReplayFileWriteError),
    /// Reading/seeking the source failed.
    Read(ReplaySeekError),
}

/// Build a recorder primed to continue from `resume_at_frame` (None = end of replay).
///
/// Writes header + patch + the prefix [0 ..= target] into the two sinks by re-feeding the
/// source player's flat packet stream. Returns the OPEN recorder (do not close it — the caller
/// continues recording) and a `ResumeInfo` describing the resume point.
///
/// `source` should be a FRESH player instance dedicated to this call (its cursor is consumed).
pub fn build_resumed_recorder<FS: ReplayFileSink, TS: ReplayFileSink>(
    source: &mut ReplayFilePlayer,
    resume_at_frame: Option<UnsignedInteger>,
    settings: ReplayFileRecorderSettings,
    crop_policy: ResumeCropPolicy,
    final_sink: FS,
    temp_sink: TS,
) -> Result<(ReplayFileRecorder<FS, TS>, ResumeInfo), ReplayResumeError>;
```

**Algorithm:**

1. `let target = resume_at_frame.unwrap_or_else(|| source.get_total_frames());`
   Clamp `target = target.min(source.get_total_frames())`.
2. `source.go_to_keyframe(0)` (map error → `ReplayResumeError::Read`).
3. Read the first packet; it must be `Packet::Keyframe { metadata, state }` at `elapsed_frames == 0` (else `BadSource`). Capture `state0`, `input0 = metadata.input`, `speed0 = metadata.speed`, `millis0 = metadata.elapsed_millis`, `counters0 = metadata.counters`.
4. `let mut metadata = source.get_replay_metadata().clone();` then apply `crop_policy` against `target`:
   - `PreserveStartDropEnd`: keep `crop_start`/`timer_offset` iff `crop_start.is_some()` and `crop_start_frame <= target`; set `crop_end = None`.
   - `DropAll`: `crop_start = None`, `crop_end = None`, `timer_offset = None`.
   - `PreserveAll`: leave as-is.
5. Construct the recorder:
   `ReplayFileRecorder::new_with_metadata(metadata, ByteVec from source.get_patch_data(), settings, millis0, input0, speed0, state0, final_sink, temp_sink)` (errors → `Write`). This emits header + patch + the frame-0 keyframe.
6. Seed counters if `counters0` is non-empty (rare): for each, `recorder.change_counter(name, value)`. Maintain a local `BTreeMap<String,i64>` seeded from `counters0` for the authoritative running snapshot.
7. Re-feed loop. Track `running_ms = millis0.0`, `cur_frames = 0`, `cur_input = input0`, `cur_speed = speed0`. Loop on `source.next_packet()`:
   - `Ok(None)` → break (end of stream; expected only when `target == total`).
   - `NextFrame { timestamp_delta }`:
     - **Stop condition:** if `cur_frames + 1 > target` → break *without emitting* (this leaves the prefix at exactly frame `target`, including all of frame `target`'s associated packets, which appear before the next `NextFrame`).
     - else `running_ms += timestamp_delta.0; recorder.next_frame(running_ms.into())?; cur_frames += 1;`
   - `ChangeInput { data }` → `recorder.set_input(data.clone())?; cur_input = data;`
   - `ChangeSpeed { speed }` → `recorder.set_speed(speed)?; cur_speed = speed;`
   - `WriteMemory { address, data }` → `recorder.write_memory(address, data.clone())?;`
   - `ResetConsole` → `recorder.reset_console()?;`
   - `LoadSaveState { state }` → `recorder.load_save_state(state.clone())?;`
   - `Bookmark { metadata }` → `recorder.add_bookmark(metadata.name.clone())?;`
   - `Keyframe { metadata, state }` → `recorder.insert_keyframe(state.clone(), metadata.elapsed_millis)?;` (input/speed already tracked via packets; the keyframe's metadata input/speed will match the recorder's current values).
   - `IncrementCounter { name, delta }` → `recorder.change_counter(name.clone(), *delta)?;` and update the local counter map.
   - `DeltaKeyframe` / `CompressedBlob` → unreachable (the player de-diffs and descends); treat as `BadSource` defensively.
   - `NoOp` → skip.
   - `Err(_)` → if `target == total` (end mode) treat as graceful end (break); otherwise return `Read`.
8. Build `ResumeInfo { elapsed_frames: cur_frames, elapsed_millis: running_ms.into(), speed: cur_speed, input: cur_input, counters: <local map → Vec<Counter>> }`.
9. **Do not call `recorder.close()`.** Return `(recorder, resume_info)`.

> If the source stream ends before reaching `target` (corruption / short file), `cur_frames < target`. Return the info at the actual last frame reached; the caller should warn and proceed (resume from the furthest intact frame).

### 6.2 `supershuckie-core` — resume entry point

Add alongside `start_recording_replay`:

```rust
/// Resume recording from an existing replay.
///
/// `player_for_emulator` is the source replay attached for positioning the emulator.
/// `source_bytes` is the raw replay file (used to spin up a SECOND, independent player
/// for the prefix re-feed so cursors don't collide).
/// `resume_at_frame == None` resumes from the final frame.
pub fn resume_recording_replay<FS, TS>(
    &mut self,
    source_bytes: &[u8],
    resume_at_frame: Option<UnsignedInteger>,
    partial: PartialReplayRecordMetadata<FS, TS>,
    crop_policy: ResumeCropPolicy,
    allow_corruption: bool,
) -> Result<(), ReplayResumeError /* or a core-level error */>
where FS: ReplayFileSink + Send + Sync + 'static,
      TS: ReplayFileSink + Send + Sync + 'static;
```

**Sequence:**

1. Ensure the source replay is attached as the active `replay_player` (the frontend will have done this via `load_replay_if_exists`). Determine `target = resume_at_frame.unwrap_or(total_frames)`.
2. **Position the emulator at the resume frame** using existing logic: `self.go_to_replay_frame(target_for_emulator)`. For end mode use `total_frames` (the seek clamps to `total_frames - 1` and runs to the last frame). The emulator now holds the correct state at the resume point. Its encoded input/speed were applied during this playback.
3. Build the prefix recorder from a **fresh** player: `let mut feed_player = ReplayFilePlayer::new(source_bytes, allow_corruption)?;` then `let (recorder, info) = build_resumed_recorder(&mut feed_player, resume_at_frame, partial.settings.clone(), crop_policy, partial.final_file, partial.temp_file)?;`
4. `self.detach_replay_player();` — but preserve input (see §6.4): capture the emulator's current encoded input *before* detach, or use the input-preserving detach variant, then re-apply `info.input`.
5. **Prime the timer:** `self.resume_timer(info.elapsed_millis, info.elapsed_frames);`
6. Restore continuation state: set `game_speed`/playback speed from `info.speed` (call the same path `set_speed` uses, which will dedup against the recorder's already-correct speed); set `frames_since_last_keyframe = 0` (next live keyframe lands on cadence; an early keyframe is harmless); set `replay_counters = Some(info.counters as map)`; set `current_input`/`base_input` from `info.input` (decode if a decoder exists, else apply the encoded bytes directly to the emulator and keep the core's logical input consistent).
7. Store the recorder: `self.replay_file_recorder = Some(Box::new(NonBlockingReplayFileRecorder::new(recorder)));` and `self.frames_per_keyframe = partial.frames_per_keyframe.get();`
8. Done. Recording continues on the next `run()`; the timeline picks up exactly where the source left off.

Add the timer helper:

```rust
fn resume_timer(&mut self, resume_millis: TimestampMillis, resume_frames: UnsignedInteger) {
    let now = self.timestamp_provider.get_timestamp_milliseconds();
    self.paused_timer_at = None;
    self.starting_milliseconds = now.wrapping_sub(resume_millis.0).into();
    self.total_milliseconds = resume_millis;
    self.total_frames = resume_frames;
}
```

**And apply the §4.3 fix** in `do_frame_timekeeping`:

```rust
// before: let ms = self.timestamp_provider.get_timestamp_milliseconds() - self.starting_milliseconds.0;
let ms = self.timestamp_provider.get_timestamp_milliseconds().wrapping_sub(self.starting_milliseconds.0);
```

### 6.3 `supershuckie-frontend` — orchestration & files

Mirror `start_recording_replay`, but read the source and call the core resume. Always create new files; never write the source.

```rust
/// Resume recording from a saved replay. `resume_at_frame == None` => from the end.
pub fn resume_recording_from_replay(
    &mut self,
    source_name: &str,
    resume_at_frame: Option<u32>,
    new_name: Option<&str>,
) -> Result<UTF8CString, UTF8CString>;
```

Steps:

1. `self.assert_replays_available()?;`
2. Resolve and **read the source replay bytes** for the current ROM (same path-building as `load_replay_if_exists`). Keep the bytes — you'll pass them to the core for the re-feed player.
3. Ensure the source is loaded/attached for playback (call `load_replay_if_exists(source_name, override_errors=true)` if not already the current replay). This also runs the ROM/BIOS/core compatibility checks.
4. Create two **new** files via `load_file_or_make_generic` — never reuse the source path. Wrap in `BufWriter` exactly like `start_recording_replay`. Choose `new_name` or auto-generate (e.g. source name + a suffix).
5. Build `PartialReplayRecordMetadata` the same way `start_recording_replay` does (settings from `self.settings.replay`, `frames_per_keyframe`, the two buffered files). The header's identity fields (console/checksums/core name/patch) come from the **source replay metadata** inside `build_resumed_recorder`, so they stay consistent with the source.
6. Call `self.core.resume_recording_replay(&source_bytes, resume_at_frame.map(Into::into), partial, ResumeCropPolicy::PreserveStartDropEnd, true)`.
7. On success, set `self.recording_replay_file = Some(ReplayFileInfo { ... })` (final name + temp path + final path), clear `last_read_replay_stats` / `last_replay_and_frame`, and unpause according to the same `auto_pause_on_record` logic. Return the new replay name.

### 6.4 Input & speed continuity (don't clobber the resume point)

`detach_replay_player()` calls `reset_input()` (enqueues empty input). On resume that would wipe the input held at the resume frame, so the first recorded live frame could differ from the source's frame-`N` input. Handle one of:

- Add `detach_replay_player_keep_input()` that does everything `detach_replay_player` does **except** `reset_input()`, and use it on the resume path; then explicitly set the core's input from `ResumeInfo.input`; **or**
- Keep the existing detach, then immediately re-apply `ResumeInfo.input` (re-encode/`set_input_encoded`) and set `base_input`/`current_input` accordingly before the first frame.

Speed: call the normal `set_speed(info.speed)` — the recorder's `set_speed` dedups, so no spurious `ChangeSpeed` is emitted, and the emulator/game speed end up correct.

---

## 7. Header crop / timing-marker policy

Default `ResumeCropPolicy::PreserveStartDropEnd`:

- **Carry `crop_start` + `crop_timer_offset` forward** when the source marked a start and `crop_start_frame <= resume_frame` (the official run start lies within the kept prefix, so the timer offset and start position remain valid and continuous).
- **Drop `crop_end`** — the run is being extended; the user re-marks the end later via `mark_replay_end`.
- If `crop_start_frame > resume_frame` (start would be in the discarded tail — only possible for odd inputs), drop the start too.

Expose the policy so the frontend can offer `DropAll` (clean re-mark) or `PreserveAll` (end-mode continuation that keeps both markers). Implement the policy as a small pure helper, ideally on `ReplayFileMetadata` (e.g. `fn with_resume_crop(self, resume_frame, policy) -> Self`), and unit-test it.

---

## 8. Edge cases & failure handling

- **Arbitrary `N` (non-keyframe):** supported. The emulator seek snaps to the nearest keyframe ≤ `N` and runs forward to `N`; the prefix re-feed stops at exactly `N`. No keyframe alignment requirement.
- **`N == 0`:** prefix is just the frame-0 keyframe; resume effectively re-records from the source's initial state. Valid.
- **`N >= total_frames` / end mode:** feed the whole stream; stop at the last frame. `ResumeInfo` reflects the source's final frame/millis.
- **`N` beyond available (corrupt/short source):** clamp to the furthest intact frame, surface a warning, proceed.
- **Source opened with corruption tolerance:** `next_packet()` may end early; in end mode treat as graceful end, in mid mode return a read error unless you choose to resume from the last good frame (recommended: resume from last good frame + warn).
- **Cursor collision:** never reuse the emulator's attached player for the re-feed — always construct a separate player from the source bytes.
- **Memory:** bounded — `insert_keyframe` auto-splits blobs during the re-feed at `minimum_uncompressed_bytes_per_blob`.
- **Chained resume:** a resumed file is a normal v3 file; resuming it again works because absolute `elapsed_millis` is always carried forward (read `millis0` from the frame-0 keyframe rather than assuming 0).
- **Two-file model on the new recording:** the in-progress prefix tail lives uncompressed in the new temp file and compresses on the next blob split or on `close()` — identical to normal mid-recording state. Do not special-case it.
- **`stop_recording_replay` afterwards:** unchanged. It closes the recorder (flushing the final blob) and removes the temp file. The resumed final file is a complete, valid replay.

---

## 9. Out of scope for v1 (document, don't build)

- **Verbatim blob-copy fast path.** Optimization for very large sources: copy the header + every completed `CompressedBlob` whose `elapsed_frames_end <= N` byte-for-byte; decompress only the single blob containing `N`, keep its packets up to `N` as the new in-progress blob; then continue recording. Challenges to note for a future task: precisely reconstructing the recorder's private state (`current_blob`, `current_blob_offset`, `current_blob_size_undiffed`, `last_state_to_diff`, `current_blob_keyframes`/`bookmarks`, `counters`, `current_input`, `current_speed`), guaranteeing the partial blob still begins with its original keyframe, and getting the byte offset for `current_blob_offset` exactly right. This requires a dedicated constructor that exposes those internals and is significantly more error-prone than the re-feed. The re-feed (§3) is the correct, safe v1.
- REST/Qt UX polish beyond a basic trigger + frame picker.

---

## 10. Testing plan

Put recorder/resume tests in `supershuckie-replay-recorder` (it has standing `// TODO: WRITE UNIT TESTS` markers). These need no emulator — the recorder and player are pure data.

1. **Round-trip prefix equivalence.** Programmatically build an in-memory replay (frame-0 keyframe + a known sequence of `NextFrame` deltas, `ChangeInput`, `ChangeSpeed`, periodic keyframes, a bookmark, counters). For several `N` (0, a non-keyframe mid value, a keyframe value, `total`), run `build_resumed_recorder`, close it, reparse with `ReplayFilePlayer`, and assert:
   - `get_total_frames() == N` and `get_total_milliseconds() == source time at N`.
   - The sequence of `NextFrame` deltas for `[0..N]` is byte-for-byte identical to the source's.
   - Keyframe save states at matching frames decode to identical bytes (covers diff/de-diff correctness).
   - Bookmarks/counters within `[0..N]` match; counters after `N` are absent.
2. **Timing-continuity (unit, recorder-level).** After `build_resumed_recorder` returns the open recorder + `ResumeInfo`, feed a few more `next_frame(running_ms + k)` calls, close, reparse: assert the appended frames carry the expected deltas and that total time = source-time-at-`N` + appended deltas, with no discontinuity at the seam.
3. **Crop policy.** Unit-test `with_resume_crop` for all three policies against several `(crop_start_frame, crop_end_frame, resume_frame)` combinations.
4. **End mode.** `resume_at_frame = None` on a closed replay reproduces all frames and reports the correct final frame/millis.
5. **Short/corrupt source.** Truncate mid-stream; assert clamping behavior and that the returned `ResumeInfo` matches the last intact frame.
6. **Core timer (integration or focused unit).** Verify `resume_timer` + the wrapping `do_frame_timekeeping` produce a non-negative first delta when `now < resume_millis` (the underflow case). A fake `MonotonicTimestampProvider` returning a small, controllable value makes this deterministic.

---

## 11. Acceptance criteria

- Loading replay **A**, resuming from frame `N`, recording more frames, and stopping yields a new replay **B** where:
  - **B** is a valid v3 replay (parses, has a keyframe at frame 0, plays back start-to-finish).
  - **B**'s frames `[0..N]` are timing-identical to **A** (same per-frame deltas, same absolute time at every keyframe).
  - **B**'s frame `N+1` onward continues the timeline with no jump or reset; total elapsed time is continuous across the seam.
  - **A** is byte-for-byte unchanged on disk.
- End-mode resume (`resume_at_frame = None`) behaves as the special case `N = total_frames`.
- Arbitrary, non-keyframe `N` works.
- No debug panic when resuming a long replay shortly after launch (the §4.3 underflow case).
- `REPLAY_VERSION` is unchanged; no serialization changes.
- New unit tests (§10) pass.

---

## 12. Quick reference — key symbols

- `supershuckie-replay-recorder`: `ReplayFileRecorder`, `NonBlockingReplayFileRecorder`, `ReplayFilePlayer`, `ReplayFileMetadata`, `ReplayHeaderRaw`, `Packet`, `KeyframeMetadata`, `Counter`, `Speed`, `TimestampMillis`, `UnsignedInteger`, `InputBuffer`, `ByteVec`, `ReplayFileSink`, `ReplayFileWriteError`, `ReplaySeekError`. New: `build_resumed_recorder`, `ResumeInfo`, `ResumeCropPolicy`, `ReplayResumeError`.
- `supershuckie-core`: `SuperShuckieCore`, `PartialReplayRecordMetadata`, `start_recording_replay`, `stop_recording_replay`, `attach_replay_player`, `detach_replay_player`, `go_to_replay_frame(_inner)`, `do_frame_timekeeping`, `push_keyframe_if_needed`, `restart_timer`, `pause_timer`/`unpause_timer`, `MonotonicTimestampProvider`. New: `resume_recording_replay`, `resume_timer`, input-preserving detach.
- `supershuckie-frontend`: `start_recording_replay`, `load_replay_if_exists`, `load_file_or_make_generic`, `mark_replay_start`/`mark_replay_end`, `ReplayFileInfo`, `continue_last_replay` (reference only). New: `resume_recording_from_replay`.
