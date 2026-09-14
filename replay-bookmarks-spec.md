# Replay Bookmarks — Implementation Spec

**Target:** Claude Code, working in the supershuckie repository on a branch off `ram-tools`.
**Components:** `supershuckie-replay-recorder`, `supershuckie-core`, `supershuckie-frontend`,
`supershuckie-frontend-c`, `supershuckie-frontend-webserver`, `supershuckie-qt`, `supershuckie-frame-server`.
**Status:** approved and implemented 2026-09-14 on branch `replay-bookmarks` (uncommitted). See §21 for
where the implementation departs from this text.

---

## 1. Goal

Turn the replay format's dormant bookmark support into a working feature:

1. Bookmarks are added at specific frames, with a specific name or a generic one.
2. A bookmark can have a duration: an in frame and an optional out frame.
3. Bookmarks are created in Super Shuckie with a keyboard shortcut, or sent programmatically.
4. A bookmark window lists every bookmark in the current replay. Clicking a row seeks playback there.
5. A **keyframe bookmark** stores a replay keyframe so returning to it skips re-emulation. It has
   its own shortcut and an API flag.
6. Bookmarks have a type. Types and their colors are user-defined, and the color identifies rows
   in the bookmark window.

Decisions already made by the user (2026-09-14):

- **Bookmarks are stored inside the replay file.** This needs a new format version (v5) with an
  editable bookmark section at the end of the file.
- **A keyframe bookmark requested during playback snaps** to the replay's existing keyframe at or
  before the current frame. A new keyframe can only be written while recording.
- Spec first, then implementation.

---

## 2. How things work today (read this first)

File paths are stable; line numbers are not, so they are omitted.

### 2.1 Replay file layout — `replay_file/header.rs`, `packet/io.rs`

- **Layout:** a 2048-byte `ReplayHeaderRaw`, then `patch_data_length` bytes of patch, then packets until EOF.
- **Version:**
  - `REPLAY_VERSION = 4`. Readers accept 2..=4 and refuse anything newer.
  - The only version-gated field is `KeyframeMetadata.counters` (`version >= 3`).
- **Spare header space:** `_padding_2` runs from **0x3A8** to 0x7FC (1108 bytes).
  - Several doc comments give wrong offsets. `crop_timer_offset` is really at 0x3A0 and the
    padding starts at 0x3A8. Fix the comments while touching the header.
- **Packets cannot be skipped:** there is no length prefix, and an unknown discriminator fails the parse.
  - **Top level:** that is an error, or the rest of the file is dropped when corruption is allowed.
  - **Inside a blob:** the whole blob is `BrokenPacket`.
- **Free discriminators:** 0x01-0x7F, 0x89-0xEF, 0xF8-0xFD.
- **No trailer or sidecar exists.** The only thing ever rewritten in place is the header
  (`sync_header` → `overwrite_header` on both sinks).
- **Two recorder sinks:**
  - *final sink:* header, patch, completed `CompressedBlob`s.
  - *temp sink:* the same, plus the in-progress blob as raw top-level packets. This is the
    crash-safe copy.

### 2.2 Existing bookmarks

- `Packet::Bookmark` (0xF1) carries `BookmarkMetadata { name, elapsed_frames, elapsed_millis }`.
  The same metadata is duplicated in `CompressedBlob.bookmarks` for indexing.
- **Writer:** `ReplayFileRecorder::add_bookmark(name)` stamps the recorder's own counters. The
  threaded wrapper and `ReplayFileRecorderFns` expose it.
- **Nothing in core, frontend, the C API, Qt or the webserver calls it.** The core ignores the packet on playback.
- **Reader:** `ReplayFilePlayer::all_bookmarks()` returns a `BTreeMap<name, Vec<&BookmarkMetadata>>`.
- **Resume and convert** re-feed bookmarks as `add_bookmark(name)`, so only the name survives and
  frames are re-stamped. Converter verify compares `(name, [(frames, millis)])`.
- **Frame server** flattens them to `(name, frame)` for Cutter's binary `Info` reply and its probe JSON.
- **Once a blob is compressed its bookmarks cannot be edited.** That is why editable bookmarks
  need a new mutable section.

### 2.3 Keyframes and seeking

- **Scheduling:** the core calls `insert_keyframe` every `frames_per_keyframe` frames (default 120),
  from `push_keyframe_if_needed` after a completed frame.
- **Encoding:** the recorder writes a `RegionDeltaKeyframe` whenever it is smaller. A full
  `Keyframe` is written only at a blob start, when the state length changes, or when the delta
  isn't smaller.
  - Blobs close at 54,000 frames (15 min), so a chain holds up to 450 deltas.
  - Masks apply to deltas only.
- **No API forces a full keyframe.**
- **`SuperShuckieCore::go_to_replay_frame(F)`:**
  - Loads the keyframe at or before `F - POST_LOAD_FRAMES`, where `POST_LOAD_FRAMES = 3`.
  - Emulates hidden frames up to `F - 1`, then draws `F`.
  - Afterwards `total_frames == F`.
- **Seek cost:**
  1. Decompressing the whole blob if it isn't cached: measured at 110-175 ms cold on 15-minute NDS chains.
  2. Folding from the last full keyframe at or before the target: up to 450 region diffs, ~20-50 ms on NDS.
  3. Emulating up to ~122 frames.
- **`ReplayFilePlayer::go_to_keyframe(frame)` picks the *first* keyframe packet at that frame.**
  The index is a `BTreeMap<frame, Vec<&KeyframeMetadata>>`.

### 2.4 App plumbing

- **Replay states:**
  - The frontend has `SuperShuckieReplayState { NoReplay, Recording, Playback }`. Recording and
    playback are mutually exclusive.
  - At attach, the parsed `ReplayFilePlayer` moves into the core thread. The frontend can only
    read its metadata before `attach_replay_player`.
  - The frontend reads replay files with `std::fs::read`, so no file handle stays open.
- **Core thread:**
  - Commands are `ThreadCommand`s. The blocking-reply pattern is `MarkReplayStart(Sender<..>)`.
  - Seeks go through atomics (`desired_replay_frame`).
  - The current frame is `ElapsedTimeStats.frames`, which comes from `core.total_frames`.
- **Settings:** a serde `Settings` in `settings.json` (`supershuckie-frontend/src/settings.rs`).
  It is written only at exit (`supershuckie_frontend_write_settings` from `MainWindow::closeEvent`).
- **C API** (`supershuckie-frontend-c`):
  - Headers are written by hand.
  - Lists cross as JSON (`supershuckie_frontend_watch_list_json` + `supershuckie_string_free`)
    with a generation counter (`watch_generation`).
- **Qt:**
  - Menu shortcuts are hard-coded `QAction::setShortcut` calls. Ctrl+B, Ctrl+Shift+B, Ctrl+Alt+B
    and Ctrl+Alt+Shift+B are unused.
  - The closest non-modal window is `RamWatchWindow`: a `QTreeWidget` with the id in `Qt::UserRole`,
    rebuilt when the generation changes.
  - No `QColorDialog` or per-row color exists anywhere yet.
  - `ReplayPlaybackControls` draws no markers.
