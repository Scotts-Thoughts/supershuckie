# Play Together — Implementation Spec

**Target:** Claude Code, working in the supershuckie repository on branch `play-together` (off `replay-bookmarks`).
**Status:** approved and implemented 2026-09-17 (uncommitted). See §17 for where the implementation departs from this text.
**Components:** new crate `supershuckie-play-together`; `supershuckie-replay-recorder`, `supershuckie-core`, `supershuckie-frontend`, `supershuckie-frontend-c`, `supershuckie-qt`; docs.
**No new third-party crates** (std::net + the already-linked blake3 / zstd-sys / serde).

## 1. Context

The user wants to play alongside friends and run side-by-side race events inside one Super Shuckie instance: load your ROM plus each friend's ROM and see every participant's game live. Instead of video, each participant publishes what already reproduces a session bit-exactly — the replay packet stream (per-frame inputs, external memory writes, resets, save-state loads, frame markers) — and everyone else runs a **follower** emulator instance of that ROM driven by the stream. That is "live replay playback", which the codebase already does from files, so the feature is: a publisher tee out of the core, a live packet source into the core, a follower pacing mode, and a small TCP session around it.

Done means: three players on GB/GBC/GBA, the local player at a rock-steady 4x (≈240 fps), followers allowed to fluctuate but never drifting further behind. NDS is refused with a clear message for now; nothing below precludes it.

## 2. Decisions already made (2026-09-17)

| Topic | Decision |
|---|---|
| Connectivity | One player **hosts** a TCP port; others connect. The host **relays** every stream to every participant (star). Code = `host:port`. `std::net` threads, no async runtime, no NAT traversal (port forward or a VPN). Transport behind a trait so a relay server can come later. |
| Layout | **One top-level window per friend** (like the RAM tool windows) with a status strip. Main window unchanged. |
| Session start | **Snapshot on subscribe**: a follower asks its publisher for a full save state and starts there. Host has **Reset everyone** (countdown, then every participant hard-resets its own console; the reset lands in each stream as `ResetConsole`). |
| Saving | Followers also write the received stream to a replay file (setting "Save friends' games as replays", default **on**): `<friend> - <UTC date>.replay` in the local ROM's replay folder. |
| Platforms | GB, GBC, SGB2, GBA. NDS publishers/joiners refused for now (hidden `allow_nintendo_ds` setting keeps the path testable). |

Judgement calls made in this plan (flip if you disagree): publishing while a **replay is attached** is refused ("Close the replay first"), and loading a replay while in a session is refused; recording your own replay during a session is fine. Reset-everyone also **unpauses**. Followers keep running while the local game is paused. Friend audio is off by default with a per-window toggle.

## 3. How things work today (what the design builds on)

- **Packets** (`supershuckie-replay-recorder/src/packet.rs`, `packet/io.rs`): `Packet::{NextFrame{timestamp_delta}, ChangeInput{data}, WriteMemory{address,data}, ChangeSpeed, ResetConsole, LoadSaveState{state}, Keyframe{metadata,state}, IncrementCounter, ...}` with a compact, hostile-safe codec (`PacketIO::write_packet_instructions` / `Packet::read_all(&mut &[u8], version)`). `KeyframeMetadata { input, speed, elapsed_frames, elapsed_millis, counters }` is already a complete "snapshot" description.
- **Recording** (`supershuckie-core/src/lib.rs`): `before_run` → `handle_replay` / `update_input` / `flush_writes`; `after_run` → `do_frame_timekeeping` (records `NextFrame`) / `push_keyframe_if_needed`. `with_recorder` is the single fan-out to `replay_file_recorder: Option<Box<dyn ReplayFileRecorderFns>>`. `update_input` latches once per emulated frame (`input_latched`; the 2026-09-17 per-poll regression is the reason — anything added must be per-frame, never per-poll).
- **Playback**: `handle_replay` pulls `ReplayFilePlayer::next_packet()` until `NextFrame`; `Ok(None)`/`Err` sets `replay_stalled` and `do_run_fn` skips the run (holds position). `is_playing_back()` = attached and driving; every "replay owns the console" gate keys off it. `attach_replay_player` checks console type, ROM/BIOS blake3 and core name before touching anything.
- **Thread** (`supershuckie-core/src/thread.rs`): one core per `ThreadedSuperShuckieCore`; `ThreadCommand` + `call()` (every reply sender must answer on every branch); `run_one` uses the core's own wall-clock pacing (GBA deadline via injected clock; SameBoy sleeps inside `GB_run` unless turbo). Windows: ABOVE_NORMAL + EcoQoS opt-out at spawn.
- **Multiple cores in one process are proven** (switch_core overlap, GB shadow audio instance, `nds_bench --repro`); no statics in mGBA/melonDS/SameBoy shims.
- **Determinism across machines**: same ROM blake3, BIOS blake3, core name, console type. mGBA RTC pinned (`RTC_FAKE_EPOCH`), states carry SRAM+RTC. GB `RtcMode::Accurate` (cycle based). The emulated SameBoy never gets a sample rate.
- **Frontend/Qt**: one `SuperShuckieFrontend` with one `core`; C API is hand-written headers with JSON for lists; Qt has a fixed-size `MainWindow`, a 1 ms tick, `MemoryToolsController` as the "N tool windows" precedent, `qt__*` custom settings, modal dialogs bracket `stop_timer()/start_timer()`. Existing network code: REST `127.0.0.1:30158` (rouille, command queue drained in `tick`), Poke-A-Byte UDP, frame server (stdin/stdout, length-prefixed framing in `supershuckie-frame-server/src/protocol.rs` — the framing pattern to copy).

## 4. Architecture

```
 local core thread (Primary)                       network threads                     follower core threads (Follower, BELOW_NORMAL)
 SuperShuckieCore                                  supershuckie-play-together
   ├─ replay_file_recorder (unchanged)             HostSession / ClientSession           SuperShuckieCore (per friend)
   └─ stream_publisher: Box<dyn StreamPublisherFns>   reader/writer thread per conn        ├─ live_source: LiveReplaySource ◄── LiveReplayFeeder ◄── FollowerSink (reader thread)
        │ set_input/write_memory/next_frame/…         host relays Arc<framed bytes>        ├─ replay_file_recorder (friend's .replay, fed by the core itself)
        ▼                                             coalesces snapshot requests          └─ run_one_follower(): paces on stream arrival, capped catch-up
   frontend SessionPublisher (batches one frame → PublisherHandle::publish)
                                                                                         frontend tick: session events (joined/left/reset/errors), peer screens, status JSON → C API → Qt PeerWindow
```

