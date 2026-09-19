# Link Cable over Play Together — Implementation Spec

**Target:** Claude Code, working in the supershuckie repository on branch `play-together`.
**Status:** implemented 2026-09-18 (phases 0–2: Game Boy, Game Boy Color, Game Boy Advance; phase 3 infrared not done). Deviations from the text below, as built:
- The wire protocol is **version 4** (sync pause / start state took version 3 meanwhile); the link messages are `0x30`–`0x37` as in §7, and `LinkFrame` also carries `elapsed_millis` (the sender's recording clock as it sends) so the receiver's copy of the sender's console keeps a clock that never runs backwards at the next snapshot. `LinkDecline.reason`: 0 declined, 1 busy, 2 console mismatch, 3 not following, 4 timeout, 5 unavailable; `Unlink.reason`: 0 unplugged, 1 desync, 2 timeout, 3 peer left, 4 failed, 5 busy; unknown values decode to "other". See `docs/play-together-protocol.md`.
- `LinkPort::connect(first)` takes which end of the pair the console is (the Game Boy Advance's lockstep clock owner); the pair's coordinator is created by the first `step_linked` of either side (`mgba_rs::LinkCoordinator`).
- The Game Boy Advance `SerialIn` payload is mGBA's driver log, not the event list in §6.4: tags `0x10` Config, `0x11` SetMode, `0x12` Start, `0x13`–`0x15` Multi/Normal8/Normal32 completions (in call order), `0x16` Async (a register change or transfer scheduling the lockstep driver made on its own, with its cycle from the frame start), `0x1E`/`0x1F` attach/detach; see `mgba-rs/interface.cpp`. The Game Boy Advance test ROM is `supershuckie-core/src/link/gba_test_rom.s` (ARM, devkitARM), embedded in `test_rom.rs`.
- The friend's follower is stepped through the lockstep by the player's own core thread while linked; its own thread forwards commands meanwhile (`ThreadedSuperShuckieCore::lend`), and the link inbox's sink is installed before `LinkStart` goes out so no early frame is lost.
- Not done: up to 4 players (§6.4 last bullet), infrared (§ phase 3).
**Components:** `supershuckie-replay-recorder`, `supershuckie-core`, `supershuckie-play-together`, `supershuckie-frontend`, `supershuckie-frontend-c`, `supershuckie-qt`, `mgba-rs` (phase 2), a vendored+patched `safeboy`; docs.
**No new third-party crates.** One dependency patch (safeboy, see §6.1).

## 1. Context

Two players in a Play Together session want to trade or battle over a link cable as if their consoles were on the same table. The link protocols of these consoles are synchronous and bit/word-clocked with no tolerance for round trips, so the cable cannot be "the network": at 30 ms one way, a Game Boy byte transfer that takes 1 ms on hardware would take 60.

Play Together already puts both consoles in one process on *every* participant's machine: each player emulates every friend's game locally from the friend's replay-packet stream. So the cable is plugged in **locally**, between the player's own core and their follower core of the friend, on both machines. Nothing about the cable crosses the network; what crosses is what already does (inputs and the other per-frame events), only sent **a few frames ahead** so that both machines can run the two consoles in lockstep from identical inputs. The result is deterministic: both machines compute the same pair of consoles.

Done means (phase 1): two players on GB/GBC trade and battle (Red/Blue/Yellow/Gold/Silver/Crystal) over a LAN or a VPN with a 2–6 frame input delay and no desync over an hour; a third player in the session watches both with 0 desyncs; both players' own replay files and the third player's friend replays play back alone and reach the same work RAM as the live session. Phase 2 adds GBA (Gen 3 trades/battles/Union Room). Nintendo DS stays out (not in Play Together).

## 2. Decisions

| Topic | Decision |
|---|---|
| Sync model | **Delay-based lockstep**, not rollback: while linked, everyone's inputs (and RAM writes, resets) are scheduled `D` frames ahead and sent to the partner; the linked pair never runs a frame whose inputs are not known on both machines. `D` is derived from the two players' RTTs to the host (§8.3), overridable in settings. Rollback is a possible later upgrade (§16); GB states are small enough. |
| Where the pair runs | On the **local core's thread**. The friend's follower core is **lent** to it for the duration (§6.3) so its `ThreadedSuperShuckieCore` handle, Poke-A-Byte port, window, stats and replay file keep working unchanged. |
| Scheduling rule | The console with the **lower peer id is "first"** on both machines. GB: step *first* one `GB_run`, then run *second* until its emulated time catches up (SameBoy counts 8 MHz cycles on every model, so raw cycle counts compare directly). GBA: mGBA's lockstep coordinator decides who runs; the rule is "the lowest awake player". Same rule, same code on both machines → identical interleave. |
| Replays stay self-contained | New packet **`SerialIn`** (§5): everything a console *received* over the cable, keyed so it can be replayed into that console alone. Written into the player's own replay, published in the stream (so third parties and their friend-replay files reproduce a linked game without lockstep), mirrored into follower files. Replay format **v7**, protocol **v3**; older peers are refused by the existing version checks. |
| Third parties | Keep following both linked players from their streams as today; the streams carry `SerialIn`. |
| The partner's stream while linked | The linked follower is driven by the lockstep, not by the partner's stream: we **unsubscribe** from the partner for the duration and **re-subscribe (fresh snapshot) at unlink**. A **pair hash** (blake3 of both consoles' work RAM every 60 lockstep frames) rides in the `LinkFrame` messages instead, so a cross-machine desync is caught within a second. |
| Desync while linked | Both sides **unplug** (the games see a pulled cable, which they handle) and the followers resync from snapshots as usual. No attempt to resync a pair. |
| Speed | *(As built: every linked pair runs at the session host's game speed on both machines — `LinkSpeed` from the host, `LinkStart.speed` for the delay; a client's own speed controls do nothing while linked. Speed is pacing only in `RtcMode::Accurate`, so the RTC concern below does not apply.)* Original design: **1x only while linked**, both cores. Besides simplicity, SameBoy's RTC advances per *wall* second (`rtc_second_length` scales with the clock multiplier), so two machines running the pair at different speeds would disagree on the RTC that Gen 2 reads into WRAM. (Same reason a Gen 2 publisher who fast-forwards can make followers resync today — outside this spec.) |
| Pause | Pausing while linked stalls the partner too ("Paused by Ash"). Unpause resumes both. |
| Refused while linked | Loading a save state, attaching/seeking a replay, video export, changing the Game Boy model, fast-forward. **Reset is allowed** (scheduled `D` frames ahead like an input; the race-start reset takes the same path). Recording your own replay, bookmarks, RAM tools, Poke-A-Byte reads and writes (writes are scheduled) all keep working. |
| Players per link | 2 in phase 1 (GB has no more). GBA up to 4 is a phase-2 extension of the same messages (§6.4). |
| Platforms | Phase 1 GB/GBC/SGB2 ↔ same family. Phase 2 GBA ↔ GBA. No GB↔GBA, no DS. Both players must run the same ROM hash (already required to follow). |

Judgement calls (flip if you disagree): the *acceptor* of a link request cannot pre-check the requester's follower on the requester's machine, so both sides validate their own follower and abort in the start handshake otherwise; an incoming request auto-declines after 30 s; a stall of more than 10 s unplugs; the first `D` frames after plugging in use the input each console held at that moment; unlinking drops the `D` queued future inputs rather than draining them.

## 3. What the design builds on (verified in the code, 2026-09-18)

- **SameBoy 1.0.2** (via `sameboy-sys 0.3.0-beta.6`): master side = `GB_set_serial_transfer_bit_start_callback` (announces the bit being sent) / `GB_set_serial_transfer_bit_end_callback` (returns the bit received) called from `GB_serial_master_edge` (`timing.c:184`) and on the SC write that starts a transfer (`memory.c:1734`); slave side = `GB_serial_get_data_bit` / `GB_serial_set_data_bit` (`gb.c:1340-1370`: shifts SB, counts to 8, then clears SC bit 7 and raises IF bit 3); `GB_disconnect_serial`. `GB_run` returns cycles in **8 MHz units on every model** (`timing.c:83` "/ 2 because we use 8MHz units"), so two instances of the same ROM align by comparing raw counts. This is exactly what SameBoy's own Cocoa frontend uses to link two windows.
- **safeboy 0.3.0-beta.6** wraps the master callbacks (`GameboyCallbacks::serial_transfer_bit_start/_end`, enabled by `connect_serial()`, disabled by `disconnect_serial()`) but **not** `GB_serial_get_data_bit`, `GB_serial_set_data_bit` or `GB_set_infrared_input`, and `RunningGameboy.gb` is private → patch (§6.1). `Gameboy { inner: Pin<Box<RunningGameboy>> }`, so a pointer to the pinned instance is stable.
- **The GB core already steps two SameBoy instances in lockstep**: `GameBoyColor::step` (`emulator/game_boy_color.rs:223`) runs one `GB_run` on the emulated instance and one on the audio shadow, comparing cycle counts. `mid_frame = frames == 0`; `update_input`/`flush_writes`/`handle_replay` only act at frame boundaries.
- **mGBA (0.11-dev)** ships the rewritten lockstep SIO: `GBASIOLockstepCoordinator` + one `GBASIOLockstepDriver` per core with an `mLockstepUser { sleep, wake, requestedId, playerIdChanged }` (`include/mgba/internal/gba/sio/lockstep.h`, `core/lockstep.h`). Player 0 sleeps until the others ack; others sleep when ahead; `_verifyAwake` asserts never all asleep. `runLoop` (`ARMRunLoop`) returns at every timing event, so a cooperative single-thread scheduler can drive it. `GBASIODriver` vtable (`include/mgba/gba/interface.h:111`): `setMode, handlesMode, connectedDevices, deviceId, writeSIOCNT, writeRCNT, start, finishMultiplayer(data[4]), finishNormal8, finishNormal32, loadState, saveState`. `mgba-rs/interface.cpp` only exposes `runFrame` today.
- **Core** (`supershuckie-core/src/lib.rs`): `do_run_fn` = `before_run` (`handle_replay` → `update_input` → `flush_writes`) → run → `after_run` (`do_frame_timekeeping` writes `NextFrame`, `push_keyframe_if_needed`, `service_stream`). `update_input` latches once per frame (`input_latched`). Followers: `FollowerState` (`live_replay.rs`), `handle_live_replay` pops packets to `NextFrame`, `apply_playback_packet` applies them and `follower_mirror` copies them into the friend's file; `apply_stream_snapshot`; `check_sync_hash`. Playback gates: `is_playing_back()`.
- **Thread** (`thread.rs`): `ThreadedSuperShuckieCoreThread { core: SuperShuckieCore, receiver, pokeabyte_integration, memory_monitor, screens (Weak), elapsed_time/frame_times/emulated_frames (Arc), follower: Option<FollowerLoopState>, ... }`; `run_thread` drains commands, does housekeeping (`handle_pokeabyte_integration`, `refresh_screen_data`, `update_queued_screens`, `handle_memory_monitor`, error drains), then `run_one` (primary, wall-clock paced) or `run_one_follower` (paces on arrival). Every `call()` reply must be answered on every branch.
- **Play Together crate**: `Message` enum + framing (`protocol/mod.rs`, `wire.rs`), packet allow-list (`protocol/stream.rs::forbidden_kind`), host relay in `session/host.rs::handle_admitted` (targeted relay pattern: `RequestSnapshot`, targeted `Snapshot`), client dispatch `session/client.rs::handle_message`, `Session` trait, `FollowerSink` delivered from the reader thread, `SessionEvent` drained in the frontend tick, `RttUpdated` events from `Pong`. `PROTOCOL_VERSION = 2`.
- **Recorder**: `PacketDiscriminator` (`packet/io.rs:258`) uses `0xF0–0xF9`, `0xFE`; `REPLAY_VERSION = 6` (`replay_file/header.rs:36`); `ReplayFileRecorderFns` (`record.rs:1039`).
- **Frontend/Qt**: `play_together.rs` (`PlayTogetherSession`, `PeerInstance { core: Option<ThreadedSuperShuckieCore>, .. }`, `SessionPublisher`, `FeederSink`, `tick_play_together` event match), C API `play_together.h` (JSON state + generation), Qt `PlayTogetherController::tick/apply_state`, `PeerWindow::contextMenuEvent`, non-modal dialog precedent (`play_together_dialog`), modal dialogs bracket `stop_timer()/start_timer()`.

## 4. Architecture

```
 machine A (peer 2, "first")                                         machine B (peer 3, "second")
 ┌ local core thread (Primary) ───────────────────────────┐          ┌ local core thread ────────────────────────────┐
 │ A_core: SuperShuckieCore (publishes stream as today)   │          │ B_core                                        │
 │   link: LinkState { delay D, local_queue, inbox, … }   │          │   link: …                                     │
 │ B_follower: lent CoreLoop (follower of B, driven here) │          │ A_follower: lent CoreLoop                     │
 │   handle_link_events() ← inbox (B's LinkFrames)        │          │   ← inbox (A's LinkFrames)                    │
 │ run_one_linked(): step A_core; catch B_follower up;    │          │ run_one_linked(): step A_follower; catch      │
 │   GB serial callbacks call straight into the partner   │          │   B_core up  (same rule: first = lower id)    │
 └────────────────────────────────────────────────────────┘          └───────────────────────────────────────────────┘
        │ LinkFrame{frame k+D, events, pair_hash?}  ──host relay──►  inbox                    (and symmetric ◄──)
        │ Stream (inputs, SerialIn, NextFrame…)     ──host relay──►  third parties' followers (no lockstep needed)
```

- **Inputs are scheduled, not applied.** While linked, `update_input` computes the frame's input exactly as today but queues it for frame `now + D` and sends it to the partner; the input applied *now* is the one queued `D` frames ago. RAM writes and resets take the same queue. Both machines apply the same events to both consoles at the same frames.
- **The cable is local.** GB: the partner's SameBoy instance is reachable from the serial callbacks for the duration of one step (§6.1). GBA: both cores share one lockstep coordinator (§6.4).
- **`SerialIn` records what came in.** Captured per frame on any core whose port is live (the local core and the lent follower alike), written before that frame's `NextFrame`; replayed on any core whose port is not live (file playback, third-party followers, the audio shadow).
- **The handshake pauses both games for about one round trip** (§8.2): accept → each side stops at a frame boundary and announces its frame; each side runs its follower of the other up to that frame; plug in; go.

## 5. Recorder crate — `supershuckie-replay-recorder`

- `Packet::SerialIn { data: ByteVec }` — console-specific bytes (§6.1 for GB, §6.4 for GBA), covering the frame that the following `NextFrame` closes. Discriminator `PacketDiscriminator::SerialIn = 0xFA`. Encoded like `WriteMemoryVar` (length-prefixed bytes); `read_all` refuses lengths above 64 KiB.
- `REPLAY_VERSION` 6 → **7**; header docs list the change. Reading v6 files is untouched (no `SerialIn` in them). `REPLAY_VERSION_CURRENT_ENCODING` stays 4 (nothing about keyframes changes; the converter must not re-encode v6 files just for this).
- `ReplayFileRecorderFns::serial_in(&mut self, data: ByteVec) -> Result<(), ReplayFileWriteError>` implemented by `ReplayFileRecorder`, `NonBlockingReplayFileRecorder` and the test recorders; it writes the packet like `write_memory` does. Keyframes need nothing: the events for a frame are read from the packets immediately before that frame runs, and a core's link-time counters restart at every frame boundary.
- Playback (`ReplayFilePlayer`): `SerialIn` is an ordinary packet handed to the core; `packet_stats` counts it. `test_support` fixtures gain a `SerialIn` case; the corruption tests get a truncated/oversized `SerialIn`.

## 6. Core — `supershuckie-core`

### 6.1 Game Boy link port (`emulator/game_boy_color.rs`, new `emulator/link.rs`)

**safeboy patch.** Vendor `safeboy 0.3.0-beta.6` under `third-party/safeboy/` with `[patch.crates-io] safeboy = { path = "third-party/safeboy" }` in the workspace `Cargo.toml` (same GPL-3 licence, nothing new for `make-attributions.py`; keep the diff as `third-party/safeboy/patches/0001-serial-slave-api.patch` like `melonds-rs/patches`, and offer it upstream). Additions to `RunnableInstanceFunctions` (both impls): `fn serial_get_data_bit(&self) -> bool`, `fn serial_set_data_bit(&mut self, bit: bool)`, `fn set_infrared_input(&mut self, on: bool)` (phase 3), and `fn serial_callbacks_connected(&self) -> bool`.

**`EmulatorCore` additions** (defaults: not linkable):
```rust
fn link_port(&mut self) -> Option<&mut dyn LinkPort> { None }
fn as_any_mut(&mut self) -> &mut dyn core::any::Any;     // for the console-specific pair scheduler to downcast the partner

pub trait LinkPort {
    /// Wire the console's serial hardware to `partner` for exactly one step of this core, run that step, unwire.
    /// GB: sets the partner pointer the callbacks use, one `GB_run`, clears it. Returns the step's RunTime.
    fn step_linked(&mut self, partner: &mut dyn EmulatorCore) -> Result<RunTime, LinkError>;
    /// Emulated time since `connect`, in the console's link unit (GB: 8 MHz cycles).
    fn link_time(&self) -> u64;
    fn connect(&mut self) -> Result<(), LinkError>;       // SerialIn capture on, callbacks installed, link_time = 0
    fn disconnect(&mut self);                               // GB_disconnect_serial; the game sees no cable
    /// Serial events received during the frame just completed (empty when none); cleared by the call.
    fn take_serial_in(&mut self, into: &mut Vec<u8>);
    /// Queue recorded events for the frame about to run (playback/follow); refuses malformed data.
    fn queue_serial_in(&mut self, data: &[u8]) -> Result<(), LinkError>;
    /// Playback bookkeeping: events that could not be delivered where recorded (a desync indicator).
    fn serial_replay_misses(&self) -> u64;
}
```

**GB implementation.**
- `GameBoyCallbackData` gains `link: LinkPortState { partner: Cell<*mut RunningGameboy> /* null unless inside step_linked */, mode: Live | Replay, log: SerialLog, replay: SerialReplayer, link_cycles: u64 /* 8 MHz cycles since the step that completed the last frame */, connected: bool }`. `CallbackHandler` implements `serial_transfer_bit_start(bit)` = remember `bit`; `serial_transfer_bit_end()` = **Live:** `let got = partner.serial_get_data_bit(); partner.serial_set_data_bit(remembered); log.master_bit(got); got` — the same three lines as SameBoy's Cocoa link, and the partner also logs `slave_bit(partner.link_cycles, remembered)`; **Replay:** pop the next recorded master bit (`true` = "no cable" and a miss counted when the queue is empty).
- `step_linked(partner)`: downcast `partner.as_any_mut()` to `GameBoyColor`, store a pointer to its pinned `RunningGameboy` in `link.partner` (SAFETY: the partner is not running, is not moved for the duration, and only this thread touches either instance; the `&mut` we hold is not used again until the pointer is cleared), `step()`, clear the pointer, `link_cycles += cycles` (reset to 0 when the step completed a frame), return. `connect_serial()` on both instances at `connect`; `disconnect_serial()` at `disconnect`. Calling `run`/`run_unlocked` on a connected core without `step_linked` is a bug (`debug_assert`), except in Replay mode.
- Slave-side timing in Replay mode: after every step, `SerialReplayer` applies every queued slave event whose `cycles == link_cycles` via `serial_set_data_bit`; an event with `cycles < link_cycles` is a miss (applied late, counted). Determinism: the same instruction stream produces the same step boundaries, so the recorded boundary is reached exactly.
- **`SerialIn` bytes (GB):** a tag stream, little-endian varints: `0x01 M count, packed bits MSB-first` (bits the master received, in callback order); `0x02 S count, then (cycle_delta varint, bit u8)*` (bits shifted into a slave, cycle position relative to the previous event, the first relative to the frame boundary); `0x04 C connected u8` (emitted on plug/unplug so a replayed master knows whether an empty queue means "no cable" or "desync"); `0x03 I (cycle_delta, on u8)*` reserved for infrared (phase 3). A byte transfer is 8 `S` events of ~3 bytes each; a Pokémon trade is a few thousand events — tens of KB, which the blob compression squeezes further. A frame with no events writes no packet. Invariant: a step completes at most one frame (`debug_assert!(time.frames <= 1)` where the packet is taken).
- **Audio shadow:** the shadow runs one `GB_run` behind the emulated instance inside the same `step`; give it a `SerialReplayer` in Replay mode fed from the emulated instance's log for that step (master bits queued before the shadow's step, slave bits applied after it at the same boundary). Without this the shadow would resync on every byte of a transfer.
- `hard_reset` keeps the port (the cable is still plugged in after a reset); `load_save_state` in Live mode is refused upstream (§8.5).

### 6.2 `SuperShuckieCore` link state (`src/link.rs`, new; `cfg(feature = "std")`)

```rust
pub struct LinkSettings { pub delay_frames: u64, pub local_is_first: bool, pub local_start_frame: u64, pub partner_start_frame: u64, pub partner_start_input: InputBuffer }
pub struct LinkInbox { /* Mutex<BTreeMap<u64 /*partner frame*/, LinkFrame>>, newest: AtomicU64, ended: AtomicBool, waker */ }
pub struct LinkFrame { pub events: Vec<Packet> /* ChangeInput | WriteMemory | ResetConsole */, pub pair_hash: Option<(u64, [u8; 32])> }
pub trait LinkPublisherFns: Send + 'static { fn frame(&mut self, frame: u64, events: Vec<Packet>, pair_hash: Option<(u64, [u8; 32])>); fn poll_errors(&mut self) -> Vec<String>; }
pub enum LinkRunOutcome { Ran { local: RunTime, partner: RunTime }, Stalled, Failed(LinkFailure) }
pub enum LinkFailure { PairHashMismatch { frame }, PartnerEnded, PartnerStreamMismatch, Timeout, Emulator(String) }
```

`SuperShuckieCore` gains `link: Option<LinkState>` (local side) and `link_partner: Option<LinkPartnerState>` (the lent follower side) and:
- `link_hold() -> (u64, InputBuffer)`: `finish_current_frame()`, set `link_holding = true` (`do_run_fn` skips the run like `replay_waiting`; not the user's pause), return `(total_frames, current encoded input)`.
- `begin_link(&mut self, partner: &mut SuperShuckieCore, settings, inbox: Arc<LinkInbox>, publisher: Box<dyn LinkPublisherFns>) -> Result<(), String>`: requires a held local core at `local_start_frame`, a following partner core at exactly `partner_start_frame` and not mid-frame, same console family, both `link_port()`s present. Pre-fills `local_queue[local_start .. +D)` with `ChangeInput{current input}` and the partner's inbox-side queue `[partner_start .. +D)` with `ChangeInput{partner_start_input}`; `connect()` both ports; `link_time` bookkeeping zeroed; partner switched to `LinkPartnerState { queue: Arc<LinkInbox>, stream_ring }` (its `handle_live_replay` is bypassed, §6.2 partner side); publishes the config event through `SerialIn`; `input_latched = false` on both.
- `run_linked(&mut self, partner: &mut SuperShuckieCore) -> LinkRunOutcome` — one slice (the paced core steps once):
  ```
  (first, second) = if local_is_first {(self, partner)} else {(partner, self)}
  if second.link_time < first.link_time { catch second up: second.run_linked_step() until link_time ≥ or it stalls (events for its next frame missing) → Stalled }
  if first at a frame boundary && !events_available(first.next_frame) → Stalled
  first.run_linked_step()            // before_run (link events at boundaries) → port.step_linked(second) → after_run (SerialIn capture, NextFrame, sync hash, stream)
  catch second up again (same loop)
  → Ran
  ```
  `run_linked_step` on the local core uses the paced path (`run` → SameBoy sleeps at vblank as today); on the partner the unpaced presenting path (`run_unlocked_presenting(draw = true)`: its window is live now). GBA: the "first" step is one `runLoop` slice and the catch-up is "run whichever player is awake" (§6.4) — the console-specific part lives behind `LinkPort`, the frame gating and event queues here.
- **Local side, while linked:** `LinkState.next_send_frame` starts at `local_start + D` (the pre-filled frames are never sent; the partner pre-fills them itself). Every scheduled event goes into `local_queue[next_send_frame]`: `update_input` builds `current_input` as today and pushes `ChangeInput{encoded}` there instead of applying it; `flush_writes` moves `self.writes` there as `WriteMemory` packets; `hard_reset` pushes `ResetConsole` there (no immediate reset, whether called at a boundary or mid-frame). At the end of `before_run` on a frame boundary, `local_queue[next_send_frame]` is sent once through `publisher.frame` (with `pair_hash` when due) and `next_send_frame += 1`; then `local_queue.remove(total_frames)` is applied in the fixed order reset → input → writes (`set_input_encoded` + recorder/publisher `set_input`, still exactly one `ChangeInput` per frame in the stream; `write_ram` and the write is recorded only if it succeeded, as today; the reset does what `hard_reset` does minus `finish_current_frame`). Both consoles on both machines apply the same order.
- **SerialIn capture** for any core with a live port: in `do_frame_timekeeping`, when `time.frames > 0`, `port.take_serial_in()` → `recorder.serial_in` + `publisher.serial_in` **before** `next_frame`. `StreamPublisherFns` gains `fn serial_in(&mut self, data: ByteVec)`. Order on the wire: `ChangeInput(N) … SerialIn(N) NextFrame(N) [SyncHash(N)]`.
- **SerialIn playback** for any core with a non-live port: `apply_playback_packet(Packet::SerialIn)` → `port.queue_serial_in(data)` (misses are counted into `replay_write_failures`-style stats: `serial_replay_misses`) and `follower_mirror` copies it into the friend's file. Cores without a port ignore it.
- **Partner side** (`LinkPartnerState`): at each frame boundary `handle_link_events()` replaces `handle_live_replay()`: take `inbox[total_frames]` (missing → `replay_waiting = true`, stats `waiting`), apply its packets through `apply_playback_packet` (so `follower_mirror` records them), then a synthetic `NextFrame{nominal delta}` (`nominal_frame_micros`, remainder carried so the file's clock does not drift; the follower's absolute `total_milliseconds` is re-based by the snapshot at unlink). `inbox.ended` → `LinkFailure::PartnerEnded`. Pair hashes: keep the last 4 `(frame, hash)` pairs this machine computed (`pair_hash = blake3(first.sync_hash() ‖ second.sync_hash())` at lockstep frames `k % 60 == 0`, keyed by the *first* console's frame); compare each incoming `pair_hash` against the entry for that frame → mismatch = `Failed(PairHashMismatch)`.
- `end_link(&mut self, partner, reason)`: `disconnect()` both ports (emits `C 0`), drop `local_queue` (future inputs) and `link`, `partner.link_partner = None` (its `follower` stays; the frontend re-subscribes so the next snapshot resyncs it: `replay_waiting = true` until then), `input_latched = false` on both, `link_holding = false`.
- Refusals in the core (return `Err`/ignore, the frontend also refuses earlier): `load_save_state`, `attach_replay_player`, `attach_live_replay_source`, `go_to_replay_frame`, `set_speed` ≠ 1x while `link.is_some() || link_partner.is_some()`.

### 6.3 Thread — lending a core (`thread.rs`)

- Split `ThreadedSuperShuckieCoreThread` into `struct CoreThread { receiver, sender_close, loop_: Option<Box<CoreLoop>>, lent: Option<LentOut> }` and `struct CoreLoop { …every other field… }` with the existing methods (`handle_command`, `housekeeping()` = the `handle_*`/`refresh_screen_data`/`update_queued_screens`/`handle_pokeabyte_integration`/`handle_memory_monitor`/`check_if_replay_stalled` block, `run_one`, `run_one_follower`, `is_running`, `update_counters`). No behaviour change when nothing is lent (`record_pacing_smoke` and `play_together_smoke` must not move).
- `ThreadCommand::Lend(Sender<LentCore>)` (follower handle): the thread moves its `Box<CoreLoop>` into `LentCore { loop_: Box<CoreLoop>, commands: Receiver<ThreadCommand>, return_to: Sender<ReturnedCore> }`, keeps the matching `forward: Sender<ThreadCommand>` and `returned: Receiver<ReturnedCore>`, and enters **forwarding mode**: `recv_timeout(10 ms)` on its own receiver → `forward.send(cmd)`; `returned.try_recv()` → back to normal; a disconnected `returned` (the primary thread died) → exit like `Close`. `ReturnedCore { loop_, then_close: bool }`: a forwarded `Close` makes the primary unlink, return the loop with `then_close`, and the follower thread finishes as usual.
- `ThreadCommand::Link { lent: LentCore, settings: LinkSettings, inbox: Arc<LinkInbox>, publisher: Box<dyn LinkPublisherFns>, reply: Sender<Result<(), String>> }`, `ThreadCommand::Unlink(Sender<()>)`, `ThreadCommand::LinkHold(Sender<(u64, InputBuffer)>)`, `ThreadCommand::LinkRelease(Sender<()>)` (abort a hold without linking). Handle methods: `link_hold()`, `link_release()`, `lend()`, `link(lent, ..)`, `unlink()`, `link_failure() -> Option<LinkFailure>` (shared mutex like `stream_errors`, never a `call()` from the UI tick).
- `run_thread` on the primary while `linked.is_some()`: each iteration drains its own commands **and** `linked.lent.commands` (executed by `linked.lent.loop_.handle_command(cmd)` against the partner's loop, unchanged code), runs `housekeeping()` for both loops, then `run_one_linked()`: `self.loop_.core.run_linked(&mut lent.loop_.core)`; `Stalled` → `park_timeout(1 ms)` (inbox pushes unpark; the wait is bounded so forwarded commands and Poke-A-Byte stay responsive; a stall longer than 10 s → `Failed(Timeout)`); `Failed` → record it for the frontend, `end_link`, return the loop. Stats: the partner's `emulated_frames`/`frame_times` are updated by the primary loop through the lent loop's own `Arc`s; `frames_behind = 0`, `waiting = stalled`.
- Link mode on the primary must add nothing per poll beyond `linked.is_some()`; everything else rides the existing per-frame paths (`input_latched`, `time.frames > 0`).

### 6.4 Game Boy Advance (phase 2; `mgba-rs`, `emulator/game_boy_advance.rs`)

- `interface.cpp` additions: `mgba_rs_link_coordinator_new/free`, `mgba_rs_core_link_attach(core, coordinator, user_callbacks)` (creates a `GBASIOLockstepDriver` with a custom `mLockstepUser` whose `sleep`/`wake` set a flag on the core's driver context and whose `requestedId` returns first=0/second=1; `GBASIOSetDriver`; `GBASIOLockstepCoordinatorAttach`), `mgba_rs_core_link_detach`, `mgba_rs_core_run_loop(core) -> frames_completed` (one `runLoop` slice; a frame is completed when `gba->video.frameCounter` changed — the `_GBACoreRunFrame` test), `mgba_rs_core_is_asleep`. The pair scheduler for GBA: `loop { p = lowest awake player; p.run_loop(); stop when the local core completed a frame }`; `link_time` = `mTimingCurrentTime`. Spike this first: two cores + one coordinator on one thread, with the `_verifyAwake` assertions on.
- `GameBoyAdvance` in link mode runs **sliced** (`is_mid_frame()` true between slices of a frame; the deadline check only at frame boundaries — no new frame before it is due, `RunTime::NONE` otherwise). Outside link mode nothing changes.
- **SerialIn (GBA)** = a replay `GBASIODriver` (`mgba_rs_replay_sio_driver`) that answers every entry point the game can observe from the recorded log and delivers transfers by scheduling an `mTimingEvent` at the recorded cycle: events `0x10 Config{devices, id}`, `0x11 Siocnt{returned u16}`, `0x12 Rcnt{returned u16}`, `0x16 SetMode{mode}` (in call order — the game's own writes drive them), `0x13 Multi{cycle_delta, data[4]}`, `0x14 Normal8{cycle_delta, data}`, `0x15 Normal32{cycle_delta, data}` (completion time relative to the frame start, `mTimingCurrentTime` at the lockstep driver's completion; a transfer that spans a frame boundary is recorded in the frame it *completes* in, so the replay driver never needs to know at `start()` when it ends — confirm against `sio.c`'s `completeEvent` handling during the spike). Recorded on the linked machines by wrapping the lockstep driver's vtable. Check that a save state taken with the lockstep driver loads under the replay driver (`loadState` is per driver id).
- Up to 4 players: `LinkRequest` to several peers, one coordinator, "first" = lowest id, `LinkFrame`s fan out to every member; §7's messages already carry `target`, so only the frontend state machine grows. Not in phase 2's definition of done.

## 7. Play Together crate — protocol v3 (`supershuckie-play-together`)

`PROTOCOL_VERSION = 3`; `Hello.replay_version` must be 7 (the existing check). New messages (all relayed by the host like `RequestSnapshot`; `from` is always overwritten by the host; the host itself can be either end):

| Tag | Message | Direction |
|---|---|---|
| 0x30 | `LinkRequest { from, target, nonce u32, console u32 }` | requester → host → target |
| 0x31 | `LinkAccept { from, target, nonce }` / 0x32 `LinkDecline { from, target, nonce, reason u32 }` | target → host → requester |
| 0x33 | `LinkStart { from, target, nonce, frame u64, input bytes, rtt_millis u32, delay_setting u8 }` | both, after pausing |
| 0x34 | `LinkFrame { from, target, frame u64, events bytes, pair_hash_frame u64, pair_hash [32] (all-zero = none) }` | both, once per lockstep frame |
| 0x35 | `Unlink { from, target, reason u32 }` | either → host → other |
| 0x36 | `PeerLinked { a, b }` / 0x37 `PeerUnlinked { a, b }` | host → everyone (informational, for the roster) |

- `LinkFrame.events` is packet bytes with the allow-list `NoOp | ChangeInput | WriteMemory | ResetConsole` (`forbidden_kind` gets a `link` variant); ≤ 64 KiB. `Stream`'s allow-list adds `SerialIn`.
- **Host bookkeeping** (`host.rs`): `links: Vec<(PeerId, PeerId)>`; a `LinkRequest` is refused (`LinkDecline { reason: Busy | ConsoleMismatch | NotFollowing }`) when either end is already linked or the consoles' families differ; `LinkAccept` records the pair and broadcasts `PeerLinked`; `Unlink` or a `PeerLeft` of either end clears it, broadcasts `PeerUnlinked` and sends `Unlink { reason: Left }` to the survivor. Everything else is relayed by `target` exactly like a targeted `Snapshot`.
- **Session API**: `fn send_link(&self, message: LinkMessage) -> Result<(), PlayTogetherError>` (queued on the connection's writer like snapshot requests); `fn set_link_sink(&self, peer: PeerId, sink: Option<Box<dyn LinkSink>>)` — `LinkFrame`s from `peer` are decoded on the **reader thread** and handed straight to the sink (like `FollowerSink`; a modal dialog on the UI thread must never stall the pair); `pub trait LinkSink: Send + 'static { fn frame(&mut self, frame: u64, events: Vec<Packet>, pair_hash: Option<(u64, Blake3Hash)>); fn ended(&mut self, reason: LeaveReason); }`. `SessionEvent::Link(LinkEvent)` with `Requested { from, nonce }`, `Accepted { from, nonce }`, `Declined { from, nonce, reason }`, `Started { from, nonce, frame, input, rtt_millis, delay_setting }`, `Unlinked { from, reason }`, `PeerLinked { a, b }`, `PeerUnlinked { a, b }`. `SessionStats` gains `link_frames_sent/received`.
- Priority: the writer thread sends `LinkFrame` ahead of any queued `Stream`/`Snapshot` bytes (a second, small outbound queue drained first), so a snapshot to a third party never delays the lockstep.
- Hostile cases: a `LinkFrame` from a peer we are not linked with is dropped; frames older than the newest applied or more than 600 ahead are dropped; unknown reasons decode to `Other`.
- `docs/play-together-protocol.md`: the table, a "Link cable" section (handshake sequence, delay, pair hash), version history entry.

## 8. Frontend — `supershuckie-frontend`

### 8.1 Settings
`PlayTogetherSettings.link_input_delay: u8` (0 = Auto, else 1..=15 frames; `clamp()`), `link_auto_accept_from_host: bool` (false; hidden, for the smoke tool).

### 8.2 State machine (`play_together.rs`)
`PlayTogetherSession.link: Option<LinkState>` with phases:
- `Requesting { peer, nonce, since }` → on `Accepted`: `core.link_hold()` (a `call()`; it is a one-off, not in the tick's hot path — do it once and remember), `session.send_link(LinkStart { frame, input, rtt, delay_setting })`, phase `Starting`. `Declined`/30 s → back to none with a message.
- `Incoming { peer, nonce, since }` → the UI answers; accept = the same hold + `LinkStart`; 30 s → auto-decline.
- `Starting { peer, my_start, their_start: Option<..>, since }` → on `Started`: compute `D` (§8.3); `let lent = peer.core.lend()?`; `session.unsubscribe(peer)` (the follower is now driven by the lockstep; the feeder is left attached but idle); `session.set_link_sink(peer, InboxSink(inbox))`; `core.link(lent, settings, inbox, SessionLinkPublisher(handle, peer))` — the primary thread runs the lent follower up to `their_start` from what the feeder already holds (the partner stopped there, so its stream ends there; cap the follower loop at the target and never overshoot), then plugs in. 5 s without `Started` or without the follower reaching the frame → `Unlink { reason: Timeout }`, `link_release()`, resubscribe. Both cores forced to speed 1x for the duration (restore the base speed at unlink).
- `Linked { peer, delay, since, stalled_since: Option<Instant> }` → tick: `core.link_failure()` → unlink with the reason; `Unlinked` event → same; the partner `Left` → same; status text "Linked with Ash · 3 frames delay" / "Waiting for Ash…" after 100 ms of stall.
- `unlink(reason)`: `core.unlink()` (returns the loop to the follower thread), `session.set_link_sink(peer, None)`, `session.send_link(Unlink)`, `session.subscribe(peer, FeederSink)` (fresh snapshot → the follower continues; its file gets `LoadSaveState` + a full keyframe as today), restore speed, bump generation.

`InboxSink` pushes into `LinkInbox` (unparks the core thread). `SessionLinkPublisher` implements `LinkPublisherFns` over `PublisherHandle`-style queuing (`session.send_link` is `&self`, `Send + Sync`).

### 8.3 Input delay
`D = max(ceil((rtt_a + rtt_b) / 2 / frame_ms) + 1, manual_a, manual_b)` clamped to `1..=15`, where `rtt_x` is each side's last RTT to the host (from `RttUpdated`; the host's own is 0), `frame_ms` = 1000 / native rate, `manual_x` = that side's `link_input_delay` setting (0 = Auto). Both sides compute it from the same two `LinkStart`s → identical. Shown in the UI. A `LinkFrame` arriving late stalls the pair rather than being skipped.

### 8.4 Public API
`play_together_link_request(peer)` (preconditions: session active, peer `Following` with `frames_behind < 30` and no snapshot pending, same console family, GB/GBC/SGB2 (GBA in phase 2), no link, no replay attached, speed 1x or will be set), `play_together_link_respond(nonce, accept)`, `play_together_unlink()`, `play_together_link_state() -> LinkView` (serde), `get/set_play_together_link_input_delay`. `PlayTogetherStateView` gains `link: LinkView { phase: none|requesting|incoming|starting|linked, peer_id, peer_name, nonce, input_delay, stalled, since_ms, last_reason }` and per-participant `linked_with: Option<PeerId>` (from `PeerLinked`).

### 8.5 Refusals and hooks in `lib.rs`
While `link.is_some()`: `load_save_state` / quick-load, replay load/seek/continue, video export, GB model/SGB toggles, speed changes other than 1x, unloading the ROM without confirming ("Unplug the link cable first" / the existing leave confirmation covers close). `hard_reset` goes through (scheduled). `unload_rom`/`play_together_leave` unlink first. RAM tool and Poke-A-Byte writes are scheduled transparently (they already go through `enqueue_write`).

## 9. C API — `supershuckie-frontend-c`
`supershuckie_frontend_play_together_link_request(f, peer, err, len)`, `..._link_respond(f, nonce, accept, err, len)`, `..._unlink(f)`, `..._get_link_input_delay(f)` / `..._set_link_input_delay(f, frames)`. The `link` object and `linked_with` fields are part of the state JSON; `generation` bumps on every phase change (the prompt and the strip key off it).

## 10. Qt — `supershuckie-qt`
- `PeerWindow::contextMenuEvent`: "Plug in link cable" (enabled per §8.4 preconditions, read from the state JSON) / "Unplug link cable"; the status strip shows "🔗 Linked · 3 frames delay", "Connecting cable…", "Waiting for Ash…" (linked and stalled) in place of the frames-behind text; the window is live at 60 fps while linked.
- `PlayTogetherController::apply_state`: `link.phase == "incoming"` → a **non-modal** `QMessageBox` (`open()`, not `exec()`: the tick must keep running for the handshake) "Ash wants to plug a link cable into your game. Both games will run with a 3-frame input delay while linked." [Plug in] [Decline]; closed automatically when the phase changes; result → `..._link_respond`. `link.last_reason` changes → `show_error`-style toast ("The link cable was unplugged: desync"). Main window status label "Link cable: Ash"; title unchanged.
- Play Together menu: "Unplug link cable" (object name `play-together-unlink`), "Link cable input delay ▸ Auto, 1…15" (`play-together-link-delay-N`, persisted). `refresh_action_states`: fast-forward/turbo, save-state load, replay actions disabled while linked.
- `closeEvent`/`do_close_rom`: the existing leave confirmation mentions the cable when linked.

## 11. Docs
- `docs/play_together.md`: a "Link cable" section (how to plug in, the delay and why both games pause when one does, what is refused while linked, what happens on a desync, third players and replays keep working).
- `docs/play-together-protocol.md` v3 (§7).
- Recorder crate docs: the `SerialIn` packet, its GB/GBA payloads, v7.
- README feature list: one line.

## 12. Determinism and guardrails (must hold)

- The pair's inputs are complete before a frame runs (delay queue on both sides, pre-fill at start); the interleave rule is `first = lower peer id` on both machines and implemented once (the `(first, second)` swap in `run_linked`), never in terms of local/partner.
- SameBoy: the emulated instances never get a sample rate (the shadow does, as today); `RtcMode::Accurate`; speed 1x while linked (RTC); the same model on both sides (already required to follow). A hard reset keeps the port.
- `SerialIn` is taken and written exactly once per completed frame, before `NextFrame`, on every core with a live port; replayed on every core with a non-live port. `packet_stats` on a linked replay ≈ 1.0 `ChangeInput`/frame and ≤ 1.0 `SerialIn`/frame.
- Pair hash every 60 lockstep frames catches a cross-machine divergence within a second; a local `serial_replay_misses > 0` on a follower is a desync indicator (surface it in `FollowerStatsSnapshot`).
- Per-poll cost on the primary thread: one `linked.is_some()`; the partner's housekeeping runs once per iteration (its screen copy is ~10 µs); the stall wait is a bounded park. `play_together_smoke` keeps its p99 tick guard; `record_pacing_smoke` is unchanged by the thread split.
- Everything the reader thread does for a link is decode + push + unpark; `LinkFrame`s jump the outbound queue.

## 13. Edge cases

| Case | Behaviour |
|---|---|
| Partner's `LinkFrame` late | Both consoles stall (the partner is stalled too, waiting for ours); "Waiting for Ash…" after 100 ms; unplug after 10 s. |
| Partner pauses | Same as late, indefinitely; "Paused by Ash". |
| Hash mismatch (pair or local miss) | `Unlink { Desync }` both ways, ports disconnected, followers re-subscribe and resync from snapshots; the game handles the pulled cable. |
| Partner leaves / host leaves / connection drops | Unlink with that reason; local game continues immediately. |
| Reset while linked (own or race start) | Scheduled `D` frames ahead; both machines reset that console at that frame; the stream carries `ResetConsole` as today. |
| Third party asks the linked player for a snapshot | Served as today (exact state); their follower continues with the stream's `SerialIn`. |
| A follower on a third machine is resyncing when the link starts | Nothing special: `SerialIn` is in the stream from the plug-in frame on and every frame after a snapshot is complete. |
| Link request while the target's follower of the requester is not in sync | Auto-decline `NotFollowing`; the requester sees why. |
| Both request each other at once | Lower peer id's request wins (the other is declined `Busy` by the host's bookkeeping). |
| Follower core dies while lent | The primary sees `Failed(Emulator)`, unlinks; the follower thread's `returned` channel disconnects → it exits and the frontend rebuilds the peer core as it would after any crash. |
| Save state loaded / replay attached while linked | Refused with "Unplug the link cable first". |
| Fast clock (CGB) transfers | Up to two edges per `GB_run`: each edge calls into the partner immediately (§6.1), so no bit is coalesced. |
| Unplug mid-transfer | The master reads 1s from then on (SameBoy without callbacks); the slave's partial byte stays partial; games time out as on hardware. |

## 14. Testing and verification

**Test ROM.** `supershuckie-core/src/link/test_rom.rs`: a hand-assembled GB ROM built at test time (header with the standard 48-byte logo — SameBoy's CGB boot ROM validates it by checksum, `bootrom/cgb/cgb_boot/cgb_boot.asm:104` — and the header checksum computed) whose RGBDS source sits in the doc comment: after ~10 frames it reads a role byte the test wrote to `$C000` (`1` = master), then exchanges 256 bytes (master sends `i` with SC=`$81`, slave preloads `i ^ $FF` with SC=`$80`, each waits for the serial interrupt), storing what it received at `$C100+i`, then writes `$AA` to `$C001`. ~80 bytes of code; no assembler needed in the build. (GBA phase 2: the same program in Thumb, assembled once with `arm-none-eabi-as`, committed with its source.)

**Unit tests** (`supershuckie-core --lib`, single thread, no network):
- `linked_pair_exchanges_bytes`: two `SuperShuckieCore`s + `LinkSettings { delay: 3 }` run through `run_linked` until both `$C001 == $AA`; master `$C100..` = `i ^ $FF`, slave = `i`.
- `both_perspectives_agree`: run the pair with `local_is_first` = true and again = false; blake3 of both consoles' WRAM equal after every frame.
- `serial_in_replays_each_console_alone`: record both cores' replays during a linked run; play each file back alone (a fresh core, port in Replay mode) → same WRAM at every 60 frames, `serial_replay_misses == 0`; truncated `SerialIn` data is rejected without panic.
- `input_delay_queue`: an input enqueued at frame `n` reaches the console at `n + D`; the pre-fill covers the first `D` frames; unlinking drops the queue and the next input is immediate; scheduled writes/resets land at their frame in the fixed order.
- `shadow_audio_follows_serial`: with audio enabled, the shadow's resync count stays 0 through a transfer.
- `lend_and_return` (`thread.rs`): a follower core lent to a primary keeps answering `read_ram`/`get_elapsed_time` through its own handle; `Close` while lent returns the loop and ends the thread; a dropped primary ends the follower thread cleanly.
- Recorder: `SerialIn` round trip, size limit, `packet_stats` count.

**Crate tests** (`supershuckie-play-together`): v3 codec round trips and hostile lengths for every new message; loopback host + 2 clients: request → accept → both `Started` relayed, `LinkFrame`s reach the sink from the reader thread in order and ahead of a queued snapshot, `Unlink` on leave, `Busy`/`ConsoleMismatch` refusals, `PeerLinked` broadcast; a v2 client is refused by version.

**Headless end-to-end.** `play_together_smoke <rom> --link` (the test ROM or a Pokémon ROM): three frontends, host + client 2 link (the third watches), `--delay-ms` through a delaying `Transport` to force `D ≥ 3` and to inject jitter; assert: link established within 2 s; for the test ROM all four linked cores (two per machine) finish with identical WRAM; pair hash mismatches 0; the third player's followers report `hash_mismatches == 0` and `serial_replay_misses == 0`; the linked players' own fps ≥ 95 % of 60 on loopback and no stall > 100 ms; unlink → all followers resync within 2 s and everyone keeps running; every saved friend replay plays to its end with the same final WRAM as the live core it followed; UI-tick p99 unchanged. Regression: `record_pacing_smoke` and `play_together_smoke` without `--link` print the same numbers as before.

**By hand** (no GUI automation; see the in-app checklist in the memory notes): Red↔Red trade and battle on a LAN; Crystal↔Crystal battle + Mystery Gift (phase 3); over Tailscale at ~40 ms RTT (expect `D` = 3–4); pause one side; pull the cable mid-trade; a third player watching; the Poke-A-Byte overlay reading the linked friend's game; phase 2: Emerald↔Emerald trade and a Union Room visit.

## 15. Order of work

1. **Phase 0 — the cable in a box** (core only): safeboy vendoring + patch; `LinkPort` + GB implementation; `SerialIn` packet (recorder v7) + capture/replay in `SuperShuckieCore`; the test ROM; the unit tests above minus lending. Proves the emulation side before any networking.
2. **Phase 1 — GB/GBC over Play Together**: thread split + lending; `LinkState`/`run_linked`/delay queue/pair hash; protocol v3 + session API; frontend state machine + settings; C API; Qt; docs; smoke `--link`; hand checks. Shippable.
3. **Phase 2 — GBA**: mGBA single-thread lockstep spike first (two cores, one coordinator, asserts on), then `mgba-rs` glue, sliced run, replay SIO driver + GBA `SerialIn`, the Thumb test ROM, smoke on the GBA ROM, hand checks with Gen 3.
4. **Phase 3 (optional)**: infrared (`GB_set_infrared_callback`/`set_infrared_input` through the same event log, `0x03 I` events) for Gen 2 Mystery Gift; 3–4 player GBA; a "verify the partner's stream while linked" pass (compare the stream's packets and sync hashes with what the lockstep applied) so that unlinking needs no snapshot.

## 16. Open questions

- Should a *third* player be able to open a link to someone already linked (GBA 4-player) in phase 2, or is 2-player enough for the first GBA release? (Spec assumes 2.)
- Rollback later: GB states are ~40 KB and `GB_run_frame` runs thousands of frames per second, so a 2–3 frame rollback would remove the input delay entirely for GB. Worth it only if the delay turns out to bother people in battles.
- Whether the plug-in prompt should be skippable ("always accept from the host") beyond the hidden smoke-tool setting.