- **REST server** (`supershuckie-frontend-webserver`):
  - Listens on `127.0.0.1:30158`, enabled by default.
  - `rouille` routes push `SuperShuckieServerCommand`s that `SuperShuckieFrontend::tick` drains.
  - Errors are `{"error": ...}` with 400/404/503; success is 204.
  - Documented in `docs/external_commands.md`, with the JS client in `js/client.js` and `client.d.ts`.

---

## 3. Design overview

A replay's bookmarks exist in up to three forms:

| Form | Where | Written | Purpose |
|---|---|---|---|
| **Bookmark section** | End of the file, at `packet_stream_end` | When the recorder closes; on every edit to a closed replay | Authoritative, editable, read without decompressing anything |
| **`BookmarkTable` snapshot packets** | Inside the packet stream | On every bookmark change while recording, and at the start of each new blob | Crash recovery for files that were never closed |
| Legacy `Bookmark` (0xF1) | v2-v4 files | Never written by v5 | Imported read-only as untyped point bookmarks |

At runtime, the **frontend owns the bookmark table** for the current replay:

```
                 hotkey / REST / window
                          │
                 SuperShuckieFrontend (ReplayBookmarks: table + generation)
                   │ Recording                          │ Playback
    ThreadCommand::SetReplayBookmarks(table)   bookmark writer thread
                   │                                    │
    recorder.set_bookmark_table(table)         write_bookmark_section(path, table)
      → BookmarkTable packet in the stream       → section at end of file
      → section written at close()
```

- **Whole-table replacement:** every change sends the whole table, never a diff. That keeps the
  recorder, the frontend and the file trivially consistent. Tables are small: ~60 bytes per bookmark.
- **Frame stamps come from the core thread.** A "now" bookmark asks the core for an anchor
  (`ThreadCommand::BookmarkAnchor`). The core is also where a keyframe bookmark's keyframe is
  forced (recording) or snapped (playback).

---

## 4. Data model — new module `supershuckie-replay-recorder/src/bookmarks.rs`

```rust
/// Frames a keyframe bookmark sits after its keyframe. Must equal SuperShuckieCore::POST_LOAD_FRAMES
/// (the core asserts this at compile time), so that go_to_replay_frame(in_frame) loads exactly that keyframe.
pub const KEYFRAME_BOOKMARK_LEAD_FRAMES: u64 = 3;

pub struct Bookmark {
    pub id: u64,                                  // unique within the replay, never reused
    pub name: String,
    pub type_id: u64,                             // 0 = untyped
    pub in_frame: UnsignedInteger,
    pub in_millis: TimestampMillis,
    pub out: Option<(UnsignedInteger, TimestampMillis)>, // Some = range; out frame >= in_frame
    pub keyframe: bool,                           // a keyframe sits at in_frame - KEYFRAME_BOOKMARK_LEAD_FRAMES
}

pub struct BookmarkTypeRecord {
    pub id: u64,                                  // non-zero, random, shared with the user's settings
    pub name: String,
    pub color: u32,                               // 0xRRGGBB
}

pub struct BookmarkTable {
    pub next_id: u64,
    pub types: Vec<BookmarkTypeRecord>,           // exactly the types referenced by `bookmarks`
    pub bookmarks: Vec<Bookmark>,                 // kept sorted by (in_frame, id)
}
```

**Helpers:**
- `insert`, `get`, `get_mut`, `remove`, `sort`.
- `truncated_to(frame)`: drops bookmarks with `in_frame > frame` and clears `out` where the out frame is past `frame`.
- `prune_types`.
- `from_legacy(&BTreeMap<String, Vec<&BookmarkMetadata>>)`:
  - Ids run `1..` in `(frame, name)` order.
  - Bookmarks come out untyped, as points, and not keyframe bookmarks.

**Frame semantics.** Frame *F* is the frame counter's value (`/stats` `total_elapsed_frames`,
status bar) while frame *F* is on screen. `go_to_replay_frame(F)` shows the same image.

`in_millis` / out millis are the replay's elapsed time at that frame:
- **Exact** for "now" bookmarks.
- **The keyframe's time** for keyframe bookmarks, 3 frames early.
- **Estimated** for explicit frames (§9.1).

**Types.**
- **Ids:** a type is created in the user's settings (§10.4) with a random non-zero id.
- **Snapshot:** each replay stores a `{id, name, color}` record for every type its bookmarks use.
- **Display resolution, first match wins:**
  1. A settings type with the same id supplies name and color, so renames and recolors apply to every replay.
  2. Otherwise the replay's record is used, e.g. a replay from another machine.
  3. Otherwise the bookmark shows as untyped.
- **Refresh:** when the frontend saves a table, it refreshes `types` from settings for every
  referenced id and drops unreferenced records.

**Encoding** (new `PacketIO` impls, same primitives as packets):

```
BookmarkTable      := u32 table_format (= 1) · next_id · Vec<ByteVec> types · Vec<ByteVec> bookmarks
BookmarkTypeRecord := id · name · u32 color
Bookmark           := id · name · type_id · in_frame · in_millis · u64 flags · out_frame · out_millis
                      flags: bit 0 = has out, bit 1 = keyframe; out_* are 0 when there is no out
```

- **Forward compatibility:** each record is wrapped in a `ByteVec`, so a reader parses the fields
  it knows and ignores trailing bytes. Future fields (notes, tags) can be appended without a
  format bump.
- **Unknown flag bits:** readers ignore them and writers write 0.
- **Known loss:** a v5 build that edits a newer table drops fields it doesn't know. This is documented, not handled.

---

## 5. Replay format v5

### 5.1 Version

- Set `REPLAY_VERSION = 5` and accept 2..=5.
- Extend the version doc comment in `header.rs`: v5 adds `packet_stream_end`, the bookmark
  section and `BookmarkTable` packets.
- Older builds refuse v5 files. That is the point of the bump, because they would otherwise choke
  on 0xF8 or on the section bytes.

### 5.2 Header field

Add at **0x3A8** `packet_stream_end: u64`. It is the absolute offset where the packet stream ends
and the bookmark section begins.

- **0** means the stream runs to EOF: a recording in progress, a crash file, or a pre-v5 file.
- `_padding_2` shrinks to 1100 bytes.
- Read the field only when `replay_version >= 5`. Older files have zeros there anyway.
- **It is structural, not metadata:** it does not go into `ReplayFileMetadata`.
  `ReplayFileMetadata::as_raw_header` writes 0, and the recorder sets the real value at close (§5.5).
  Resume and convert copy metadata, so they can never inherit a stale offset.

### 5.3 Packets

- **New:** `BookmarkTable = 0xF8` → `Packet::BookmarkTable { table: BookmarkTable }`.
- **Legacy:** `Packet::Bookmark` (0xF1) and `CompressedBlob.bookmarks` are still read.
  - The v5 writer never emits 0xF1 and always writes an empty `bookmarks` vector.
  - Remove `add_bookmark` from `ReplayFileRecorder`, `ReplayFileRecorderFns` and
    `NonBlockingReplayFileRecorder` (§7.1 replaces it).

### 5.4 Bookmark section

Located at `packet_stream_end`:

```
+0x00  [4]            magic "SSBM"
+0x04  u32            section_format = 1
+0x08  u64            payload_len
+0x10  [32]           blake3(payload)            (util::blake3_hash)
+0x30  [payload_len]  BookmarkTable encoding
EOF must equal packet_stream_end + 0x30 + payload_len
```