- **Wire payload = replay packets.** `Stream` messages carry concatenated `Packet` bytes; a **snapshot** is a structured `Snapshot` message (state zstd-compressed off the emulator thread) that the follower side turns into a `Packet::Keyframe` with the state present; `SyncHash` (blake3 of work RAM every 60 frames) rides beside them on the same ordered connection.
- **Stream bytes bypass the UI thread.** The reader thread pushes straight into that follower's `LiveReplayFeeder`, so a modal dialog or a busy UI never stalls followers. Only roster/reset/error events go through the frontend tick.
- **Snapshots are pulled by whoever subscribes** (join, late ROM locate, gap, hash mismatch, too far behind, queue overflow); requests are coalesced per publisher (1 per 2 s) and a snapshot with several requesters is broadcast.
- **Friend replay files are written by the follower core** (it already integrates time, inserts periodic keyframes, and can write an exact full keyframe at every snapshot), started lazily at the first applied snapshot.

## 5. Recorder crate — `supershuckie-replay-recorder` (small, additive)

- `src/packet/io.rs`: `PacketWriteCommand::bytes()` → `pub` (so other crates can serialize a `Packet` to bytes without a sink). Add `pub fn append_packet(packet: &Packet, into: &mut Vec<u8>)` next to it.
- `src/util.rs`: `compress_data` / `decompress_data` → `pub` (+ docs; `decompress_data(data, expected_len)` already refuses absurd claimed sizes before allocating — exactly what a hostile snapshot needs). Add `pub fn blake3_hash_slices<'a>(parts: impl IntoIterator<Item=&'a [u8]>) -> ReplayHeaderBlake3Hash` so the core does not take a direct blake3 dependency.
- No new packet discriminator, no format bump: the on-disk format is untouched; friend replays are ordinary v6 files.

## 6. Core — `supershuckie-core`

### 6.1 Publisher tee (`src/stream.rs`, new; trait consumed by the core, implemented by the frontend)

```rust
pub trait StreamPublisherFns: Send + 'static {
    fn snapshot(&mut self, metadata: KeyframeMetadata, state: Vec<u8>);   // join/resync; exact create_save_state output
    fn next_frame(&mut self, timestamp_millis: TimestampMillis);          // absolute; impl computes deltas
    fn set_input(&mut self, input: InputBuffer);
    fn write_memory(&mut self, address: UnsignedInteger, data: ByteVec);
    fn reset_console(&mut self);
    fn load_save_state(&mut self, state: ByteVec);
    fn change_counter(&mut self, name: String, delta: SignedInteger);
    fn sync_hash(&mut self, frame: UnsignedInteger, hash: [u8; 32]);
    fn end(&mut self);
    fn poll_errors(&mut self) -> Vec<String>;
    fn take_free_state_buffer(&mut self) -> Option<Vec<u8>> { None }     // pooling like the recorder
}
pub enum SnapshotRequestReason { Join, HashMismatch, TooFarBehind, QueueOverflow, Gap }
pub const SYNC_HASH_INTERVAL_FRAMES: u64 = 60;
```

`SuperShuckieCore` (`src/lib.rs`) gains `stream_publisher: Option<Box<dyn StreamPublisherFns>>`, `stream_snapshot_pending: bool`, `stream_frames_since_hash: u64` and:
- `start_stream_publishing(publisher) -> Result<(), String>`: refuses when no console or `has_replay_attached()`; `finish_current_frame()`; publish a snapshot immediately; `replay_counters.get_or_insert_with(..)`; `input_latched = false`. **Never `restart_timer`** (a file recording may be running; its `next_frame` errors on backwards time) — the stream carries absolute millis in snapshots and the impl derives deltas.
- `stop_stream_publishing()`, `request_stream_snapshot()` (sets the coalescing flag), `publish_pending_stream_snapshot()` (for the paused case), `poll_stream_errors()`.
- `fn is_capturing() = replay_file_recorder.is_some() || stream_publisher.is_some()` replaces the `replay_file_recorder.is_some()` gates in `update_input` (ByteVec build; clone once when both are active), `change_replay_counter`, the `replay_counters` invariant, and the `load_save_state` "draw a preview only when nothing captures" branch.
- Tee call sites, one line beside each existing `with_recorder`: `flush_writes` (successful writes only), `hard_reset`, `load_save_state`, `change_replay_counter`, `do_frame_timekeeping` (`next_frame` per frame), `update_input` (`set_input`, riding the existing `input_latched` latch so it is per-frame by construction). **Not forwarded:** `set_speed`, periodic keyframes, bookmarks, mark start/end.
- `after_run` → `service_stream(time)`: gated on `time.frames > 0 && !is_mid_frame()`; every 60 frames `sync_hash()`; then, if pending, `publish_snapshot_now()`. Order on the wire is therefore `NextFrame(N)`, `SyncHash(N)`, `Snapshot(N)`, then frame N+1's packets — in order by construction, no cross-thread merge.
- `sync_hash() -> Option<[u8;32]>`: blake3 over the writable work-RAM regions by short name (GB: WRAM+WRAMX+HRAM ≈ 32 KB, ~20 µs; GBA: EWRAM+IWRAM = 288 KB, ~0.2 ms; NDS later: MAIN+SWRAM+WRAM7). Not VRAM/OAM/palette/SAVE (GBA save length varies with detection). 4 hashes/s at 4x on GBA ≈ 0.02 % of the budget.
- `publish_snapshot_now()`: `KeyframeMetadata { input: current encoded input, speed, elapsed_frames: total_frames, elapsed_millis: total_milliseconds, counters }` + `create_save_state_into` a pooled buffer (extend `take_state_buffer` to also pull from `publisher.take_free_state_buffer()`).

### 6.2 Live source for followers (`src/live_replay.rs`, new, `cfg(feature="std")`)