The section is **valid** only if all of these hold:
- The magic is correct.
- `section_format == 1`.
- The file length matches exactly.
- The hash matches.
- The payload parses.

Anything else is invalid and falls back per §5.6. The replay itself stays playable either way.

### 5.5 Write ordering

**At recorder `close()`:**

1. Flush the last blob to the final sink (existing behavior).
2. Set `packet_stream_end` to the final sink's length and `sync_header`. The recorder must track
   the final sink's byte length; it already tracks offsets for the temp sink.
3. Append the section to the final sink and flush.

A crash between steps 2 and 3 leaves the header pointing at a missing or partial section. That
section is invalid, so the player falls back to the in-stream snapshot, and the packets are intact.
Never write the section before the header: a strict parse would then read section bytes as packets.

**When editing a closed file**, `write_bookmark_section(path, expected_header, table)`:

1. Open the file read+write and read its header.
   - Refuse with `HeaderChanged` unless the header equals `expected_header` (captured at load),
     ignoring `replay_version` and `packet_stream_end`.
   - Refuse if the file is shorter than `packet_stream_end`. Either case means the file changed on disk.
2. If `replay_version < 5` or `packet_stream_end == 0`, upgrade it (§5.7): set version 5 and
   `packet_stream_end = file length`, then write the header.
3. Write the section at `packet_stream_end`, `set_len(packet_stream_end + section length)`, then `sync_data`.

A crash during step 3 invalidates the section. Playback falls back to the stream snapshot or the
legacy bookmarks, so only edits made since the last successful write are lost. Journaled writes
are a follow-up (§16).

### 5.6 Player — resolving the table at load (`ReplayFilePlayer::new`)

1. **Section:** if `version >= 5 && packet_stream_end != 0`:
   - Packet parsing stops at `packet_stream_end`. A file shorter than that is `BrokenPacket`, or
     truncated when corruption is allowed.
   - If the section is valid → `BookmarkTableSource::Section`.
2. **Stream snapshot:** otherwise, if `version >= 5`, take the newest snapshot:
   - Reverse-scan the top-level raw packets.
   - If none are found there, decompress **only the last `CompressedBlob`** and take its last
     `BookmarkTable` → `StreamSnapshot`.
   - §7.2 guarantees the newest table is in one of those two places, so never scan further back.
3. **Legacy:** otherwise, if the legacy index is non-empty → `Legacy` (`BookmarkTable::from_legacy`).
4. **None:** otherwise `None` (empty table, `next_id = 1`).

New API:
- `bookmark_table(&self) -> &BookmarkTable`
- `bookmark_table_source() -> BookmarkTableSource`
- `stream_truncated() -> bool`: true when `allow_some_corruption` dropped data.
- `raw_header_bytes() -> ReplayHeaderBytes`

Rename `all_bookmarks()` to `legacy_bookmarks()` and move its callers (frame server, converter verify, tests) to `bookmark_table()`.

### 5.7 Upgrading a pre-v5 replay on its first bookmark edit

- **v3 and v4 are allowed.**
  - The header version becomes 5, `packet_stream_end` is set to the file length, and the section is appended.
  - v3/v4 packets parse identically under a v5 header, because only `counters` (≥ 3) is version-gated.
  - Afterwards older builds refuse the file. Qt asks first (§13.5).
- **v2 is refused.** Its `KeyframeMetadata` has no `counters`, so it would misparse under a v5
  header. Error: "Convert this replay to the current format before adding bookmarks."
- **A replay loaded with `stream_truncated()` is refused.** Error: "This replay is damaged; convert it before adding bookmarks."

### 5.8 Several keyframes on one frame

A forced full keyframe (§6) can land on the same frame as the scheduled keyframe written just
before it. Change `go_to_keyframe` to resolve a frame to the **last** keyframe-class packet at that frame:

- Pick the last blob whose `keyframes` contain the frame.
- Within the chosen list, use `rposition` instead of `position`.
- At top level, take the last match.

`fast_forward_chain` then restarts at the full keyframe itself, so there are zero folds.

---

## 6. Keyframe bookmarks

### While recording

A request arrives at frame *F*.

1. The core creates a save state and calls `insert_keyframe_full(state, ms)`.
   - This always writes a full `Keyframe`.
   - It still honors the blob limit, replaces `last_state_to_diff` and resets `frames_since_last_keyframe`.
2. The bookmark gets `in_frame = F + 3`, `in_millis` = the time at *F*, and `keyframe = true`.

**Seek.** Jumping to it is the ordinary `go_to_replay_frame(F + 3)`. That loads the keyframe at
*F* (full, via §5.8, so zero folds) and emulates 3 frames.
- **Cost:** decompressing the blob if it isn't cached, one state copy and 3 frames.
- **Compared with today:** up to 450 folds plus ~122 frames.
- **Why F + 3:** the jump lands 3 frames (50 ms at 60 fps) after the press. Screens hold stale
  content right after `load_save_state`, and `POST_LOAD_FRAMES` guards against masked stale
  buffers, so this keeps a single seek path.

**File cost.** One full save state per keyframe bookmark before blob compression. That is ~20 MB
uncompressed on NDS (per the comment in `attach_replay_player`) and small on GB/GBA.
- Measure the compressed cost on an NDS recording in phase 1 and report it in the PR.
- Later deltas diff against the full state, so the rest of the chain is unaffected.

**Mid-frame requests.** If `mid_frame` is set, the full keyframe is marked pending and written when
that frame completes (`full_keyframe_pending`, consumed by `push_keyframe_if_needed`); the bookmark
counts from that frame. Nothing is emulated to place a bookmark: a paused Game Boy core does sit
mid-frame, and finishing the frame while the timer is paused would record it with a timestamp that
the following frames go back from, which stops the recording. The wrapper uses a 500 ms
`recv_timeout`, so the UI never waits on an unpause.

### During playback (snap)

A request arrives at frame *F*.

- Let *K* be the greatest keyframe frame `<= F.saturating_sub(3)` from `player.all_keyframes()`.
- The bookmark gets `in_frame = K + 3`, `in_millis` = that keyframe's `elapsed_millis`, and `keyframe = true`.
- **Seek cost:** blob decompression, folds up to *K* (none if *K* is full) and 3 frames. There is no long re-emulation.
- **Offset:** the bookmark lands up to `frames_per_keyframe` frames (~2 s) before *F*.

### Other rules

- **Resume and convert keep anchor keyframes full** (§8).
- **Deleting** a keyframe bookmark leaves its keyframe in the stream, which is harmless.
- **Editing `in_frame`** of a keyframe bookmark clears `keyframe`.

---

## 7. Recorder crate

### 7.1 API

**`ReplayFileRecorder`:**
- `bookmark_table(&self) -> &BookmarkTable`
- `set_bookmark_table(&mut self, table) -> Result<(), ReplayFileWriteError>`
  - A no-op if the table is equal to the current one.
  - Otherwise it stores the table, writes `Packet::BookmarkTable` (to the current blob and the temp
    sink, like any packet) and sets `bookmarks_written = true`.
- `insert_keyframe_with(state, ms, KeyframeEncoding::{Auto, Full}) -> Result<u64, _>`
  - `insert_keyframe` becomes `Auto`.
- `close()` follows §5.5.

**Wrappers:**
- `ReplayFileRecorderFns` swaps `add_bookmark` for `set_bookmark_table` and gains `insert_keyframe_full`.
- `NonBlockingReplayFileRecorder` gains the commands `SetBookmarkTable(BookmarkTable)` and `InsertKeyframe { full: bool, .. }`.

**New module `replay_file/bookmark_section.rs`:**
- `read_bookmark_section(bytes, header) -> Result<BookmarkTable, SectionError>`
- `write_bookmark_section(path, expected_header, table) -> Result<SectionWriteOutcome { upgraded_from: Option<u32> }, BookmarkWriteError>`
- `BookmarkWriteError` variants: `Io`, `HeaderChanged`, `NeedsConversion { version }`, `Damaged`.

### 7.2 Snapshot invariant

After writing the **first keyframe of a new blob**, if `bookmarks_written` is set, write the
current table as a `BookmarkTable` packet.

- **Consequence:** the newest table is always either in the raw tail or in the last blob, which is
  what §5.6 step 2 relies on.
- **Blob starts:** a snapshot is never the first packet of a blob, because blobs must start with a
  full `Keyframe`.
- **Resume:** the seed snapshot (§8) is written after the re-feed's first keyframe for the same reason.

### 7.3 Rate

Each change writes the whole table. The frontend coalesces changes to at most one
`SetReplayBookmarks` per tick (§10.3), so an API client in a tight loop cannot flood the stream.

---

## 8. Resume and convert (`record/resume.rs`, `replay_file/convert.rs`)

**`build_resumed_recorder`:**
- Gains `bookmarks: Option<BookmarkTable>`. `None` means `source.bookmark_table()`. The frontend
  passes its own table so that unsaved playback edits carry over.
- The seed is `table.truncated_to(target)`.

**`prime_and_refeed`:**
- Ignores `Packet::BookmarkTable` and `Packet::Bookmark`.
- Calls `set_bookmark_table(seed)` right after the first keyframe it writes.
- If the fast path copies every blob and re-feeds nothing, it writes the seed after the first
  keyframe of the new in-progress blob.

**Anchor preservation:**
- A re-fed source keyframe at frame *K* is inserted with `KeyframeEncoding::Full` when the seed
  holds a keyframe bookmark with `in_frame - KEYFRAME_BOOKMARK_LEAD_FRAMES == K`.
- Verbatim-copied blobs keep their full keyframes unchanged.

**`build_reencoded_recorder`** (converter) seeds the full table without truncation and applies the same preservation.

**`verify_replay_files`:**
- Compares `bookmark_table()` for equality across all fields and types.
- Skips `BookmarkTable` and `Bookmark` packets in the packet-by-packet comparison.
- Checks that every keyframe bookmark's anchor is a full `Keyframe` in the output.

A v2-v4 source converts to v5 with its legacy bookmarks written as a section.

---

## 9. Core (`supershuckie-core`)

### 9.1 `SuperShuckieCore`

**`bookmark_anchor(&mut self, keyframe: bool) -> Result<BookmarkAnchor, BookmarkAnchorError>`**
- `BookmarkAnchor { in_frame, in_millis, keyframe }`.
- Recording: `(total_frames, total_milliseconds)`, or the keyframe path from §6.
- Playback: `(total_frames, total_milliseconds)`, or the snap from §6.
- Neither: `Err(NoReplay)`.

**`estimate_millis_at(frame) -> TimestampMillis`**, for bookmarks at explicit frames:
- Recording: `total_milliseconds - (total_frames - frame) × frame period`.
- Playback: the nearest keyframe at or before `frame`, plus `(frame - K) × frame period`.
- Clamp at 0.

**Other changes:**
- `set_replay_bookmarks(&mut self, table)`: calls `with_recorder(|r| r.set_bookmark_table(table))` while recording, and is ignored otherwise.
- `handle_replay` adds a `Packet::BookmarkTable { .. } => {}` arm.
- `resume_recording_replay` gains `bookmarks: BookmarkTable` and passes it to `build_resumed_recorder`.
- Add `const _: () = assert!(Self::POST_LOAD_FRAMES == KEYFRAME_BOOKMARK_LEAD_FRAMES);`.

### 9.2 `ThreadedSuperShuckieCore` (`thread.rs`)

**New commands:**
- `ThreadCommand::BookmarkAnchor { keyframe, reply: Sender<Result<BookmarkAnchor, BookmarkAnchorError>> }`
- `ThreadCommand::EstimateMillisAt(UnsignedInteger, Sender<TimestampMillis>)`
- `ThreadCommand::SetReplayBookmarks(BookmarkTable)`

**Changed:** `ResumeRecordingReplay` carries `bookmarks`.

**Wrapper methods** follow the `mark_start` pattern but use `recv_timeout(500 ms)` and map a timeout to an error:
- `bookmark_anchor(keyframe)`
- `estimate_millis_at(frame)`
- `set_replay_bookmarks(table)`

### 9.3 Smoke test

Add `examples/bookmark_smoke.rs`, modeled on `core_smoke.rs` with the same ROM argument.

1. Record, take a keyframe bookmark mid-chain, then stop.
2. Reload and seek to the bookmark.
3. Assert the load folded **zero** deltas. Add a `last_seek_fold_count()` debug accessor on the player.
4. Assert exactly 3 frames were emulated.
5. Print the seek time next to an ordinary seek to a frame 110 frames past a scheduled keyframe.

---

## 10. Frontend (`supershuckie-frontend`)

### 10.1 New module `bookmarks.rs`

```rust
pub struct ReplayBookmarks {
    table: BookmarkTable,
    generation: u64,                 // bumped on every change, replay switch and type change
    target: BookmarkTarget,          // None | Recording | Playback { path, header: ReplayHeaderBytes, version, source, truncated }
    open_range: Option<u64>,         // the range started by toggle_range_bookmark
    core_dirty: bool,                // Recording: send SetReplayBookmarks on the next tick
    write_error: Option<String>,     // last playback write failure, shown in UI and /bookmarks
}
```

**Playback writer:** one background thread with a latest-wins slot `(path, header, table)`.
- It writes 250 ms after the last change.
- It reports its result back through a channel that `tick` drains.
- **Synchronous flush** happens when:
  - playback stops,
  - another replay or ROM is loaded,
  - recording resumes from this replay,
  - the app exits.
- **If a flush fails at stop:** Qt shows a warning with the error.

### 10.2 Lifecycle

- **`load_replay_if_exists`:**
  - Before `attach_replay_player`, capture `player.bookmark_table().clone()`, the source, version,
    `raw_header_bytes()` and `stream_truncated()`.
  - Target Playback; bump the generation.
- **`start_recording_replay`:** empty table (`next_id = 1`), target Recording.
- **`resume_recording_from_replay`:** flush, then pass `table.truncated_to(N)` to the core. Target Recording.
- **`stop_recording_replay` / `stop_replay_playback`:** flush if needed, clear the table and
  `open_range`, target None, bump the generation.

### 10.3 Operations