```rust
pub enum LiveItem { Packet(Packet), SyncHash { frame: u64, hash: [u8; 32] } }   // snapshots arrive as Packet::Keyframe with state
pub struct LiveReplayFeeder(Arc<LiveShared>);   // owned by the network reader thread
pub struct LiveReplaySource(Arc<LiveShared>);   // owned by the follower core
pub fn live_replay_channel(stats: Arc<FollowerStats>, upstream: mpsc::Sender<SnapshotRequestReason>) -> (LiveReplayFeeder, LiveReplaySource);
impl LiveReplayFeeder { pub fn push_packet(&self, p: Packet); pub fn push_sync_hash(&self, frame: u64, hash: [u8;32]); pub fn end(&self); }
impl LiveReplaySource { pub(crate) fn pop(&self) -> LivePoll /* Item | Waiting | Ended */; pub fn frames_available(&self) -> u64; pub fn newest_publisher_frame(&self) -> u64; pub fn request_snapshot(&self, reason); }
pub struct FollowerStats { frames_behind, newest_publisher_frame, waiting: AtomicBool, snapshots_applied, hash_mismatches, snapshot_requests, drawn_frames, emulated_frames }  // atomics + a plain `FollowerStatsSnapshot` copy
```
- `push_packet(Keyframe)` **clears the queue** (a snapshot supersedes any backlog) and sets `have_snapshot`; before the first snapshot everything is dropped. `NextFrame` increments `frames_queued` / `newest_frame` (learned from the last keyframe's `elapsed_frames`). Pushes unpark the core thread (`set_waker`).
- Bound `MAX_QUEUED_FRAMES = 1200`: on overflow drop the queue and request `QueueOverflow`. Upstream requests are rate-limited in the source (one outstanding until a keyframe arrives or 2 s pass) and sent through the `mpsc::Sender` the frontend drains in `tick` → `session.request_snapshot(peer)`.

`SuperShuckieCore` changes (`src/lib.rs`): add `live_source: Option<LiveReplaySource>`, `replay_waiting: bool`, `stream_time_origin: TimestampMillis`, `hash_suppressed_until: u64`, `pending_follower_recording: Option<..>`. **Keep `replay_player` as is** (a second `Option`, not an enum: every seek/bookmark/resume site is file-only and already tolerates `None`).
- `is_playing_back()` → `(replay_player.is_some() || live_source.is_some()) && !replay_playback_stopped`; `has_replay_attached()` stays file-only; add `is_following()`.
- `do_run_fn`: skip `before_run` and the run while `replay_waiting` too (waiting ≠ stalled: never flips `playback_paused`).
- Extract the packet `match` of `handle_replay` into `apply_playback_packet(&Packet, keyframe_state: Option<&[u8]>)`; add `handle_live_replay()` (pop until `NextFrame`; `Waiting` → `replay_waiting = true`; `Ended` → `replay_stalled`; `SyncHash` → `check_sync_hash`; `Keyframe` → `apply_stream_snapshot`).
- `apply_stream_snapshot(metadata, state)`: `load_save_state`, `set_input_encoded(metadata.input)`, `total_frames = elapsed_frames`, `total_milliseconds = elapsed_millis`, counters, `frames_since_last_keyframe = 0`, `bump_state_epoch`, `hash_suppressed_until = total_frames + POST_LOAD_FRAMES`, `snapshots_applied += 1`; start the pending follower recording here if requested (initial keyframe = this state) or, if already recording, `load_save_state` + `insert_keyframe_full`. No transient-buffer splice needed (exact state, never a delta).
- `check_sync_hash(frame, hash)`: ignore when `frame != total_frames` or `< hash_suppressed_until`; on mismatch count + `request_snapshot(HashMismatch)` and keep running.
- `attach_live_replay_source(source, metadata: &ReplayFileMetadata, allow_mismatched)`: extract the compat block of `attach_replay_player` into `check_replay_compat(..)` and share it; `stop_recording_replay(); detach_replay_player(); detach_live_source();` reset input/writes/counters; `replay_waiting = true`; no `restart_timer`. `detach_live_source()` mirrors `detach_replay_player`.
- `run_unlocked_presenting(draw: bool)`: `set_skip_drawing(!draw)` + `do_run_fn(run_unlocked, audible = true)` (free while `audio_enabled == false`; makes friend audio a one-liner).
- `start_recording_follower_replay(partial: PartialReplayRecordMetadata<FS,TS>, metadata: &ReplayFileMetadata)`: requires `is_following()`; deferred until the next applied snapshot; `stream_time_origin = total_milliseconds` at that point; the follower path of `apply_playback_packet` mirrors `ChangeInput/WriteMemory(if applied)/ResetConsole/LoadSaveState/IncrementCounter/NextFrame(total_milliseconds - origin)` into the recorder (gated on `is_following()` — a file-playback core never has a recorder). `write_keyframe` subtracts `stream_time_origin` (0 for non-followers), so periodic keyframes keep working for free.

### 6.3 Thread (`src/thread.rs`)

- `pub enum CoreThreadRole { Primary, Follower }`; `new_with_role(core, role)`; `mark_thread_role`: Primary = existing ABOVE_NORMAL + EcoQoS opt-out; Follower = `THREAD_PRIORITY_BELOW_NORMAL`, no EcoQoS opt-out (E-cores are fine). Thread name `ThreadedSuperShuckieCore/follower`.
- New `ThreadCommand`s (all with reply must answer on every branch): `AttachLiveSource{source, metadata, allow_mismatched, reply}`, `DetachLiveSource`, `StartStreamPublishing{publisher, reply}`, `StopStreamPublishing`, `RequestStreamSnapshot`, `StartRecordingFollowerReplay{..., reply}`. Handle methods: `attach_live_replay_source`, `detach_live_replay_source`, `start_stream_publishing`, `stop_stream_publishing`, `request_stream_snapshot` (+ `wake()`), `start_recording_follower_replay`, `follower_stats() -> Option<FollowerStatsSnapshot>`, `get_stream_errors()`.
- `run_thread`: `if follower.is_some() { run_one_follower() } else { run_one() }`; on the idle path, `publish_pending_stream_snapshot()` when a snapshot is pending and the core is paused (not mid-frame). Drain stream errors beside `handle_replay_recording_errors`.
- `run_one_follower()` — the guardrail loop. Constants (documented tunables): `FOLLOWER_TARGET_LAG_FRAMES = 2`, `FOLLOWER_MAX_FRAMES_PER_PASS = 8`, `FOLLOWER_MAX_PASS = 4 ms`, `FOLLOWER_RESYNC_BEHIND_FRAMES = 300`, `FOLLOWER_DRAW_INTERVAL = 16 ms`, `FOLLOWER_WAIT = 4 ms`.
  ```
  behind = newest_publisher_frame - total_frames; stats.frames_behind = behind
  if behind > RESYNC_BEHIND → source.request_snapshot(TooFarBehind)      (rate-limited inside)
  want = min(behind - TARGET_LAG, MAX_FRAMES_PER_PASS)
  if want == 0 || frames_available == 0 → waiting = true; park_timeout(FOLLOWER_WAIT); return   (unparked by pushes)
  loop up to `want` frames and MAX_PASS: run_unlocked_presenting(draw = last of pass && last_draw ≥ DRAW_INTERVAL); stop on waiting/stalled; record frame_times; emulated_frames += frames
  ```
  Never calls `run()` (no wall-clock deadline, no SameBoy sleep, no `present_every`). GB slices only wait at frame boundaries (`handle_live_replay` returns early on `is_mid_frame()`). Stream `End` → stalled → paused, which the frontend shows as "friend left", not "replay finished".

## 7. New crate — `supershuckie-play-together` (protocol + session; no emulator dependency)

Layout: `src/{lib.rs, code.rs, error.rs, compat.rs, transport.rs, conn.rs}`, `src/protocol/{mod.rs, wire.rs, stream.rs, snapshot.rs}`, `src/session/{mod.rs, host.rs, client.rs, follow.rs}`, `tests/{framing,loopback,backpressure,hostile}.rs`, `docs/play-together-protocol.md`. Deps: `supershuckie-replay-recorder`, `blake3`, optional `serde`.

**Framing** (copy `supershuckie-frame-server/src/protocol.rs`): `u32 len LE | u8 tag | payload`; strings/bytes length-prefixed; `PROTOCOL_VERSION = 1`; `MAX_MESSAGE_LENGTH = 48 MiB` for `Stream`/`Snapshot`, `64 KiB` for everything else; bodies read in 64 KiB chunks (never `vec![0; claimed]`); unknown tags skipped (forward compatible); a `HelloOnly` tag policy before admission. `DecodeError` mirrors the frame server's plus `TooLongForTag`, `BadEnum`, `TooMany`, `ZeroPeerId`, `BadSpeed`, `ForbiddenPacket`, `StateTooLarge`, `ProtocolViolation`.

**Messages** (`PeerId = u16`, host = 1, clients 2.. never reused; 0 = none/broadcast):

| Tag | Message | Direction |
|---|---|---|
| 0x01 | `Hello { protocol_version, replay_version, app_version, display_name, publisher: PublisherInfo }` | client → host |
| 0x02 | `Welcome { your_peer_id, session_id, your_display_name, participants }` | host → client |
| 0x03 | `Refused { reason, text }` | host → client |
| 0x04/0x05 | `PeerJoined { participant }` / `PeerLeft { peer_id, reason }` | host → clients |
| 0x06 | `ResetAll { race_id, countdown_millis }` | host → clients |
| 0x10 | `Stream { from, first_frame, bytes }` — whole `Packet`s, allow-list: NoOp/NextFrame/ChangeInput/WriteMemory/ChangeSpeed/ResetConsole/LoadSaveState/IncrementCounter (file-structure packets → `ForbiddenPacket`) | publisher → host → others |
| 0x11 | `Snapshot { from, target, frame, elapsed_millis, input, speed, counters, encoding(raw/zstd), state_len, state }` | publisher → host → target(s) |
| 0x12 | `SyncHash { from, frame, hash }` | publisher → host → others |
| 0x13 | `RequestSnapshot { requester, target }` (host overwrites `requester`) | follower → host → publisher |
| 0x20/0x21/0x22/0x2F | `Ping`/`Pong`/`Goodbye`/`Error` | both |

`PublisherInfo { metadata: ReplayFileMetadata (crops omitted, patch must be Unpatched), initial_input, speed, frame }`; `ParticipantInfo { peer_id, display_name, app_version, publisher }`. `ReplayConsoleType`/`ReplayPatchFormat` travel as `u32` via `try_from`. Snapshots are zstd level 3 (GBA ~400 KB → ~120 KB) compressed on the **writer thread**; `Stream` is never compressed. A `Stream` message coalesces whatever frames are pending when the writer wakes (one message per frame when idle, `TCP_NODELAY`), split at 256 KiB boundaries **between** packets (a single large `LoadSaveState` packet may exceed it).

**Session API** (`session/mod.rs`; `&self` methods, `Send + Sync`, so a `PublisherHandle` can be used from the core thread):
```rust
pub trait Session: Send + Sync {
    fn role(&self) -> Role; fn session_id(&self) -> SessionId; fn local_peer_id(&self) -> PeerId; fn participants(&self) -> Vec<ParticipantInfo>; fn is_connected(&self) -> bool;
    fn poll_events(&self) -> Vec<SessionEvent>;                       // Connected/Joined/Left/SnapshotRequested{requesters}/ResetAll{race_id,deadline}/StreamOverrun/RttUpdated/Warning/Disconnected
    fn publisher(&self) -> PublisherHandle;                            // Clone + Send: publish(first_frame, bytes) / publish_snapshot(SnapshotData, target) / publish_sync_hash(frame, hash) — non-blocking, queue-backed
    fn subscribe(&self, publisher: PeerId, sink: Box<dyn FollowerSink>) -> Result<(), PlayTogetherError>;   // reader thread delivers Stream/Snapshot/SyncHash straight to the sink; requests a snapshot
    fn unsubscribe(&self, publisher: PeerId);
    fn request_snapshot(&self, publisher: PeerId);                     // deduped per publisher within 2 s
    fn send_reset_all(&self, countdown: Duration) -> Result<u32, PlayTogetherError>;  // host only
    fn kick(&self, peer: PeerId) -> Result<(), PlayTogetherError>;     // host only
    fn stats(&self) -> SessionStats; fn leave(&self);                  // Goodbye, join threads ≤ 2 s; also on Drop
}
pub trait FollowerSink: Send { fn packets(&mut self, first_frame: u64, packets: Vec<Packet>); fn snapshot(&mut self, s: SnapshotData); fn sync_hash(&mut self, frame: u64, hash: [u8;32]); fn ended(&mut self, reason: LeaveReason); }
impl HostSession { pub fn bind(config: HostConfig { bind_address, port, max_participants: 8, allow_nintendo_ds }, local: LocalParticipant) -> Result<Self, PlayTogetherError>; pub fn local_addr(&self) -> SocketAddr; }
impl ClientSession { pub fn connect(code: JoinCode, local: LocalParticipant) -> Self; }   // non-blocking; outcome via Connected / Disconnected events
```
- **Per-publisher follow state** on every receiver: `AwaitingSnapshot` (Stream/SyncHash discarded — also while no sink is subscribed) → `Live { expected_frame }` (`first_frame` must equal expected; a gap sends `RequestSnapshot` and returns to `AwaitingSnapshot`) → `Overrun` (inbound high-water 64 MiB while unsubscribed queues grow; with a sink the feeder's own 1200-frame bound applies instead).
- **Outbound queues** per connection: bounded (64 MiB / 16 384 items), `try_push` never blocks; overflow → that peer is dropped `TooSlow`; the host relays `Arc<framed bytes>` so one slow peer never stalls the others. Writer thread: condvar, coalesce, compress snapshots, `Ping` after 1 s idle. Reader: 10 s idle → `Timeout`; any decode error → `Error` + close.
- **Admission** (`compat.rs`): protocol version, replay version, session full, NDS unless allowed, `Unknown` console, patched ROM, second Hello → `ProtocolViolation`. Display names deduped (`"Alice (2)"`, clamped 32 bytes, `""` → `"Player"`). `follow_compatibility(local, publisher)` (console/ROM/core/BIOS) is decided locally per publisher: you can publish while being unfollowable by one peer.
- **Snapshot coalescer** (publisher side): one `SnapshotRequested { requesters }` per 2 s; the frontend answers with one `request_stream_snapshot()`; the resulting snapshot goes to the single requester or is broadcast when several asked.
- **Join code** (`code.rs`): `JoinCode::parse/format` (`"1.2.3.4:30159"`, `[v6]:port`, hostnames, bare host → `DEFAULT_PORT = 30159`), `probe_local_ip()` (UDP-connect trick). Default bind `0.0.0.0` (the one deliberate departure from the `127.0.0.1` defaults; the UI says so). Timeouts: connect 5 s, handshake 5 s, keepalive 1 s, drop 10 s.
- **Transport trait** (`transport.rs`): `Transport { listen, connect }`, `Listener { try_accept (polled every 20 ms) }`, `Connection { split, set_timeouts, shutdown }`; `TcpTransport` only in v1; `bind_with::<T>` / `connect_with::<T>` are `pub` for tests and a future relay.
- Out of scope, documented in the crate docs: encryption/auth (share codes only with friends; prefer LAN/VPN), optional session password as a follow-up (`Challenge` pre-Hello message, new protocol version).

## 8. Frontend — `supershuckie-frontend`

### 8.1 Settings (`src/settings.rs`)
```rust
pub struct PlayTogetherSettings {            // Settings.play_together, #[serde(default)]
    display_name: String ("Player"), host_port: u16 (30159), bind_address: String ("0.0.0.0"), last_join_code: String,
    save_peer_replays: bool (true), peer_video_scale: NonZeroU8 (2), allow_nintendo_ds: bool (false, hidden),
    known_roms: BTreeMap<String /*blake3 hex*/, UTF8CString /*path*/> (≤ 64 entries, learned from every load_rom and locate)
}
```
`clamp()`: name trimmed/≤32 chars, scale ≤ 12, bind address must parse as an `IpAddr` else reset. Every successful `load_rom` records `(hex(core.rom_checksum()), path)`.

### 8.2 `src/play_together.rs` (new; `impl SuperShuckieFrontend` lives here)
- `PlayTogetherSession { session: Arc<dyn Session>, role, code, local_peer_id, peers: Vec<PeerInstance>, publishing, pending_reset: Option<Instant>, rom_candidates, errors, generation }`.
- `PeerInstance { peer_id, name (sanitised), info: ParticipantInfo, emulator_type, status: NeedsRom|Starting|Following|Waiting|Resyncing|Desynced|Error, status_text, core: Option<ThreadedSuperShuckieCore>, stats: Arc<FollowerStats>, snapshot_requests: mpsc::Receiver<SnapshotRequestReason>, audio: Option<Arc<AudioOutput>>, replay: Option<PeerReplayFile>, local_rom_path, last_stats: ElapsedTimeStats, fps, video_scale, window_hidden }`.
- **`SessionPublisher`** (implements `StreamPublisherFns` around `PublisherHandle` + `Arc<Mutex<Vec<PeerId>>>` pending requesters): buffers a frame's packets via `append_packet`; `next_frame` appends `NextFrame{delta}` and calls `publish(first_frame, bytes)`; `snapshot` → `publish_snapshot(SnapshotData{..}, target = single requester or 0)`; `sync_hash` → `publish_sync_hash`. Tracks `first_frame` (= frame count before the first `NextFrame` in the message; after a snapshot at N the next Stream starts at N).
- **`FeederSink`** (implements `FollowerSink` around `LiveReplayFeeder`): `packets` → `push_packet` each; `snapshot` → `push_packet(Packet::Keyframe { metadata: KeyframeMetadata{input, speed, elapsed_frames: frame, elapsed_millis, counters}, state })`; `sync_hash` → `push_sync_hash`; `ended` → `end()`.
- Public API: `play_together_host(port, name)`, `play_together_join(code, name)`, `play_together_leave()`, `play_together_reset_all(countdown_s)` (host), `play_together_locate_rom(peer, path)` (blake3-verified), `play_together_add_rom_candidates(paths)`, `play_together_set_video_scale(peer|all, scale)`, `play_together_set_peer_audio_enabled(peer, bool)` / `play_together_peer_audio_output(peer)`, `play_together_state() -> PlayTogetherStateView` (serde → JSON), `play_together_generation()`, `is_play_together_active()`, getters/setters for the settings. Preconditions: game running, GB/GBC/SGB2/GBA (NDS → clear message), no replay attached, no session active, `refuse_if_exporting`.
- `find_rom_by_hash(hash)`: loaded ROM → `known_roms` (re-verify, prune stale) → `settings.recent_roms` → `rom_candidates` (favorites from Qt); skip files > 64 MiB; cache hits.
- `build_peer_core(peer, rom, path)`: `make_new_core_with_bios(rom, None /*no save: the snapshot carries SRAM*/, emulator_type from the publisher's declared console type, bios for that type (GBA: `compute_builtin_bios_override(bios_checksum)`; warn in `status_text` if the local BIOS hash differs), nds_jit=false)`; `ThreadedSuperShuckieCore::new_with_role(core, Follower)`; `set_audio_enabled(false)`; `set_speed(base)`; `live_replay_channel(stats, tx)`; `attach_live_replay_source(source, &info.publisher.metadata, false)`; `session.subscribe(peer, Box::new(FeederSink(feeder)))`; if `save_peer_replays` → open `PeerReplayFile` and `start_recording_follower_replay(..)` (the core starts it at the first snapshot); `callbacks.peer_change_video_mode(peer, screens, scale)`; `core.start()`.
- `tick_play_together(&mut errors)` from `tick()` after `refresh_screen(false)`: drain `poll_events` (`Connected` → start publishing via `core.start_stream_publishing(Box::new(SessionPublisher))` + add roster peers; `Joined` → `add_peer` (sanitise, map console, refuse NDS/Unknown, ROM lookup → build or `NeedsRom`); `Left` → shutdown peer (close replay file, drop core), bump generation; `SnapshotRequested{requesters}` → store targets, `core.request_stream_snapshot()`; `ResetAll` → `pending_reset`; `Disconnected` → full teardown + deferred error); fire the pending reset (`hard_reset_console(); set_paused(false)`); per peer: `is_alive()` check, `get_elapsed_time()` → `peer_refresh_screens` on generation change, `follower_stats()` → status enum (bump generation only on enum change), drain `snapshot_requests` → `session.request_snapshot(peer)`, recorder errors.
- `PeerReplayFile`: dir = `get_replays_dir_for_rom(local rom filename)`; name `"<sanitised friend> - <UTC yyyy-mm-dd hh.mm.ss>.replay"` (`sanitize_file_name_component` per `check_user_file_name`; `create_new` + ` (2)` suffix on collision; `temp-<name>` sibling like `start_recording_replay`); metadata from the publisher's `ReplayFileMetadata` (crops none, Unpatched); closed on leave/disconnect/exit; zero-frame files deleted; failed close keeps the temp file and reports like `stop_recording_replay`.

### 8.3 `src/lib.rs` hooks
- Field `play_together: Option<PlayTogetherSession>`; `unload_rom()` → `play_together_leave()`; `after_switch_core()` re-attaches the publisher (re-emits a snapshot) when a session is active and the console type is unchanged, else leaves; `set_speed_settings` propagates the base speed to peer cores.
- Refusals while in a session: `load_replay_if_exists`/`continue_last_replay` ("Close the Play Together session first"), GB mode/SGB toggles that would change the emulator type. Refuse hosting/joining while a replay is attached.
- Unchanged and local-only: RAM tools, Poke-A-Byte, REST, bookmarks, save states, SRAM, export, screenshots.
- `make_new_core` → thin wrapper over new `make_new_core_with_bios(rom, sram, emulator_type, bios, nds_jit)`. Add `SuperShuckieEmulatorType::from_replay_console_type`.
- `SuperShuckieFrontendCallbacks` gains defaulted `peer_refresh_screens(peer, screens)` (called with that peer's screens mutex held; no call-backs) and `peer_change_video_mode(peer, screens, scaling)` (no lock; read-only getters allowed). Peer removal is observed through the generation counter, not a callback.

## 9. C API — `supershuckie-frontend-c`

- `include/supershuckie/frontend.h` + `src/frontend.rs`: two new nullable callback fields `peer_refresh_screens(user_data, uint16_t peer, size_t count, const uint32_t *const *pixels)` and `peer_change_video_mode(user_data, uint16_t peer, size_t count, const SuperShuckieScreenData *, uint8_t scaling)` (Qt zero-initialises the struct, so the ABI growth is safe).
- New `src/play_together.rs` + `include/supershuckie/play_together.h` (included from `supershuckie.h`), JSON + `supershuckie_string_free` pattern:
  `..._play_together_host(f, port, name, err, len)`, `_join(f, code, name, err, len)`, `_leave`, `_is_active`, `_generation`, `_state_json` (schema below), `_reset_all(f, countdown_s, err, len)`, `_reset_countdown_ms`, `_locate_rom(f, peer, path, err, len)`, `_add_rom_candidates_json(f, paths_json)`, `_set_video_scale(f, peer /*0 = all + default*/, scale)`, `_get_video_scale`, `_set_peer_audio_enabled(f, peer, bool, err, len)`, `_retain_peer_audio_output(f, peer)` (release with `supershuckie_audio_output_release`), `_get/set_save_peer_replays`, `_get_display_name(f, out, len)`, `_get_last_host_port`, `_get_last_join_code(f, out, len)`, `_local_addresses_json()`.
- State JSON: `{"active","role":"host|client|connecting","code","local_name","local_peer_id","reset_countdown_ms","save_peer_replays","participants":[{"peer_id","name","rom_name","console","status":"needs_rom|starting|following|waiting|resyncing|desynced|error","status_text","frames_behind","waiting","snapshots_applied","hash_mismatches","fps","elapsed_frames","elapsed_ms","counters":{},"replay_file","video_scale","audio"}],"errors":[]}`. `generation` changes only on roster/status-enum/error/countdown changes; numeric fields are polled at ~4 Hz.

## 10. Qt — `supershuckie-qt/src` (add every new `.cpp` to `CMakeLists.txt`)

- **`screen_canvas.hpp/.cpp`** (new): `class ScreenCanvas : public QGraphicsView` — the scene/pixmap/scale compositing extracted from `GameRenderWidget` (`set_layout(count, screen_data, scale, horizontal, swap)`, `refresh_screen(count, pixels)`, `capture()`). `GameRenderWidget : ScreenCanvas` keeps all key/mouse/drag handlers (every `main_window->frontend` use lives there) and `set_dimensions()` becomes a call to `set_layout`. No duplicated compositing code.
- **`peer_window.hpp/.cpp`** (new): top-level `QWidget` (`Qt::Window | Qt::MSWindowsFixedSizeDialogHint`, `SetFixedSize`): `ScreenCanvas` (`Qt::NoFocus`, no key handlers) + status strip (`name — rom`, sync text "3 frames behind / Waiting for Ash… / Resyncing… / Desynced ×2 / ROM needed — click to locate", timer/counters, fps). Title `"Ash — crystal.gbc — Play Together"`. Close = hide (controller destroys on leave). Context menu: scale 1x–6x, Friend audio (opens its own `AudioOutput` from the retained peer ring), Locate ROM…. Geometry in `qt__play_together_window_<name>`.
- **`play_together_controller.hpp/.cpp`** (new, `MemoryToolsController` pattern, `friend` of `MainWindow`): owns the dialog and `std::map<uint16_t, PeerWindow*>`; `tick()` from `MainWindow::tick` (generation → re-read state JSON, create/destroy windows, prompt Locate ROM once per peer, status label "Play Together: 2 friends", countdown text, `refresh_action_states`; numeric strip every ~25 ticks); the two static callback thunks; `confirm_leave()` used by `load_rom`/`do_close_rom`/`do_unload_rom`/`closeEvent` (bracketed with `stop_timer()/start_timer()`); `locate_rom_for(peer)` (explanatory box → `QFileDialog` filtered by console → mismatch shown via `show_error` and re-offered); registers favorites as ROM candidates at start.
- **`play_together_dialog.hpp/.cpp`** (new, non-modal `QDialog`): Host tab (port default from settings, display name, "Start hosting" → big code + Copy + local addresses + port-forward/VPN hint), Join tab (code prefilled from `last_join_code`, name, Join), participants table (name, ROM, console, status, behind, fps, replay file), host-only "Reset everyone (3 s)", Leave.
- **`main_window.cpp/.hpp`**: `set_up_play_together_menu()` between Tools and Settings with object names `play-together-host`, `play-together-join`, `play-together-leave`, `play-together-reset-all`, `play-together-show-windows`, `play-together-scale-N`, `play-together-save-replays`; callbacks wired in the constructor; `refresh_action_states` (host/join iff game running, not NDS, not active, no replay loaded; leave/scale/windows iff active; reset-all iff host; GB settings disabled while active); `closeEvent` saves window geometry and leaves.

## 11. Determinism and the smoothness guardrail (must hold)

- The only per-poll additions are `follower.is_some()` and `is_capturing()` checks; every tee call rides an existing per-frame path (`input_latched`, `time.frames > 0`). Verify with `packet_stats` (≈1.0 ChangeInput/frame) on a friend replay and `record_pacing_smoke` unchanged.
- Local thread stays Primary/ABOVE_NORMAL; followers are BELOW_NORMAL, park when caught up, and run capped passes (8 frames / 4 ms) when behind; a follower that cannot keep up resyncs rather than spins.
- Snapshot cost on the publisher: one pooled `create_save_state_into` + channel push per request, coalesced (≤ 1 per 2 s per publisher); compression happens on the writer thread. Sync hash ≈ 0.2 ms every 60 frames on GBA.
- Follower cores never `run()` (no wall clock), never get a sample rate (GB), NDS JIT forced off, built from the publisher's declared console type, no save file (SRAM comes with the snapshot), `ChangeSpeed` never streamed.
- Hashes cover work RAM only; suppressed for `POST_LOAD_FRAMES` after a snapshot; snapshots are exact states (no delta masking), so the GBA "14 bytes differ after seek" class cannot false-positive.

## 12. Edge cases

| Case | Behaviour |
|---|---|
| Friend's ROM not found locally | Peer shows `needs_rom`; Locate ROM prompt; hash verified; stream discarded until subscribe, then a snapshot is requested. |
| Core/BIOS/console mismatch with a publisher | That peer shows `error` with the reason; you still publish and follow others. |
| Publisher pauses | No `NextFrame`s; followers show "Waiting". Snapshot requests while paused are served from the idle path. |
| Follower falls > 300 frames behind / queue > 1200 frames / stream gap / hash mismatch | Snapshot requested (rate-limited), backlog dropped when it arrives; file gets `LoadSaveState` + full keyframe. |
| Slow peer at the host | Its outbound queue overflows → dropped `TooSlow`; nobody else is affected. |
| Client crash / cable pull | 10 s idle timeout → `PeerLeft`; its follower windows close, files finalised at the last frame. |
| Host leaves | Everyone gets `HostLeft`; local games keep running; no host migration in v1. |
| Reset everyone | Countdown is UX only; each participant resets itself at its deadline; followers see `ResetConsole` in-stream. |
| Loading a different ROM / closing the ROM | Confirm dialog, then leave the session. Same-ROM reload/save-file switch re-snapshots instead. |
| Second Hello, garbage, oversized lengths, spoofed `from` | Refused/dropped without panics; `from` is always rewritten by the host. |
| Port in use | "Could not host Play Together on 0.0.0.0:30159: … Change the port in the dialog." |

## 13. Testing and verification

**Unit tests** (cargo, from PowerShell):
- `supershuckie-play-together`: framing round trips, byte-exact layout, truncation at every offset never panics, oversized lengths refused before allocation, unknown tag skipped, hostile counts/strings, packet allow-list, snapshot decompress claims, join-code parsing, name dedupe, follow compatibility; loopback on `127.0.0.1:0` with host + 2 clients (streams contiguous, subscribe → snapshot only to the requester, stream-before-snapshot discarded, coalesced requests, reset-all, client leave, host leave frees the port, refusals, gap → request via a dropping transport, sync-hash ordering); backpressure (slow peer dropped, `publish` never blocks > 5 ms, queue merge); hostile raw sockets (garbage, never-Hello timeout, second Hello, spoofed from, unknown targets, fuzz smoke).
- `supershuckie-core --lib`: publisher emits exactly one `ChangeInput` per emulated frame on a paced core (mirror of the existing latch test); tee survives starting/stopping a file recording (no backwards timestamps); `NextFrame → SyncHash → Keyframe` order; follower consumes one frame per emulated frame; waits then resumes; snapshot supersedes backlog; hash mismatch requests once; hash suppressed after a snapshot; sliced (GB) follower waits only at frame boundaries; follower disk recording plays back identically with a full keyframe at the resync; `AttachLiveSource` replies on every branch.
- `supershuckie-frontend --lib`: settings defaults/round-trip/clamp; `sanitize_display_name`; replay file naming/collisions; ROM lookup by hash (temp dir, stale `known_roms` pruned); refusals.

**Headless end-to-end** (link like `nds_bench`/`record_pacing_smoke`; `timeBeginPeriod(1)` for GB):
- `supershuckie-core/examples/play_together_loopback.rs <rom>`: publisher core at 4x → in-memory framing round trip → follower core; 60 s; assert publisher fps ≥ 90 % of 240, `hash_mismatches == 0`, `snapshots_applied == 1`, frames-behind p99 ≤ 10; `--late-join` and `--hiccup` (freeze follower 2 s → catches up or resyncs, never desyncs). Run on a `.gbc` and a `.gba`.
- `supershuckie-frontend/examples/play_together_smoke.rs <rom> [--players 2|3] [--speed 4] [--seconds 20]`: 2–3 real frontends in one process (each with its own temp UserData and a pre-written `settings.json` disabling REST + Poke-A-Byte so ports don't collide), host on port 0, others join; assert every frontend's local emulation fps ≥ 90 % of target, follower `frames_behind` median < 10 and max ≤ 1 s, `hash_mismatches == 0`, ≥ 1 `peer_refresh_screens` per peer; after leaving, each saved friend replay opens with `ReplayFilePlayer`, has ≈ the publisher's frame count, and plays to its end in a fresh frontend.
- Regression: `record_pacing_smoke` on Platinum/Emerald/Crystal at 4x must still print 240 fps and 1.000 ChangeInput/frame; `packet_stats` on a friend replay ≈ 1.0.

**Manual (user, two machines on a LAN; no GUI automation):** host + join at 4x on GBC and GBA; local status-bar FPS steady while the friend window's "N frames behind" fluctuates; Locate ROM flow; Reset everyone countdown; close/reopen friend windows; pull a cable → window closes, file finalised; the friend replay plays in Replays → Play replay. Build the Windows static exe after phase 6; `scripts/make-attributions.py` must still pass (no new crates).

## 14. Implementation order (each step compiles and is tested before the next)

0. Copy this plan into `play-together-spec.md` at the repo root in the existing spec format (status "approved"), branch `play-together` off `replay-bookmarks`.
1. Recorder visibility + helpers (§5).
2. `supershuckie-play-together`: protocol/framing/code/error + `tests/framing.rs`; then transport/conn + queue tests; then sessions + loopback/backpressure/hostile tests; docs.
3. Core: `stream.rs` tee + `live_replay.rs` + `lib.rs` changes + unit tests.
4. Thread: role, commands, `run_one_follower`, stats; `play_together_loopback` example on GB and GBA.
5. Frontend: settings, `play_together.rs`, `lib.rs` hooks, unit tests, `play_together_smoke` (2 then 3 players).
6. C API + Qt: callbacks, `play_together.h`, `ScreenCanvas` split, `PeerWindow`, controller, dialog, menu. Windows static build.
7. Docs: `docs/play-together.md` (user guide: ports, port forwarding/VPN, codes, what is and isn't streamed), README ports table, spec "implementation notes" section with measured numbers.

## 15. Out of scope (documented follow-ups)

Relay server / NAT traversal; session password; NDS (bandwidth class of 20 MB snapshots; needs per-console tunables); host migration; spectator-only mode; friend windows docked in the main window; mixing friend audio; REST routes for session state; publishing during replay playback.

## 16. Key symbols

| Symbol | File |
|---|---|
| `Packet`, `PacketIO`, `PacketWriteCommand`, `KeyframeMetadata` | `supershuckie-replay-recorder/src/packet.rs`, `packet/io.rs` |
| `ReplayFileRecorderFns`, `new_with_metadata`, `PartialReplayRecordMetadata` | `supershuckie-replay-recorder/src/replay_file/record.rs` |
| `ReplayFileMetadata`, `ReplayConsoleType`, `REPLAY_VERSION` | `supershuckie-replay-recorder/src/replay_file/header.rs` |
| `compress_data`, `decompress_data`, `blake3_hash` | `supershuckie-replay-recorder/src/util.rs` |
| `SuperShuckieCore::{handle_replay, update_input, flush_writes, do_frame_timekeeping, with_recorder, attach_replay_player, splice_live_transient_buffers, POST_LOAD_FRAMES}` | `supershuckie-core/src/lib.rs` |
| `ThreadedSuperShuckieCore`, `ThreadCommand`, `run_thread`, `run_one`, `mark_thread_latency_sensitive` | `supershuckie-core/src/thread.rs` |
| `EmulatorCore::{run_unlocked, memory_regions, memory_region_data, create_save_state_into, encode_input}` | `supershuckie-core/src/emulator.rs` |
| Framing/Encoder/Decoder pattern to copy | `supershuckie-frame-server/src/protocol.rs` |
| Named-thread + socket-timeout + close-notifier pattern | `supershuckie-pokeabyte-integration/src/lib.rs` |
| `SuperShuckieFrontend::{tick, load_rom, make_new_core, switch_core, refresh_screen, start_recording_replay}` | `supershuckie-frontend/src/lib.rs` |
| `Settings`, `clamp` | `supershuckie-frontend/src/settings.rs` |
| Watch-list JSON FFI pattern | `supershuckie-frontend-c/src/memory.rs` |
| `MemoryToolsController` (N windows), `GameRenderWidget`, `set_up_menu`, `refresh_action_states`, `stop_timer/start_timer` | `supershuckie-qt/src/memory_tools_controller.cpp`, `render_widget.cpp`, `main_window.cpp` |
| Headless harness templates | `supershuckie-frontend/examples/record_pacing_smoke.rs`, `supershuckie-core/examples/nds_bench.rs` |

## 17. Implementation notes (2026-09-17)

What was built follows the plan; these are the places it differs or adds to it.

**Ports.** The default host port is **30170**, not 30159: the user's stream overlay
(`stp-unified-frontend`, Electron) already listens on 30159.

**Protocol crate.** `PublisherHandle`/`Session`/`FollowerSink` are as planned. The follower-side
snapshot-request limiter re-arms when a snapshot arrives (a gap right after a join must be
reported at once). Merged `Stream` messages close once they *reach* 256 KiB rather than refusing
the item that would cross it. `ClientSession::has_disconnected()` was added. Everything else the
tests found were test-side races (peer ids are assigned in Hello order, so the tests connect
clients one after another).

**Core.** `replay_player` stays a plain `Option`; the follower is `follower: Option<FollowerState>`
in `live_replay.rs` with `is_playing_back()` covering both. The file-playback keyframe resync was
split into `resync_from_keyframe` (a borrow-checker constraint, no behaviour change). The threaded
core gained `core_name()`. Snapshot counters are absolute in the stream; a follower's file records
the differences.

**Frontend.** "Waiting" needs a 400 ms lull before it shows (a caught-up follower is idle between
frames). Hosting with port 0 means "the configured port"; the smoke picks a free port itself.
Peer replay folders are keyed by the *local* ROM file name.

**Qt.** `GameRenderWidget` is now a `ScreenCanvas` subclass (compositing moved out, input stayed).
The peer callbacks' `user_data` is the main window, which reaches the controller.

**Measured (this machine, 4x, 20 s, loopback, `play_together_smoke`):**

| ROM | Players | Own game fps (every player) | Behind (median / max) | Desyncs | Friend file |
|---|---|---|---|---|---|
| Crystal 2026 v2.2.1 (GBC) | 3 | 238.9 avg, 237.7 worst second | 2–3 / 3 frames | 0 | 6575 frames, 1.000 ChangeInput/frame, 47 KB, plays to the end |
| Emerald 2026 v1.6.0 (GBA) | 3 | 238.9 avg, 238.0 worst second | 2 / 3 frames | 0 | 6120 frames, 1.000 ChangeInput/frame, 101 KB, plays to the end |

Nine emulator cores (3 own + 6 followers) ran in one process for the 3-player runs. The
race-start reset appears exactly once in every friend file. `record_pacing_smoke` on Emerald at 4x
is unchanged (239 fps, 0 over budget, 1.000 ChangeInput/frame). Tests: recorder 88, core 49
(10 new), frontend 29 (4 new), play-together 35, frame server 6, webserver 2.

**Not done by hand:** the Qt windows were built (release exe links) but not driven: hosting/
joining from the dialog, the friend windows, Locate ROM, the countdown label, the leave
confirmation and the windows' geometry persistence need an in-app check on two machines.

**Follow-ups:** a join can take up to 4 s with 3+ players (the publisher-side request coalescer
waits 2 s between announcements); the `errors` list in the state JSON is shown in the dialog but
not the status bar; friend audio is wired but unheard in any test.