All operations return `Result<_, BookmarkError>` with user-facing messages.

**`add_bookmark(NewBookmark) -> Bookmark`.** Fields: `name: Option<String>`,
`type_ref: Option<TypeRef>`, `in_frame: Option<u64>`, `out: Option<OutSpec>`, `keyframe: bool`,
`allow_upgrade: bool`.
- **`keyframe` with an explicit `in_frame`** → `InvalidRequest`.
- **`in_frame: None`** → `core.bookmark_anchor(keyframe)`.
- **`in_frame: Some(f)`:**
  - Must be `<= current frame` while recording, or `<= total frames` in playback.
  - `in_millis` comes from `core.estimate_millis_at(f)`.
- **`name: None`** → a generic name:
  - `"<type name> N"`, or `"Bookmark N"` when untyped.
  - *N* is 1 + the largest *N* among existing names with that prefix.
- **`type_ref: None`** → `settings.bookmarks.active_type`.
- **`out`:** `OutSpec::Now` or `OutSpec::Frame(u64)`, and must be `>= in_frame`.

**`update_bookmark(id, BookmarkPatch) -> Bookmark`.** Fields: `name`, `type_ref` (`Some(None)`
clears it), `in_frame`, `out: Option<Option<OutSpec>>`, `allow_upgrade`.

**Other operations:**
- `delete_bookmark(id, allow_upgrade)`
- `toggle_range_bookmark(name, type_ref, keyframe, allow_upgrade) -> (Bookmark, started: bool)`
  - If `open_range` names an existing bookmark, set its out to now and clear `open_range`.
  - Otherwise create a bookmark with no out and remember it.
- `go_to_bookmark(id, BookmarkPoint::{In, Out})`
  - Playback only; otherwise `WrongState("Seeking to bookmarks works during playback")`.
  - Calls `go_to_replay_frame`.
- Getters: `bookmarks()`, `bookmark_generation()`, and `bookmarks_view()` (the resolved JSON view, §11.1).

**After any change:**
- Sort the table and refresh its type records.
- Bump the generation.
- Recording: set `core_dirty`, so `tick` sends at most one `SetReplayBookmarks` per tick.
- Playback: schedule a write.

**Upgrade gate** (playback only):
- If `version < 5`, `confirm_replay_upgrade` is on and `allow_upgrade` is false → `NeedsUpgradeConfirmation`.
- If `version == 2` → `NeedsConversion`.
- If `truncated` → `Damaged`.
- REST calls always pass `allow_upgrade = true`.

### 10.4 Settings (`settings.rs`)

```rust
pub struct BookmarkSettings {
    pub types: Vec<BookmarkTypeSetting>,   // { id: u64, name: String, color: String /* "#RRGGBB" */ }
    pub active_type: u64,                  // 0 = untyped
    pub confirm_replay_upgrade: bool,      // default true
}
```

- **Registration:** add it as `Settings.bookmarks` with the usual `#[serde(default = ...)]`. Seed no types.
- **Operations:** `create_bookmark_type(name, color) -> id`, `update_bookmark_type`, `delete_bookmark_type`.
  - Every type change bumps the bookmark generation.
  - Every type change calls `write_config()` immediately, so types survive a crash.
- **Deleting a type:** its bookmarks keep the id and display from the replay's record.
- **Ids:** random non-zero u64. Use an existing RNG dependency if one exists; otherwise blake3 over
  `(SystemTime::now(), process id, a counter)`.
- **Lookup by name** (REST):
  1. A case-insensitive match in settings.
  2. Otherwise, the current table's records, adopting that id and color into settings.
  3. Otherwise, create the type with the next color from a golden-angle hue sequence (saturation
     0.65, value 0.9).

---

## 11. External commands (REST, `127.0.0.1:30158`)

### 11.1 JSON

Bookmark:

```json
{ "id": 7, "name": "Bookmark 3",
  "type": { "id": "9f3c01a2b4c5d6e7", "name": "Death", "color": "#E53935" },
  "in_frame": 51234, "in_millis": 853900, "out_frame": null, "out_millis": null, "keyframe": false }
```

- `type` is `null` when untyped.
- Type ids are 16-digit hex strings, because JS numbers cannot hold a u64. Bookmark ids are small
  sequential numbers.

### 11.2 Routes

**Status codes:**
- New routes return **200 JSON** on success; `delete-bookmark` and `go-to-bookmark` return 204.
- Errors keep the existing `{"error": ...}` body:
  - **400:** bad or missing parameter.
  - **404:** no replay, or unknown id.
  - **409:** wrong state, e.g. `keyframe` with `frame`, seeking while recording, or a replay that needs conversion or is damaged.
  - **503:** emulator unavailable.

| Route | Parameters | Success |
|---|---|---|
| `/bookmarks` | — | `{ replay, state: "none"\|"recording"\|"playback", generation, open_range, write_error, bookmarks: [...], types: [...] }` |
| `/add-bookmark` | `name`?, `type`? (name), `type_id`? (hex), `frame`? (default: now), `out`? (`now` or a frame), `keyframe`? (default `false`) | bookmark |
| `/update-bookmark` | `id`, `name`?, `type`?/`type_id`? (`type=none` clears), `frame`?, `out`? (`now`, a frame, or `none`) | bookmark |
| `/delete-bookmark` | `id` | 204 |
| `/toggle-range-bookmark` | `name`?, `type`?/`type_id`?, `keyframe`? | `{ bookmark, started }` |
| `/go-to-bookmark` | `id`, `point`? (`in`\|`out`, default `in`) | 204 |

- `/stats` gains `bookmark_generation`, so clients can poll cheaply and fetch `/bookmarks` only on change.
- **Webserver commands:** add `SuperShuckieServerCommand::{ListBookmarks, AddBookmark, UpdateBookmark,
  DeleteBookmark, ToggleRangeBookmark, GoToBookmark}`, each with a
  `Sender<Result<serde_json::Value, (u16, String)>>`. The frontend handles them in `tick`.
- **Tick blocking:** an anchor request blocks `tick` for at most one frame, the same as `mark-start` does today.

### 11.3 Docs and client

- **Docs:** add a section per route to `docs/external_commands.md`.
- **JS client:** add `bookmarks()`, `add_bookmark(options)`, `update_bookmark(id, options)`,
  `delete_bookmark(id)`, `toggle_range_bookmark(options)` and `go_to_bookmark(id, point)` to
  `js/client.js`, with types in `client.d.ts`.

---

## 12. C API (`supershuckie-frontend-c`)

New `src/bookmarks.rs` and a hand-written `include/supershuckie/bookmarks.h`, included from `supershuckie.h`. JSON crosses the boundary, as with the watch list.

```c
typedef enum { SUPERSHUCKIE_BOOKMARK_OK = 0, SUPERSHUCKIE_BOOKMARK_ERROR = 1,
               SUPERSHUCKIE_BOOKMARK_NEEDS_UPGRADE_CONFIRMATION = 2 } SuperShuckieBookmarkResult;

uint64_t supershuckie_frontend_bookmark_generation(SuperShuckieFrontendRaw *frontend);
char *supershuckie_frontend_bookmarks_json(SuperShuckieFrontendRaw *frontend);        /* free: supershuckie_string_free; /bookmarks schema */

/* request/patch JSON mirror the REST parameters; out buffer receives bookmark JSON or the error text */
SuperShuckieBookmarkResult supershuckie_frontend_bookmark_add_json(SuperShuckieFrontendRaw *frontend, const char *request_json,
                                                                   bool allow_upgrade, char *out, size_t out_len);
SuperShuckieBookmarkResult supershuckie_frontend_bookmark_update_json(SuperShuckieFrontendRaw *frontend, uint64_t id, const char *patch_json,
                                                                      bool allow_upgrade, char *out, size_t out_len);
SuperShuckieBookmarkResult supershuckie_frontend_bookmark_delete(SuperShuckieFrontendRaw *frontend, uint64_t id,
                                                                 bool allow_upgrade, char *out, size_t out_len);
SuperShuckieBookmarkResult supershuckie_frontend_bookmark_toggle_range(SuperShuckieFrontendRaw *frontend, bool keyframe,
                                                                       bool allow_upgrade, char *out, size_t out_len);
bool supershuckie_frontend_bookmark_go_to(SuperShuckieFrontendRaw *frontend, uint64_t id, bool out_point, char *error, size_t error_len);
bool supershuckie_frontend_bookmark_flush(SuperShuckieFrontendRaw *frontend, char *error, size_t error_len);

char *supershuckie_frontend_bookmark_types_json(SuperShuckieFrontendRaw *frontend);
bool supershuckie_frontend_bookmark_type_upsert_json(SuperShuckieFrontendRaw *frontend, const char *type_json, char *out, size_t out_len);
bool supershuckie_frontend_bookmark_type_delete(SuperShuckieFrontendRaw *frontend, const char *type_id_hex);
const char *supershuckie_frontend_bookmark_active_type(SuperShuckieFrontendRaw *frontend);   /* hex or "" */
void supershuckie_frontend_set_bookmark_active_type(SuperShuckieFrontendRaw *frontend, const char *type_id_hex);
bool supershuckie_frontend_bookmark_confirm_upgrade(SuperShuckieFrontendRaw *frontend);
void supershuckie_frontend_set_bookmark_confirm_upgrade(SuperShuckieFrontendRaw *frontend, bool confirm);
```

---

## 13. Qt UI (`supershuckie-qt`)

### 13.1 Menu and shortcuts

Add a bookmarks block to the Replays menu, after "Continue last replay":

| Action | Shortcut |
|---|---|
| Add bookmark | Ctrl+B |
| Add keyframe bookmark | Ctrl+Shift+B |
| Start/end range bookmark | Ctrl+Alt+B |
| Add bookmark at frame… | — |
| Bookmarks… (window) | Ctrl+Alt+Shift+B |

- **Enabled state:** actions are enabled only in the Recording or Playback state (`refresh_action_states`).
- **Shortcut context:** use `Qt::ApplicationShortcut`, so the shortcuts also fire while the bookmark window has focus.
- **Names and types:** new bookmarks get a generic name and the active type.
- **Feedback:** show a 2-second status bar message, e.g. "Bookmark 3 added at frame 51234" or "Range started…".

### 13.2 `BookmarkWindow` (`bookmark_window.cpp/.hpp`, added to `CMakeLists.txt`)

**Window:** `QWidget(main_window, Qt::Window)`.
- Non-modal and created lazily.
- Visibility and geometry persist in custom setting `qt__bookmark_window` (`visible|base64 geometry`), like the RAM tools.

**Layout, top to bottom:**
1. "Type for new bookmarks:" `QComboBox` (Untyped plus each type with a swatch icon) and a **Types…** button.
2. Buttons: **Add**, **Add keyframe**, **Start/end range**, **Set out to current**, **Delete**.
3. A `QTreeWidget` with columns **Type**, **Name**, **In**, **Out**, **Duration**, **KF**.
   - Type shows a swatch and the name.
   - In and Out show the frame and `m:ss.mmm`.
   - KF shows ◆ for keyframe bookmarks.
   - Rows are sorted by in frame, with the bookmark id in `Qt::UserRole`.
4. A status label: replay name, state, count, the last write error, or "Seeking works during playback" while recording.

**Row color:**
- A 12×12 swatch icon in the Type column.
- Each column's background set to the type color at ~20% alpha. Pick alpha so text stays legible in the light theme and in the Windows dark palette (`theme.cpp`).
- Untyped rows get no tint.

**Interaction:**
- **Single left-click** (`itemClicked(item, column)`) seeks during playback. Clicking the Out cell goes to the out frame; any other cell goes to the in frame.
- **Double-click Name** edits it inline. Only the Name column is editable; `itemChanged` calls update.
- **Context menu:** Go to in · Go to out · Rename · Type ▸ · Set in to current frame · Set out to current frame · Clear out · Delete.
- **Delete key** deletes the selected rows.

**Refresh:** `MainWindow::tick` calls `bookmark_window->tick()` while the window is visible. When
the generation changes, the window rebuilds from `bookmarks_json`, keeping the selection and scroll position.

### 13.3 `BookmarkTypesDialog`

A modal dialog using the `stop_timer`/`start_timer` `exec()` pattern.
- **List:** a `QListWidget` of types with swatches.
- **Actions:**
  - **Add:** prompt for a name, then `QColorDialog`.
  - **Rename**
  - **Color…:** `QColorDialog`.
  - **Delete:** confirm with "Bookmarks of this type keep the color saved in each replay."
- **Commit:** changes go through the type FFI and are saved immediately.

### 13.4 `AddBookmarkDialog` ("Add bookmark at frame…")

- **Fields:**
  - Name (the placeholder shows the generic name)
  - Type combo
  - In frame (defaults to current)
  - Optional out frame (checkbox + spin box)
  - A **Keyframe bookmark** checkbox
- **Keyframe checkbox:** enabled only while the in frame equals the current frame.

### 13.5 Upgrade confirmation

On `NEEDS_UPGRADE_CONFIRMATION`, show a `QMessageBox`:

> Saving bookmarks into this replay upgrades it to replay format v5. Older Super Shuckie builds will not be able to open it.

- **Buttons:** Upgrade / Cancel.
- **"Don't ask again":** a checkbox that sets `confirm_replay_upgrade = false`.
- **On Upgrade:** retry the operation with `allow_upgrade = true`.
- **Errors:** show `NeedsConversion` and `Damaged` in a plain warning box.

### 13.6 Optional phase — bindable controls

- Add `Control::{AddBookmark, AddKeyframeBookmark, ToggleRangeBookmark}`, unbound by default, so
  they can be bound to keys or gamepad buttons in the Controls dialog.
- Every exhaustive match needs updating: `is_button`, `set_for_input`, `invert_for_input`,
  `as_c_str`, `is_available_for_emulator_type` and `on_user_input`.

---

## 14. Frame server (`supershuckie-frame-server`)

- **`source.rs`:** build bookmarks from `player.bookmark_table()` as `(name, in_frame)`, sorted by
  `(frame, name)`. The binary `Info` layout does not change, because Cutter owns that protocol.
- **`probe.rs`:** each JSON entry gains `id`, `out_frame`, `keyframe` and `type` (`{name, color}`
  from the replay's records, since the frame server has no user settings). This is additive only.
- **`docs/frame_server.md`:** mention the new probe fields.

---

## 15. Edge cases

- **Seek requested while recording:** window clicks do nothing and show a status message; REST returns 409.
- **Keyframe bookmark:**
  - With an explicit frame: 400 over REST; the Qt checkbox is disabled.
  - With no replay: `NoReplay` → 404.
  - A mid-frame request finishes the frame first (§6). An anchor timeout (500 ms) is an error, never a hang.
- **Out frame:**
  - An out frame before the in frame is 400.
  - Moving the in frame past the out frame is 400.
- **Open range:**
  - An open range at stop stays a point bookmark, and `open_range` is cleared.
  - Deleting the open range clears `open_range`.
- **Bursts of changes while recording:** coalesced to one snapshot per tick (§7.3).
- **Playback write failure** (the file is locked by another program, e.g. on Windows, or the media is read-only):
  - Edits stay in memory, and `write_error` shows in the window and in `/bookmarks`.
  - The write is retried on the next change and at flush.
  - If it still fails at stop, warn the user.
- **File changed on disk since load** (header mismatch, or shorter than `packet_stream_end`): refuse and show the error.
- **Refused replays:** v2 → "convert first"; a truncated or damaged replay → refused (§5.7).
- **Two instances editing one replay:** last writer wins. Documented, not handled.
- **Resume:** bookmarks after the resume frame are dropped, and ranges crossing it lose their out frame (§8).
- **Temp files** contain snapshots. The frontend still deletes `temp-*.replay` at stop and zero-frame recordings as today.
- **Types:**
  - A type deleted from settings displays from the replay's record.
  - The same type id with a different color in the replay → the settings color wins.
- **Crop and timer markers** are unaffected.

---

## 16. Out of scope (follow-ups)

- Bookmark ticks and range spans on `ReplayPlaybackControls`, with hover names.
- Highlighting the bookmark at the playback position in the window.
- Journaled section writes that survive power loss during an edit.
- A Cutter frame-server protocol revision carrying ranges, types and colors.
- Editing bookmarks of replays that aren't loaded (a replay browser).
- Exact-frame keyframe bookmarks during playback (would need a stored save state).
- Bookmark notes; CSV/JSON import and export.
- Prefetching blobs that hold keyframe bookmarks, so the first jump is warm.

---

## 17. Testing plan

**Recorder** (`supershuckie-replay-recorder`, extend `test_support.rs`):

1. **Table codec:** `BookmarkTable` round-trips. Extra trailing bytes in a record are ignored, and unknown flag bits are ignored.
2. **Section validity:** a valid section reads back. Each corruption (magic, format, length
   mismatch, hash, truncation) → invalid → fallback source.
3. **Close ordering:** truncate a closed file at `packet_stream_end`. It resolves to
   `StreamSnapshot` with the latest table and plays back exactly.
4. **Snapshot invariant:** a script changes the table across at least 3 blobs.
   - The temp layout and a closed-without-section layout both resolve to the latest table.
   - Only one blob is decompressed (count it).
5. **Existing coverage:** `check_script_replay` passes for every layout, with expected packets updated for `BookmarkTable`.
6. **Legacy:** the v3 fixtures resolve to `Legacy` `{checkpoint@7, late@76, checkpoint@165}`.
7. **Upgrade:**
   - Both v3 fixtures upgrade to v5 in place, still play back exactly, and read back the written section.
   - A v2-header file is refused. Build it with `file_with_packets`.
8. **Rewrites:** section rewrites that grow and shrink both work; a changed header is refused.
9. **Duplicate-frame keyframe:** a forced full keyframe on the same frame as a scheduled one.
   `go_to_keyframe` yields the full one with zero folds.
10. **Resume:** truncation rules; the seed snapshot comes after the first keyframe; anchors stay full
    on both the fast path and the re-feed path.
11. **Convert:** `convert` + `verify` on the v3 fixtures and on a v5 file with keyframe bookmarks:
    tables are equal and anchors are full.

**Core:** `examples/bookmark_smoke.rs` (§9.3).

**Frontend:** unit tests for:
- generic naming
- range toggling
- type resolution and auto-create
- `truncated_to`
- the upgrade gate
- writer flush and error reporting (use a temp dir)

**Webserver:** parameter parsing for each route. Exercise success and error paths manually with `curl`.

**Qt manual checklist:** each acceptance criterion below, on macOS and on the Windows static build.

---

## 18. Acceptance criteria

1. **Generic "now" bookmarks:** Ctrl+B while recording and while playing back adds "Bookmark N"
   at the current frame. `/add-bookmark?name=X&frame=F` adds "X" at *F*.
2. **Ranges:** pressing Ctrl+Alt+B twice creates a range with in and out frames. The out frame can
   be edited in the window and set via `out=`.
3. **Shortcuts and API:** the shortcuts work from the main window and from the bookmark window.
   Every REST route works, and is documented in `docs/external_commands.md` and the JS client.
4. **Window and seeking:**
   - The window lists all bookmarks sorted by in frame.
   - During playback, clicking a row shows that frame; the frame counter equals `in_frame`.
   - Clicking the Out cell shows the out frame.
5. **Keyframe bookmarks:**
   - **Recording:** Ctrl+Shift+B, stop, reload, click. Playback lands 3 frames after the press,
     the smoke test reports zero folds and 3 emulated frames, and the jump is visibly faster than
     an ordinary seek on NDS.
   - **Playback:** the bookmark snaps to the keyframe at or before the current frame minus 3.
6. **Types:**
   - Types and colors created in the Types dialog persist across restarts.
   - Rows show the swatch and tint.
   - A replay opened on a machine without those types still shows the recorded colors.
7. **File behavior:**
   - Playback edits survive reloading the replay.
   - A v3/v4 replay prompts before upgrading; a v2 replay is refused.
   - After killing the app mid-recording, the temp and final files both load with the latest bookmarks.
8. **No regressions:**
   - All existing tests pass.
   - `supershuckie-replay-convert --verify` passes on the v3 fixtures.
   - `supershuckie-frame-server --probe` lists bookmarks from the table.

---

## 19. Implementation order

**Phase 0 — branch.** Create `replay-bookmarks` off `ram-tools`.

**Phase 1 — recorder format.**
- Bookmarks module and codec.
- Header field and v5 bump.
- `BookmarkTable` packet.
- Section read/write and the close ordering.
- Player resolution.
- Last-keyframe-at-frame resolution.
- `set_bookmark_table`, `insert_keyframe_with`, snapshot invariant.
- In-place upgrade.
- Tests 1-9. Measure the NDS keyframe-bookmark file cost.

**Phase 2 — resume and convert.**
- Seeding, truncation and anchor preservation.
- Verify changes.
- Tests 10-11.

**Phase 3 — core.**
- Anchor, millis estimate and bookmarks commands.
- `handle_replay` arm; resume plumbing.
- `bookmark_smoke.rs`.

**Phase 4 — frontend.**
- `ReplayBookmarks` and the writer thread.
- Settings types.
- Operations, lifecycle hooks, upgrade gate.
- Unit tests.

**Phase 5 — REST.** Routes, `/stats` field, docs, JS client.

**Phase 6 — C API and Qt.**
- Header and functions.
- Menu actions.
- `BookmarkWindow`, `BookmarkTypesDialog`, `AddBookmarkDialog`, upgrade prompt.

**Phase 7 — frame server.** Table-based bookmarks and probe fields.

**Phase 8 (optional) — bindable `Control` entries.**

Build the Windows static MinGW release after phases 1 and 6. `scripts/make-attributions.py`
must still pass. No new crates are expected.

---

## 20. Quick reference — key symbols

| Symbol | Location |
|---|---|
| `ReplayHeaderRaw::parse`, `ReplayFileMetadata::as_raw_header`, `REPLAY_VERSION`, `_padding_2` | `supershuckie-replay-recorder/src/replay_file/header.rs` |
| `Packet`, `PacketDiscriminator`, `BookmarkMetadata` | `supershuckie-replay-recorder/src/packet.rs`, `packet/io.rs` |
| `ReplayFileRecorder::{add_bookmark, insert_keyframe, close, sync_header, next_blob}` | `supershuckie-replay-recorder/src/replay_file/record.rs` |
| `NonBlockingReplayFileRecorder` | `supershuckie-replay-recorder/src/replay_file/record/thread.rs` |
| `ReplayFilePlayer::{new, all_bookmarks, all_keyframes, go_to_keyframe, fast_forward_chain}` | `supershuckie-replay-recorder/src/replay_file/playback.rs` |
| `build_resumed_recorder`, `build_reencoded_recorder`, `prime_and_refeed` | `supershuckie-replay-recorder/src/replay_file/record/resume.rs` |
| `convert_replay_file`, `verify_replay_files` | `supershuckie-replay-recorder/src/replay_file/convert.rs` |
| `util::blake3_hash` | `supershuckie-replay-recorder/src/util.rs` |
| `SuperShuckieCore::{go_to_replay_frame, POST_LOAD_FRAMES, push_keyframe_if_needed, handle_replay, resume_recording_replay}` | `supershuckie-core/src/lib.rs` |
| `ThreadCommand`, `ThreadedSuperShuckieCore::mark_start` | `supershuckie-core/src/thread.rs` |
| `SuperShuckieFrontend::{load_replay_if_exists, start_recording_replay, resume_recording_from_replay, stop_recording_replay, tick}` | `supershuckie-frontend/src/lib.rs` |
| `Settings`, `Control` | `supershuckie-frontend/src/settings.rs` |
| Watch-list JSON FFI pattern | `supershuckie-frontend-c/src/memory.rs`, `include/supershuckie/memory.h` |
| `SuperShuckieServerCommand`, routes | `supershuckie-frontend-webserver/src/lib.rs` |
| `set_up_replays_menu`, `refresh_action_states`, `tick` | `supershuckie-qt/src/main_window.cpp` |
| `RamWatchWindow` (analogue) | `supershuckie-qt/src/ram_watch_window.cpp` |
| `ReplaySummary::of`, `Info` | `supershuckie-frame-server/src/source.rs`, `protocol.rs`, `probe.rs` |

---

## 21. Implementation notes (2026-09-14)

What was built follows the spec; these are the places it differs or adds to it.

**Format and recorder**
- **Always a section.** `close()` writes a bookmark section even for an empty table, so every closed
  v5 file resolves from its section.
- **Snapshots.** `set_bookmark_table` ignores a table equal to the current one, so a recording that
  never has bookmarks writes no snapshots. Resume and convert seed the table with
  `seed_bookmark_table`, which always writes a snapshot: blobs copied verbatim may hold snapshots of
  bookmarks the new file must not recover.
- **Conversion.** `REPLAY_VERSION_CURRENT_ENCODING = 4` was added. The app's replay conversion
  (and `convert-replays.ps1`, which already checked `>= 4`) skips v4 and newer files. Otherwise every
  v4 replay would count as "not current" after the bump and be re-encoded for nothing.
- **`no_std`.** `record.rs` imported `std::collections::BTreeMap` in the `no_std` build; it now uses
  `alloc`.

**Core**
- **Anchors never emulate.** Plain bookmarks use the last completed frame. Keyframe bookmarks
  requested mid-frame write their keyframe when the frame completes (§6).
- **Diagnostics.** `SuperShuckieCore::replay_player()` was added, read-only, for the smoke test's
  fold count.

**Frontend**
- **Replay names.** `start_recording_replay(Some(name))` used to give the temp file the final file's
  path, so stopping deleted the recording. It now uses `temp-<name>`, like resume already did. Qt
  always passes no name, so the app never hit this.
- **Flush timing.** Bookmark changes of a played-back replay are flushed right before another replay
  is attached, not when the load starts, so a replay that fails to parse keeps its state.
- **C API.** `supershuckie_frontend_bookmark_active_type` writes into a caller buffer instead of
  returning a borrowed pointer.
- **Range toggle.** The toggle also exists as `supershuckie_frontend_bookmark_toggle_range_json`, so a
  range can be started as a keyframe bookmark.

**Not built**
- The optional bindable `Control` entries (§13.6).
- Everything in §16.

**How it was checked**
- `cargo test -p supershuckie-replay-recorder` (63), `-p supershuckie-frontend --lib` (14),
  `-p supershuckie-frontend-webserver` (2), `-p supershuckie-frame-server` (6).
- `supershuckie-core/examples/bookmark_smoke.rs`:
  - GBC (Pokémon Red) and GBA (Emerald hack).
  - A keyframe bookmark seek took 1.8 ms against 68-78 ms for an ordinary seek 115 frames past a
    keyframe, with no deltas applied.
  - On GBC the state was identical to sequential playback.
  - On GBA the state was identical to an ordinary seek. Both differ from sequential playback in the
    same 14 bytes of mGBA's state, which is how GBA seeks already behave.
- `supershuckie-frontend/examples/bookmark_frontend_smoke.rs`: recording, REST, playback edits,
  resume, and upgrading a v4 replay, on the same two ROMs.
- The macOS Qt app was run against a scratch home directory (`CFFIXED_USER_HOME`) and driven over
  REST. The bookmark window was screenshotted; row clicks were not driven, but they call the same
  seek function as `/go-to-bookmark`.

**Link flags for the examples on macOS**

```text
cargo rustc --release -p supershuckie-core --example bookmark_smoke -- \
    -L native=build/melonDS/src -L native=build/melonDS/src/teakra/src -L native=build/mgba \
    -l static=core -l static=teakra -l static=mgba -l c++ \
    -C link-arg=-framework -C link-arg=CoreFoundation -C link-arg=<path to libclang_rt.osx.a>
```

**Found along the way, not changed**
- **melonDS build script.** `scripts/build-melonds.sh` passes `-ffat-lto-objects`, which Apple clang
  rejects (`scripts/build-melonds.sh` is not executable either). melonDS was built by hand without
  the LTO flags.
- **AGL on macOS 26.** Qt 6's `FindWrapOpenGL` links the AGL framework, which the macOS 26 SDK no
  longer has. Configuring with `-DWrapOpenGL_AGL=/System/Library/Frameworks/OpenGL.framework` links.
- **Extra keyframe at frame 1.** `SuperShuckieCore::start_recording_replay` does not reset
  `frames_since_last_keyframe`, so a recording started after the game ran gets a keyframe at frame 1.
