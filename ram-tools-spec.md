# RAM Viewer, Search, Watch, Editing and Freezing — Implementation Spec

**Target:** Claude Code, working in the `SnowyMouse/supershuckie` repository.
**Components:**
- `supershuckie-core`: memory regions, sampling, edits and freezes on the core thread.
- `mgba-rs` / `melonds-rs`: accessors for the extra memory regions.
- `supershuckie-memory-tools`: a new pure-logic crate with the search engine, value codecs and watch model.
- `supershuckie-frontend`: ownership, the search worker thread, persistence and recording policy.
- `supershuckie-frontend-c`: the C API.
- `supershuckie-qt`: separate tool windows.

---

## 1. Goal

Five capabilities. All of them work while the game runs, while it's paused, and (read-only) during replay playback:

1. **RAM viewer.** A live hex view with **one-click switching between memory regions** and changed-byte highlighting. You can open several viewer windows at once, each on its own region.
2. **RAM search.** A cheat-search style engine. It starts with an initial scan (known value, range, unknown value, byte pattern or text) and then takes any number of refinements, with undo.
3. **RAM watch.** A per-ROM list of addresses, including pointer chains, shown as typed values. Each watch can log every change frame-accurately and pause the game when a condition is met.
4. **Memory editing.** Type over bytes in the viewer, or edit values in the watch and search windows, with undo and redo.
5. **Memory freezing.** Hold an address at a value. You can freeze from any of the three windows, and every freeze is listed in the watch window.

**Addresses match Poke-A-Byte.** Every address the tools show or accept is one `EmulatorCore::read_ram` / `write_ram` accepts. That's the address space Poke-A-Byte, the REST API and replay `WriteMemory` packets already use. Extra regions are added as a strict superset: every existing address keeps its meaning (§4.1).

**All tools are separate top-level windows** (§5.3).

**The hard requirement: no measurable slowdown.** The reference workload is the one in `nds-performance-report.md`: Nintendo DS at 4× (240 fps). One frame has a **4.17 ms** budget and currently costs **1.9–2.2 ms mean, with p99 ≈ 3.8–4.1 ms**. The design targets these costs:

| State | Added cost on the core thread |
|---|---|
| All tools closed, nothing frozen or traced | **One `Option` check per loop iteration.** Nothing else. |
| Viewer(s) + watch window open | ≤ 16 KiB copied **30× per second of wall-clock time**, regardless of emulation speed. Microseconds per second in total. |
| Traced watches | One compare per traced watch per emulated frame, capped at 64 (< 1 µs/frame). |
| Freezes | One compare per freeze per emulated frame, capped at 256 entries / 4 KiB. A write happens **only on frames where the game actually changed the value**. |
| Edits | A single write at the next frame boundary, at the speed a human types. |
| Search step | **One** full copy of RAM at a frame boundary (NDS main RAM: 4 MiB ≈ 0.1–0.4 ms, *once*; measured in Phase 1). Comparisons run on a separate low-priority worker thread. |

**Replays.** Viewing, searching and watching never write to memory. Edits and freezes go through the existing recorded write path (`SuperShuckieCore::enqueue_write` → `WriteMemory` packet), so a replay recorded with edits or freezes plays back identically. During playback, editing and freezing are disabled (§8).

---

## 2. How RAM and threads work today (read this first)

### 2.1 Threads

- **Qt UI thread.** `MainWindow::tick` runs on a 1 ms `QTimer` and calls `supershuckie_frontend_tick` → `SuperShuckieFrontend::tick` (`supershuckie-frontend/src/lib.rs`).
- **Core thread.** `ThreadedSuperShuckieCoreThread::run_thread` (`supershuckie-core/src/thread.rs`). Each loop iteration:
  - `try_recv` one `ThreadCommand`, handle it, then `continue`.
  - Otherwise run `handle_replay_recording_errors`, `go_to_desired_frame`, `refresh_screen_data`, `update_queued_screens`, `handle_pokeabyte_integration` and `check_if_replay_stalled`.
  - Then call `run_one()`. It sleeps for most of the wait until the next paced frame (capped at 8 ms), then **spins for the last ~1 ms**, so the loop runs many times per emulated frame.
  - While paused, it calls `thread::sleep` for **100 ms** (10 ms with a replay attached).
- The core thread is marked latency-sensitive (`mark_thread_latency_sensitive`). New worker threads must **not** be.

### 2.2 Patterns to copy

- **Non-blocking buffer hand-off.** `refresh_screen_data` / `update_queued_screens` fill a buffer on the core thread, `try_lock` the shared `Mutex<Vec<ScreenData>>`, and `mem::swap`. If the lock is contended, they publish on a later iteration instead. The core thread never blocks on the UI.
- **Coalescing requests.** `go_to_replay_frame` stores into an `AtomicU32` "because we do not want to clog the queue".
- **Frame-boundary gating.** `handle_pokeabyte_integration` doesn't touch RAM while `self.core.mid_frame && is_running`, because the Game Boy core steps in sub-frame slices.

### 2.3 RAM access today

`EmulatorCore::read_ram` / `write_ram` (`supershuckie-core/src/emulator.rs`) use a core-specific address space:

| Console | Mapping today | Backing |
|---|---|---|
| GBC (`game_boy_color.rs`, `pokeabyte_protocol_region_from_address`) | `0x8000–0x9FFF` → VRAM; `0xC000–0xDFFF` → `RAM[0x0000..0x2000]`; `0x10000–0x11FFF` → `RAM[0x2000..0x4000]` (synthetic address); `0xFF80–0xFFFE` → HRAM | `safeboy` `direct_access(DirectAccessRegion::…)` |
| GBA (`game_boy_advance.rs`) | `0x02000000` EWRAM (256 KiB), `0x03000000` IWRAM (32 KiB) | `mgba-rs` `get_ewram` / `get_iwram` |
| NDS (`nintendo_ds.rs`) | `0x02000000` main RAM (4 MiB); any other address is an error | `melonds-rs` `get_main_ram` → `nds->MainRAM` |

Each call is a `dyn` call plus an address `match` and bounds checks. That's fine for occasional reads but the wrong shape for bulk copies (§4.1 adds region slices).

### 2.4 The write path today — and three problems freezing must fix

- `SuperShuckieCore::enqueue_write(address, data)` pushes a `QueuedWrite` and calls `flush_writes()`. `before_run()` also calls `flush_writes()` before every `run`.
- `flush_writes()` returns early **if a replay player is attached** or **if `mid_frame`**. Otherwise it calls `core.write_ram` and `recorder.write_memory(...)` for each queued write.
- `ReplayFileRecorder::write_memory` writes a `WriteMemory` packet **every time**. Unlike `set_speed`, it doesn't deduplicate.
- During playback, `WriteMemory` packets are applied with `self.core.write_ram(...).expect("failed to write RAM (and this was not handled)")`.
- Poke-A-Byte freezes (`thread.rs`, `handle_pokeabyte_integration`) call `enqueue_write` for every frozen address **on every loop iteration** where the game is running and not mid-frame.

Reading the code, this looks like three problems. Confirm each with a test before fixing (Phase 1):

1. **Replay bloat from Poke-A-Byte freezes.** The loop runs many times per frame (§2.1), so while recording, one frozen address can produce many identical `WriteMemory` packets per frame.
2. **Unbounded queue during playback.** `flush_writes` returns early without clearing, so a Poke-A-Byte freeze during playback grows `self.writes` without limit. Every queued write is then applied, and recorded, at whatever moment playback detaches or recording resumes.
3. **Playback panics on unmapped writes.** Once new regions exist (§4.1), a replay that edits them panics on playback in an **older** build. That can't be fixed retroactively. This build must at least not panic.

### 2.5 State discontinuities

`SuperShuckieCore::run_serial()` increments on **every** `run`, including zero-frame pacing runs and Game Boy sub-frame steps. RAM can also change without a frame running, through `load_save_state`, `hard_reset`, `go_to_replay_frame` (seek), attaching or detaching a replay, and core swaps. `SuperShuckieFrontend::switch_core` / `assign_core` build a **new** `ThreadedSuperShuckieCore` on ROM load, reload and unload.

---

## 3. Architecture overview

```
 Qt UI thread                                  Search worker thread         Core thread (latency-sensitive)
 ────────────                                  ────────────────────         ───────────────────────────────
 HexViewerWindow ×1–4 ─┐                                                    run_thread loop:
 RamSearchWindow      ─┼─ MemoryToolsController                               … handle_pokeabyte_integration()
 RamWatchWindow       ─┘   (one 33 ms QTimer)                                 handle_memory_monitor()          ◄── NEW
        │                          │                                            ├ None → return
        ▼                          ▼                                            ├ mid_frame && running → return
 supershuckie_frontend_memory_* (C API)                                         ├ new frame / state jump? ─┐
        │                                                                       │   1 traces (observe game changes)
        ▼                                                                       │   2 pause conditions
 SuperShuckieFrontend                                                           │   3 freezes (write only if differs)
   ├ MemoryToolsState ───── Arc<MemoryMonitorShared> ─────────────────────────  │   4 pending edits
   │   (request, freezes, edit history, recording policy)                       │   5 sample (wall-clock rate-limited)
   ├ RamSearchSession ─── jobs ──► scan engine                                  └ snapshot requested? → copy all regions
   │                     ◄─── FullSnapshot (recycled Vec<u8>) ─────────────────── mpsc
   └ WatchList (per ROM JSON)
```

Principles:

1. **Copy only what a human can see, only as often as they can see it.** Wall-clock rate limiting keeps the cost the same at 4× as at 1×.
2. **The core thread never waits, never allocates in steady state, and never computes.** It uses atomics and `try_lock` only, and buffers are preallocated and swapped. Edits are the one exception: they allocate, but only at human speed.
3. **Everything happens at a frame boundary, in a fixed order.** Traces see the game's own changes before a freeze restores the value. Edits land after freezes. The sample shows the final state that the next frame will start from.
4. **Writes use one path.** Tool edits, tool freezes and Poke-A-Byte freezes all end in `SuperShuckieCore::enqueue_write`, so recording and playback treat them the same.

---

## 4. Core layer

### 4.1 Memory regions: a switchable superset of the Poke-A-Byte address space

Add to `emulator.rs`:

```rust
#[derive(Copy, Clone, Debug)]
pub struct MemoryRegionInfo {
    pub name: &'static str,         // "EWRAM", "Palette", "Main RAM", …
    pub short_name: &'static str,   // "EWRAM", "PAL", "MAIN" — used in "PAL:1F0" address syntax
    pub base_address: u32,          // first address accepted by read_ram for this region
    pub len: u32,
    pub default_big_endian: bool,   // default for search/watch
    pub writable: bool,             // false → tools refuse edits/freezes (UI greys them out)
}

pub trait EmulatorCore {
    /// Memory regions, in the order the UI lists them. Default: none.
    fn memory_regions(&self) -> &[MemoryRegionInfo] { &[] }
    /// Direct read-only view of region `index`. Must not copy. Default: None.
    fn memory_region_data(&self, _index: usize) -> Option<&[u8]> { None }
}
```

For GBA and NDS, **rewrite `read_ram` / `write_ram` to be table-driven from `memory_regions()`**, and extend GBC's `pokeabyte_protocol_region_from_address`. That way the tools, Poke-A-Byte, the REST API and replays share one mapping and can't drift apart. Existing addresses must map to exactly the bytes they map to today; this is enforced by a test (§10.2). A region's `len` comes from the core at runtime where the size varies (cart RAM, save data). Regions of size 0 are omitted.

**GBC** (big-endian default):

| Region | Addresses | Backing | Writable | Status |
|---|---|---|---|---|
| VRAM | `0x8000–0x9FFF` | `DirectAccessRegion::VRAM` | yes | existing |
| WRAM | `0xC000–0xDFFF` | `RAM[0x0000..0x2000]` | yes | existing |
| WRAM (upper) | `0x10000–` `0x10000 + ram_size − 0x2000 − 1` | `RAM[0x2000..ram_size]`, the existing formula with its range extended from 8 KiB to the full RAM | yes | existing formula; the range past `0x11FFF` is new |
| OAM | `0xFE00–0xFE9F` | `DirectAccessRegion::OAM` | yes | new (real hardware address) |
| I/O registers | `0xFF00–0xFF7F` | `DirectAccessRegion::IO` | **no** (raw writes would bypass hardware side effects) | new (real hardware address) |
| HRAM | `0xFF80–0xFFFE` | `DirectAccessRegion::HRAM` | yes | existing |
| Cart RAM | `0x20000–` `0x20000 + size − 1` | `DirectAccessRegion::CART_RAM` (the whole save RAM, all banks) | yes | new (synthetic; the real `0xA000` window only shows the current bank) |

Before writing code:
- Confirm `safeboy` exposes `OAM`, `IO` and `CART_RAM`.
- Confirm the `RAM` slice size in CGB mode (expected 32 KiB, which makes WRAM (upper) `0x10000–0x15FFF`) and in DMG mode (8 KiB, so WRAM (upper) is omitted).
- Confirm the upper range can't collide with anything: `0x16000–0x1FFFF` stays unmapped and cart RAM starts at `0x20000`.

**GBA** (little-endian default). Addresses are the real bus addresses:

| Region | Base | Size | Writable | Backing / write path | Status |
|---|---|---|---|---|---|
| EWRAM | `0x02000000` | 256 KiB | yes | `get_ewram` | existing |
| IWRAM | `0x03000000` | 32 KiB | yes | `get_iwram` | existing |
| Palette | `0x05000000` | 1 KiB | yes | new FFI accessor. **Writes go through mGBA's patch path** (e.g. `GBAPatch8` / `core->rawWrite8`) so the renderer's palette cache updates. | new |
| VRAM | `0x06000000` | 96 KiB | yes | as Palette | new |
| OAM | `0x07000000` | 1 KiB | yes | as Palette (OAM is cached by the renderer) | new |
| Save data | `0x0E000000` | from mGBA savedata (SRAM/Flash); omitted for EEPROM/none | yes | new FFI accessor into `savedata.data` | new |

In `mgba-rs/interface.cpp`, add `mgba_rs_core_get_region(core, id, &size) -> *mut u8` and `mgba_rs_core_patch_write(core, address, data, len)`. Check the exact mGBA API names against the vendored `mgba-rs/mgba` checkout. I/O registers are left out because reading some of them has side effects in mGBA's bus path.

**NDS** (little-endian default). ARM9-view addresses:

| Region | Base | Size | Writable | Backing | Status |
|---|---|---|---|---|---|
| Main RAM | `0x02000000` | 4 MiB | yes | `nds->MainRAM` | existing |
| Shared WRAM | `0x03000000` | 32 KiB | yes | `nds->SharedWRAM` (raw; which CPU sees it depends on WRAMCNT, noted in the tooltip) | new |
| ARM7 WRAM | `0x03800000` | 64 KiB | yes | `nds->ARM7WRAM` | new |

The new addresses can't collide with existing ones: today `0x03000000` computes an offset of `0x1000000`, which is past the 4 MiB bounds, so it errors. When **JIT is enabled**, raw writes into memory the game executes aren't seen by already-compiled blocks. Call melonDS's JIT invalidation for the written range in `melonds_rs_core_write` (check the API name in the vendored melonDS). Replays already require JIT off, so this only affects live play.

**Null:** no regions.

Also:
- Add a `debug_assert!` and a `core_smoke` test that `read_ram(region.base + i, …)` equals `memory_region_data(index)[i]` for sampled offsets in every region.
- Because Poke-A-Byte uses the same `read_ram`/`write_ram`, it can see the new regions too. That's harmless: previously those addresses returned errors.

### 4.2 State epoch

Add `state_epoch: u64` to `SuperShuckieCore`, with a `pub fn state_epoch(&self) -> u64` getter. Increment it in `load_save_state`, `hard_reset`, `go_to_replay_frame_inner`, `attach_replay_player`, `detach_replay_player`, `resume_recording_replay`, and anywhere else memory is replaced wholesale. It costs nothing on the frame path.

### 4.3 Write-path fixes (from §2.4)

1. `enqueue_write` **drops the write** and returns `false` while a replay player is attached. The drop is explicit and documented. `self.writes` never grows during playback.
2. Freezes are applied by one shared helper, `apply_freezes(&mut SuperShuckieCore, freezes)`. It runs **once per emulated frame** (plus once after a state jump or a freeze change) and calls `enqueue_write` **only when the current bytes differ**. Poke-A-Byte's `freezes: BTreeMap<u64, ByteVec>` moves to this helper too, so both kinds of freeze behave the same. A recording then gets one `WriteMemory` per address only on frames where the game changed the value.
3. In the playback path, replace the `write_ram(...).expect(...)` with a non-panicking error. Log it, count it, and skip the write.

Write a test that shows problems 1 and 2 first, then fix them.

### 4.4 `memory_monitor.rs`: shared state

New module `supershuckie-core/src/memory_monitor.rs`, behind the `std` feature:

```rust
pub struct MemoryMonitorShared {
    // UI → core
    request: Mutex<MonitorRequest>,     // core only try_locks, and only when the generation changed
    request_generation: AtomicU64,
    snapshot_requested: AtomicU64,      // job id; core swaps to 0 when taken
    // core → UI
    sample: Mutex<MonitorSample>,       // try_lock + mem::swap (screens pattern)
    sample_generation: AtomicU64,
    events: Mutex<EventRing>,           // fixed capacity; oldest dropped, `dropped` counter
    snapshot_out: Mutex<Option<Sender<FullSnapshot>>>,
    snapshot_pool: Mutex<Vec<Vec<u8>>>, // buffers returned by the search worker
}

pub struct MonitorRequest {
    pub enabled: bool,                  // display sampling on/off (traces/freezes run regardless)
    pub interval: Duration,             // default 33 ms (15/30/60 Hz setting)
    pub windows: ArrayVec<ViewWindow, 4>, // one per open viewer window; total ≤ 16 KiB
    pub probes: Vec<Probe>,             // visible watch rows + visible search rows; cap 512
    pub traces: Vec<TraceSpec>,         // cap 64
    pub freezes: Vec<FreezeSpec>,       // cap 256 entries, ≤ 4 KiB of frozen bytes total
    pub freezes_suspended: bool,        // set by frontend while a replay is attached
}
pub struct ViewWindow { pub viewer_id: u8, pub address: u32, pub len: u32 }
pub struct AddressPath { pub base: u32, pub derefs: ArrayVec<i32, 4> } // addr = read_u32_le(addr) + off, per deref
pub struct Probe { pub id: u32, pub path: AddressPath, pub len: u8 }
pub struct TraceSpec { pub watch_id: u32, pub path: AddressPath, pub len: u8, pub pause_when: Option<RawCondition> }
pub struct FreezeSpec { pub id: u32, pub path: AddressPath, pub bytes: ArrayVec<u8, 64> }

pub struct MonitorSample {
    pub frame: u64, pub state_epoch: u64,
    pub windows: ArrayVec<(ViewWindow, Vec<u8>, u32 /*valid len*/), 4>,
    pub probe_values: Vec<u8>, pub probe_ok: Vec<bool>, pub resolved_addresses: Vec<u32>,
    pub freeze_restores: Vec<u32>,      // per freeze: cumulative count of frames the value had to be restored
}

pub enum MonitorEvent {
    Changed      { frame: u64, watch_id: u32, old: u64, new: u64 },
    Discontinuity{ frame: u64 },        // seek/load/reset: baselines reset, not a change
    PausedByCondition { frame: u64, watch_id: u32 },
    Written      { frame: u64, edit_id: u64, address: u32, old: Vec<u8>, new: Vec<u8> }, // for undo
    WriteFailed  { edit_id: u64, reason: WriteFailure }, // Unmapped, ReadOnlyRegion, PointerInvalid, Playback
}

pub struct FullSnapshot { pub job_id: u64, pub frame: u64, pub state_epoch: u64,
                          pub regions: Vec<(MemoryRegionInfo, Range<usize>)>, pub bytes: Vec<u8> }
```

### 4.5 The core-thread hook

`ThreadedSuperShuckieCoreThread` gains `memory_monitor: Option<MemoryMonitorLocal>`. The local half holds:
- the `Arc` and a private copy of the request (updated with `clone_from`, which reuses capacity);
- a private sample buffer, trace baselines and per-freeze restore counters;
- `pending_edits: VecDeque<PendingEdit>`, `last_frame`, `last_epoch`, `next_sample_at` and `seen_request_generation`.

Call it right after `handle_pokeabyte_integration()` in `run_thread`, and at the end of `go_to_desired_frame()` when a seek happened.

```rust
fn handle_memory_monitor(&mut self) {
    let Some(mon) = self.memory_monitor.as_mut() else { return };      // detached: this is the whole cost
    let running = self.is_running();
    if self.core.mid_frame && running { return }

    let frame = self.core.total_frames;
    let epoch = self.core.state_epoch();
    let jumped = epoch != mon.last_epoch;
    let new_frame = frame != mon.last_frame || jumped;
    let request_changed = mon.pull_request_if_changed();               // one atomic load; try_lock only if changed
    let playback = self.core.replay_player.is_some();

    if new_frame {
        mon.run_traces(self.core.get_core(), frame, jumped);           // 1. observe the game's own changes
        if mon.take_pause_trigger() { self.pause_from_core_thread() } // 2. same effect as ThreadCommand::Pause
    }
    if (new_frame || request_changed) && !playback && !mon.req.freezes_suspended {
        mon.apply_freezes(&mut self.core);                             // 3. enqueue_write only if bytes differ
    }
    if !mon.pending_edits.is_empty() {
        mon.apply_edits(&mut self.core, frame, playback);              // 4. resolve paths now; Written / WriteFailed events
    }
    if mon.req.enabled && ((new_frame && Instant::now() >= mon.next_sample_at) || request_changed
                           || mon.edits_applied || (new_frame && !running)) {
        mon.fill_sample(self.core.get_core(), frame, epoch);           // 5. preallocated copies
        mon.try_publish();                                             // contended → re-offer next iteration, no re-copy
        mon.next_sample_at = Instant::now() + mon.req.interval;
    }
    let job = mon.shared.snapshot_requested.swap(0, Relaxed);
    if job != 0 { mon.send_full_snapshot(self.core.get_core(), job, frame, epoch); }

    mon.last_frame = frame; mon.last_epoch = epoch;
}
```

Performance notes:
- `Instant::now()` only runs on new frames (≤ 240×/s).
- Address resolution uses a binary search over `memory_regions()` (≤ 7 entries) and then `copy_from_slice` from `memory_region_data`. Nothing is read byte-by-byte through `dyn read_ram`.
- `apply_freezes` compares with `memory_region_data` slices. The write itself goes through `enqueue_write` → `write_ram` so side-effect regions (GBA palette/VRAM/OAM) use the patch path and the write is recorded. Because `mid_frame` is false here, `flush_writes` applies it immediately, before the next frame runs.
- Traces reset their baselines when the state epoch changes and emit a single `Discontinuity`. Because traces run before freezes, the log shows "game changed X → Y" and the freeze then restores the value silently. The restore is counted in `freeze_restores`, not logged as an event.
- `apply_edits` rejects edits during playback, to read-only regions, to unmapped addresses and through invalid pointers. For each accepted edit it copies the old bytes (for undo), calls `enqueue_write`, and emits `Written`. Edits allocate, which is fine at typing speed.
- Events are pushed into a local fixed-capacity buffer and flushed to the shared ring with `try_lock` at most once per display interval (edits flush immediately).
- `send_full_snapshot` reuses a pooled buffer, does one `extend_from_slice` per region, and sends it through mpsc.
- Pointer paths read 4-byte little-endian pointers (GBA/NDS only). The UI disables pointer paths on GBC.

### 4.6 Commands and waking a paused thread

- `ThreadCommand::SetMemoryMonitor(Option<Arc<MemoryMonitorShared>>)`, exposed as `ThreadedSuperShuckieCore::set_memory_monitor`.
- `ThreadCommand::MemoryEdit { edit_id: u64, path: AddressPath, data: ByteVec }`. The handler only pushes onto `pending_edits`, and `handle_memory_monitor` applies it at the next boundary so pointer paths resolve against a consistent frame. Limit: 4 KiB per edit (larger pastes are rejected in the UI).
- **Paused responsiveness.** Keep the spawned thread's `Thread` handle. In `run_thread`, replace the two paused `std::thread::sleep(…)` calls with `std::thread::park_timeout(…)`, and add `ThreadedSuperShuckieCore::wake()` → `unpark()`. The frontend calls `wake()` after changing the request, queuing an edit or requesting a snapshot. The running path (`run_one`) is unchanged.
- Video export holds the core thread inside `ExportVideo`, so the monitor isn't serviced during an export and edits are rejected in the UI.

### 4.7 Benchmark support (required before any UI work)

Extend `supershuckie-core/examples/nds_bench.rs` with:

```
--mm-window <bytes>  --mm-hz <n>  --mm-probes <n>  --mm-traces <n>
--mm-freezes <n>            n 4-byte freezes on addresses the game writes every frame (worst case: restore every frame)
--mm-snapshot-every <n>     stress only
```

Factor the body of `handle_memory_monitor` into a method on `MemoryMonitorLocal` that takes `&mut SuperShuckieCore`, so the bench can drive it without the thread. Report mean, p99, over-budget % and the cost of a single snapshot. Gates are in §10.1.

---

## 5. RAM viewer

### 5.1 Frontend

`SuperShuckieFrontend` gains `memory: MemoryToolsState`, which owns:
- `shared: Arc<MemoryMonitorShared>`, created lazily;
- the per-viewer windows, watch probes, search probes, traces and freezes, all merged into one `MonitorRequest` by `rebuild_request()` (lock → edit → bump generation → `core.wake()`);
- the edit history (§8.3) and the recording policy state (§8.4).

Core swaps: in `assign_core`, call `new_core.set_memory_monitor(Some(shared.clone()))` if anything is active, and refresh the cached region list.
- If the **console type or `rom_checksum()` changed**, invalidate the search session, clear the edit history, deactivate all freezes, and load the new ROM's watch list.
- For the same ROM (*Reload core*), keep everything, including active freezes.

Detaching: call `set_memory_monitor(None)` when no tool window is visible **and** there are no traces and no active freezes. The core thread is then back to a single `Option` check.

Freeze suspension: set `freezes_suspended = true` while a replay is attached for playback, and clear it on detach or when recording resumes.

### 5.2 C API (viewer)

Follow the existing out-parameter style (e.g. `supershuckie_frontend_get_frame_time_stats`):

```c
struct SuperShuckieMemoryRegion { const char *name; const char *short_name; uint32_t base_address; uint32_t length;
                                  bool default_big_endian; bool writable; };

size_t supershuckie_frontend_memory_get_regions(const struct SuperShuckieFrontendRaw *, struct SuperShuckieMemoryRegion *out, size_t capacity);
uint64_t supershuckie_frontend_memory_regions_generation(const struct SuperShuckieFrontendRaw *); /* bumps on core swap */
bool   supershuckie_frontend_memory_parse_address(const struct SuperShuckieFrontendRaw *, const char *text, uint32_t *address); /* "0x2024284", "EWRAM:24284", "PAL+1F0" */
void   supershuckie_frontend_memory_set_viewer_window(struct SuperShuckieFrontendRaw *, uint8_t viewer_id, bool enabled, uint32_t address, uint32_t length, uint8_t hz);
bool   supershuckie_frontend_memory_read_viewer(struct SuperShuckieFrontendRaw *, uint8_t viewer_id, uint64_t *generation, uint64_t *frame,
                                                uint32_t *address, uint8_t *bytes, uint32_t capacity, uint32_t *valid_length);
```

### 5.3 Qt: separate windows

New files: `memory_tools_controller.{hpp,cpp}`, `hex_viewer_window.{hpp,cpp}`, `hex_view_widget.{hpp,cpp}`, `ram_search_window.{hpp,cpp}`, `ram_watch_window.{hpp,cpp}`, `watch_edit_dialog.{hpp,cpp}`.

- **Tools menu.** A new top-level menu in `MainWindow::set_up_menu`, after *Replays*:
  - *RAM viewer* (opens or raises the first viewer)
  - *New RAM viewer window* (up to 4)
  - *RAM search*
  - *RAM watch*
  - separator, then *Unfreeze all* (enabled when anything is frozen)
- **Window type.** Every tool is a non-modal **top-level** `QWidget` window (`Qt::Window`, parented to `MainWindow` so it closes with the app). There are no docks and no tabs between tools. Open windows and their geometry are remembered in the frontend settings (`settings.rs`, new `memory_tools` section) and restored on the next launch.
- **`MemoryToolsController`** (owned by `MainWindow`) runs **one** `QTimer` (default 33 ms) shared by every tool window. It isn't part of the 1 ms `MainWindow::tick`. It runs only while a tool window is visible, or while traces or freezes are active (to drain events and update the status bar). Windows stop requesting data in `hideEvent`/minimize and start again in `showEvent`.
- **Status bar.** When anything is frozen, `MainWindow` shows a "**N frozen**" label. Clicking it opens RAM watch. When the current recording contains tool writes, it shows "**RAM modified**" (§8.4).

### 5.4 Region switching

Region switching is a primary control in every tool:

- **Viewer.** A **tab bar across the top** with one tab per region, using its short name (GBC: 7, GBA: 6, NDS: 3). Tooltips show the full name, address range, size and "read-only" where it applies. Tabs rebuild whenever `regions_generation` changes.
  - Shortcuts: **Ctrl+1…Ctrl+9** jump to a region, and **Ctrl+PgUp / Ctrl+PgDn** cycle through them.
  - Each tab remembers its own scroll position, cursor and selection within the window.
  - **Go to address** (Ctrl+G) accepts `0x…`, bare hex, `SHORT:offset` or `SHORT+offset`, and switches to the region that contains the address.
  - **Back / Forward** (Alt+Left / Alt+Right) walk a navigation history that works across regions.
  - The window title is `RAM viewer — EWRAM (0x02000000)`, so several viewer windows on different regions are easy to tell apart.
- **Search.** A region checklist, with *All* and *All writable* presets, plus an optional custom address range.
- **Watch.** The address editor has a region dropdown plus an offset field (or free-text address). A Region column shows each watch's short name.
- **Cross-links.** *Show in RAM viewer* from search and watch switches the most recently focused viewer to the right region and address. It opens a viewer if none is open.

### 5.5 `HexViewWidget`

- A `QAbstractScrollArea` subclass that paints **only visible rows**: an address column, then N byte columns (8/16/32 per row, default 16), then a character column (ASCII or a loaded `.tbl` table, §6.2).
- Painting is one `drawText` per row for the hex text and one for the characters (monospace, cached advance width), plus rectangles for highlights. The scroll range covers the current region (NDS main RAM: 262,144 rows at 16/row).
- When scrolling or resizing changes the visible range, it calls `set_viewer_window` for that range.
- **Highlights:**
  - Changed bytes fade over ~1 s, tracked with a per-visible-byte `heat` array.
  - **Frozen bytes** get a distinct background, from the frontend's frozen-address set.
  - Selection.
  - The byte being edited.
- If a sample is byte-identical to the previous one and nothing is fading, it doesn't call `update()`, so a paused game costs nothing to redraw.
- Unmapped addresses are drawn as `--`. The header shows `Frame N`.
- **Data inspector** panel: u8/i8, u16/i16/u32/i32 in LE and BE, f32, 2/4/6/8-digit BCD, and a pointer (clickable if it lands in a region). Each typed row is **editable**: type a value and press Enter to write it with that encoding.
- **Context menu:**
  - *Add watch…*
  - *Freeze selection*, *Unfreeze selection*
  - *Search for this value*
  - *Paste hex over selection*, *Fill selection…*
  - *Copy address*, *Copy bytes*
- **Editing** is covered in §8.2.

---

## 6. RAM search

### 6.1 Placement

A new workspace crate, **`supershuckie-memory-tools`**, with no emulator, Qt or thread dependencies. It holds value codecs, the scan engine, character tables and the watch model, so all of it can be unit-tested on synthetic buffers.

`supershuckie-frontend` owns `RamSearchSession` and one `RamSearchWorker` thread at normal priority (below-normal on Windows). That thread is **never** marked latency-sensitive.

### 6.2 Value model

```rust
pub enum ValueType { U8, I8, U16, I16, U32, I32, F32, Bcd { digits: u8 }, Bytes { len: u8 }, Text { len: u8, table: TableId } }
pub enum Endian { Little, Big }
pub struct ScanFormat { pub ty: ValueType, pub endian: Endian, pub alignment: u8 /* 1, 2, 4 */ }
```

- Endianness defaults to the region's `default_big_endian`. Alignment defaults to 1 on GBC and to the value size on GBA/NDS.
- Character tables: built-in ASCII, plus standard `.tbl` files (`XX=c`, multi-byte keys allowed) loaded from `<user dir>/tables/`. The same tables drive the viewer's character column, text editing and text watches.
- Byte patterns use `12 ?? 34 5?` syntax, stored per byte as `(mask, value)`.
- A value can't straddle a region boundary.
- The same codecs **encode** values for editing and freezing (§8). Encoding fails with a message for out-of-range values, invalid BCD, or text containing a character the table doesn't have.

### 6.3 Scans

**Initial scan** (over the selected regions or a custom range): exact value · range `lo..=hi` · unknown initial value · byte pattern / text.

**Refinement** (`x` = now, `p` = previous scan, `f` = first scan):

| vs. a value | vs. previous scan | vs. first scan |
|---|---|---|
| `==`, `!=`, `<`, `<=`, `>`, `>=`, between, in set | changed, unchanged, increased, decreased, increased by n, decreased by n, changed by ±n, changed by ≥ n | `== f`, `!= f`, increased/decreased since first |

F32 equality uses an epsilon (default 0.01). Invalid BCD never matches.

**One scan step:**
1. The UI calls `search_scan(params)` and gets a `job_id`.
2. The frontend stores the id in `snapshot_requested` and calls `core.wake()`.
3. At the next frame boundary, the core thread sends a `FullSnapshot`.
4. The worker checks the session (§6.6) and compares in 64 KiB chunks, reporting progress in per-mille and checking a cancel flag.
5. The worker publishes the new candidate set, pushes the old one onto the history stack, and returns buffers it no longer needs to the pool.

A *"Pause while scanning"* checkbox is offered, default **off**.

### 6.4 Candidate storage

- **Dense:** a bitset over aligned slots per region, plus the previous snapshot. Comparisons use `chunks_exact` over 64-bit words. For changed/unchanged, the old and new words are XORed and whole words are skipped when they're zero and have no candidate bits. Loops are written so they auto-vectorize. The target is single-digit milliseconds for 4 MiB, verified with a test in the crate.
- **Sparse:** a `Vec<u32>` of addresses plus packed previous values. It switches to sparse once `count × (4 + width) < bitset + snapshot` bytes, and never switches back.
- **First-scan values** are kept for the "vs. first scan" comparisons (candidates only once sparse).
- **History:** undo/redo, capped at **16 steps or 128 MiB**; the oldest steps are dropped. *Reset* returns all buffers to the pool.

### 6.5 Results window

- The worker exposes `result_count()` and `results(offset, count)`. For a dense bitset, a rank index (cumulative popcount per 4096-bit block) is built after each scan so any page loads immediately.
- `RamSearchWindow` (separate window):
  - A `QTableView` over a lazily fetched model (256 rows per page).
  - Columns: Address · Region · **Current** (live, editable) · Previous · First · Δ · Frozen (checkbox).
  - Only **visible rows** become probes (≤ ~50). With more than 100,000 results, *Current* stays blank until you scroll to a row.
- Controls: type/endianness/alignment, region checklist, comparison + value fields, **New search**, **Scan**, **Undo**, **Redo**, **Reset**, **Cancel**. Status line: "*N candidates · frame F · 3.1 ms*".
- Row actions (multi-select supported):
  - *Add to watch list*
  - *Set value…*: writes the same encoded value to every selected address, max 256 addresses in one action
  - *Freeze* / *Unfreeze*: freezing creates watches in group "Frozen from search"; the 256-freeze cap applies
  - *Show in RAM viewer*
  - *Copy address*

### 6.6 Session validity

A session records the console type, `rom_checksum`, region layout and the epoch of its last snapshot.

- **Same ROM, different epoch** (a state was loaded or the replay was seeked): the session continues, and the status line notes it.
- **Different ROM or console:** the session is invalidated and the window shows "Game changed — start a new search".
- **Reload core** (same ROM): the session continues.

---

## 7. RAM watch

### 7.1 Model (`supershuckie-memory-tools::watch`)

```rust
pub struct Watch {
    pub id: u32, pub label: String,
    pub address: AddressPath,               // ≤ 4 derefs, GBA/NDS only
    pub format: ScanFormat, pub display: DisplayBase,  // Hex, Unsigned, Signed, Binary, Float, Text
    pub group: Option<String>, pub notes: String,
    pub trace: bool,
    pub pause_when: Option<WatchCondition>, // implies trace
    pub freeze: Option<FreezeState>,        // §8.1
}
pub struct FreezeState { pub value: Vec<u8>, pub active: bool }
pub enum WatchCondition { Changes, Equals(i64), NotEquals(i64), GreaterThan(i64), LessThan(i64), IncreasedBy(i64), DecreasedBy(i64) }
```

### 7.2 Sampling modes

| Mode | When | Where | Cost |
|---|---|---|---|
| Display | Row visible in the watch window | A probe at the UI rate | Nothing when the window is hidden |
| Trace (`trace` or `pause_when`) | Always, until turned off | A `TraceSpec` checked every emulated frame | ≤ 8-byte compare; cap 64 |
| Freeze (`freeze.active`) | Always, until unfrozen | A `FreezeSpec` checked every emulated frame | Compare, and a write only when the value differs; cap 256 |

- `pause_when` pauses at the frame boundary where the change first became visible, using the same mechanism as `ThreadCommand::Pause`.
- Values longer than 8 bytes are traced by FNV-1a hash.

### 7.3 `RamWatchWindow` (separate window)

- A `QTreeView` of groups and watches. Columns:
  - Label
  - Region
  - Address (pointer paths show the resolved address, e.g. `[0x02101D2C]+0x0C → 0x0227A3F4`)
  - **Value** (double-click to edit, §8.2)
  - Previous
  - Changed (frames ago)
  - **Frozen** (checkbox; shows the restore count, e.g. "restored ×1,203")
- Only visible rows become probes.
- **Toolbar:** Add · Edit · Delete · Duplicate · Freeze · Unfreeze · *Unfreeze all* · Import · Export.
- **Add/edit dialog:** region dropdown + offset (or free-text address), optional pointer path, type/endianness/display, label, group, trace, pause condition, and *Freeze at value* with the current value pre-filled.
- **Change log** (bottom splitter):
  - Drains events into a capped 10,000-row model: `frame · label · old → new`.
  - `Discontinuity` rows appear as separators.
  - `Written` events appear as "edited by user" rows.
  - Shows "N events dropped" when the ring overflowed.
  - Actions: Clear, Copy, Export CSV.

### 7.4 Persistence

- Saved per ROM as `<userdir for rom>/ram-watch.json` (`serde_json` is already a workspace dependency), with `"version": 1`, console type and ROM checksum. A checksum mismatch warns but still loads, since ROM hacks share layouts.
- Saves are debounced by 1 s after each edit and also happen in `before_unload_or_reload_rom`. The list auto-loads on ROM load.
- **Freezes keep their value but load inactive.** Loading a game never starts writing to its memory by itself, which would also be recorded into a new replay. One click re-activates them.

### 7.5 C API (search + watch)

```c
/* search */
uint64_t supershuckie_frontend_search_new(struct SuperShuckieFrontendRaw *, const struct SuperShuckieSearchParams *);
uint64_t supershuckie_frontend_search_refine(struct SuperShuckieFrontendRaw *, const struct SuperShuckieSearchParams *);
bool     supershuckie_frontend_search_poll(const struct SuperShuckieFrontendRaw *, uint32_t *progress_per_mille, uint64_t *result_count, uint64_t *snapshot_frame, uint32_t *status);
size_t   supershuckie_frontend_search_results(const struct SuperShuckieFrontendRaw *, uint64_t offset, struct SuperShuckieSearchRow *out, size_t capacity);
void     supershuckie_frontend_search_set_visible_rows(struct SuperShuckieFrontendRaw *, uint64_t offset, uint32_t count);
bool     supershuckie_frontend_search_read_visible(struct SuperShuckieFrontendRaw *, uint64_t *generation, uint64_t *values, bool *ok, size_t capacity);
bool     supershuckie_frontend_search_undo(struct SuperShuckieFrontendRaw *);
bool     supershuckie_frontend_search_redo(struct SuperShuckieFrontendRaw *);
void     supershuckie_frontend_search_cancel(struct SuperShuckieFrontendRaw *);
void     supershuckie_frontend_search_reset(struct SuperShuckieFrontendRaw *);

/* watch — passed as JSON to avoid a wide struct ABI */
uint32_t supershuckie_frontend_watch_upsert_json(struct SuperShuckieFrontendRaw *, const char *watch_json);
void     supershuckie_frontend_watch_remove(struct SuperShuckieFrontendRaw *, uint32_t id);
char    *supershuckie_frontend_watch_list_json(const struct SuperShuckieFrontendRaw *);
void     supershuckie_frontend_watch_set_visible(struct SuperShuckieFrontendRaw *, const uint32_t *ids, size_t count);
size_t   supershuckie_frontend_watch_read_values(struct SuperShuckieFrontendRaw *, uint64_t *generation, struct SuperShuckieWatchValue *out, size_t capacity);
size_t   supershuckie_frontend_watch_drain_events(struct SuperShuckieFrontendRaw *, struct SuperShuckieWatchEvent *out, size_t capacity, uint64_t *dropped);
bool     supershuckie_frontend_watch_import(struct SuperShuckieFrontendRaw *, const char *path);
bool     supershuckie_frontend_watch_export(const struct SuperShuckieFrontendRaw *, const char *path);
```

The editing and freezing C API is in §8.5. Value formatting and parsing happen in Rust. `SuperShuckieWatchValue` carries `char text[64]`, so the C++ side doesn't duplicate the BCD, table or float logic.

---

## 8. Editing and freezing

### 8.1 Semantics

- **Edit:** a one-time write. It's applied at the **next frame boundary**, after freezes, so it's visible to the next frame the game runs.
- **Freeze:** the address is held at a value. After every emulated frame, and immediately when a freeze is created or changed (even while paused), the current bytes are compared with the frozen bytes and rewritten only if they differ. This is a **frame-boundary lock**, the same model as GameShark-style per-frame RAM codes. Inside a frame the game can briefly see its own new value before the freeze restores it. A true lock would need a hook on every emulated memory write, which costs performance even when unused; see §11.4.
- **Freezing a value that's being edited:** editing a frozen address (in any tool) **updates the freeze's value** instead of doing a one-time write. Otherwise the freeze would undo the edit on the next frame.
- **Poke-A-Byte freezes** are applied first and RAM-tool freezes second, so a tool freeze wins on conflict. Both use the same once-per-frame, write-only-if-different helper (§4.3).
- **Limits:**
  - Edits: 4 KiB per operation.
  - Freezes: 256 entries and 4 KiB total; entries over 64 bytes are split.
  - Multi-address *Set value*: 256 addresses.
  - Read-only regions (GBC I/O) and unmapped addresses are rejected with a message.

### 8.2 Editing UI

- **Hex viewer:**
  - *Edit mode* toggle (Insert key; the tab shows a pencil marker). Typing hex digits in the hex pane overwrites nibbles, and each completed byte is committed. Typing in the character pane encodes through the active table.
  - Arrow keys and Tab move the cursor; Esc cancels a half-typed byte.
  - *Paste hex over selection*, *Fill selection…* (a repeating byte pattern), and the typed rows in the data inspector.
  - Bytes that are pending (sent but not yet in a sample) are drawn outlined until the next sample confirms them.
  - Read-only regions have editing disabled.
- **Watch window:** double-click Value to edit it in the watch's own format, validated by the codec before sending. The Frozen checkbox toggles the freeze, and turning it on captures the current value.
- **Search window:** edit the *Current* cell, or use *Set value…* / *Freeze* on a selection.

### 8.3 Undo / redo

- One global memory edit history in `MemoryToolsState`, shared by all tool windows. **Ctrl+Z / Ctrl+Shift+Z** work in any tool window.
- Each entry stores the **exact old bytes from the core's `Written` event**, not the UI's slightly older sample.
- Undo writes the old bytes back as a new write, which is recorded too. Undo isn't offered for writes made before the last state epoch change: after a load or reset, "old bytes" are meaningless.
- Freeze and unfreeze actions are on the same stack.
- History is capped at 1,000 entries and cleared on ROM change.

### 8.4 Replays and recording policy

| Situation | Behavior |
|---|---|
| **Recording** | Edits and freeze restores go through `enqueue_write` and are recorded as `WriteMemory` packets. That's required for deterministic playback. Thanks to §4.3, a freeze adds a packet only on frames where the game changed the value. |
| First tool write in a recording | Before sending, the frontend shows a confirmation: "This change will be recorded into the replay." Options: *Continue* / *Cancel*, plus *Don't ask again*. The setting is `memory_tools.confirm_writes_while_recording`, default on. |
| Starting or resuming a recording **with active freezes** | A dialog asks: *Keep freezes (they will be recorded)* / *Unfreeze all and record* / *Cancel*. |
| During a recording that contains tool writes | The status bar shows "**RAM modified**", with the tool-write count for this recording in the tooltip. |
| **Replay playback** | Editing is disabled everywhere (greyed out, tooltip "Stop playback or resume recording to edit"). Active freezes are **suspended** (`freezes_suspended`), not deleted, and shown as "suspended". The core also rejects edits with `WriteFailed::Playback`. `WriteMemory` packets in the replay are applied as usual. |
| Playback detached / *Resume recording from here* | Freezes are un-suspended. The "starting a recording with active freezes" dialog applies. |
| Video export | Editing is disabled; the monitor is idle. |
| Save states | Freezes are tool state, not emulator state, so they aren't saved into or restored from save states. After a state load, active freezes reapply on the next boundary (the epoch changed). |

**Compatibility note (document in the README):** a replay that writes to one of the **new** regions (§4.1) will panic when played in a Super Shuckie build older than this feature, because of the old `expect`. This build replaces that `expect` (§4.3). Writes to existing regions (GBA EWRAM/IWRAM, NDS main RAM, and GBC addresses already mapped) stay fully compatible.

### 8.5 C API (editing + freezing)

```c
/* Returns an edit id (0 = rejected immediately; error text written). Pointer path optional. */
uint64_t supershuckie_frontend_memory_write(struct SuperShuckieFrontendRaw *, uint32_t address, const int32_t *derefs, size_t deref_count,
                                            const uint8_t *data, size_t length, char *error, size_t error_capacity);
/* Encode a typed value via the Rust codecs, then write. */
uint64_t supershuckie_frontend_memory_write_value(struct SuperShuckieFrontendRaw *, uint32_t address, uint32_t value_type, uint8_t size_or_digits,
                                                  bool big_endian, const char *text_value, char *error, size_t error_capacity);
bool     supershuckie_frontend_memory_can_write(const struct SuperShuckieFrontendRaw *, uint32_t *reason); /* false during playback/export/no game */
bool     supershuckie_frontend_memory_needs_record_confirmation(const struct SuperShuckieFrontendRaw *);
void     supershuckie_frontend_memory_confirm_record_writes(struct SuperShuckieFrontendRaw *, bool dont_ask_again);

bool     supershuckie_frontend_memory_freeze(struct SuperShuckieFrontendRaw *, uint32_t address, const uint8_t *value, size_t length, uint32_t *watch_id); /* value NULL = current */
void     supershuckie_frontend_watch_set_frozen(struct SuperShuckieFrontendRaw *, uint32_t watch_id, bool frozen, const uint8_t *value, size_t length);
void     supershuckie_frontend_memory_unfreeze_all(struct SuperShuckieFrontendRaw *);
size_t   supershuckie_frontend_memory_frozen_ranges(const struct SuperShuckieFrontendRaw *, uint32_t *starts, uint32_t *lengths, size_t capacity);
uint32_t supershuckie_frontend_memory_frozen_count(const struct SuperShuckieFrontendRaw *);

bool     supershuckie_frontend_memory_undo(struct SuperShuckieFrontendRaw *);
bool     supershuckie_frontend_memory_redo(struct SuperShuckieFrontendRaw *);
uint32_t supershuckie_frontend_memory_writes_this_recording(const struct SuperShuckieFrontendRaw *);
```

---

## 9. Implementation phases

Each phase ends with a commit and passing tests. **Don't start Phase 2 until Phase 1's benchmark gates pass.**

1. **Core plumbing, regions, write-path fixes, benchmark**
   - §4.1: region API, table-driven `read_ram`/`write_ram`, new regions and binding accessors (including the mGBA patch-write path and melonDS JIT invalidation).
   - §4.2: state epoch.
   - §4.3: write-path fixes, each with a test that shows the problem first.
   - §4.4–4.6: monitor (sampling, traces, freezes, edits, snapshots), commands, park/unpark.
   - §4.7: bench flags.
2. **RAM viewer (read-only)**
   - `MemoryToolsState`, core-swap handling, viewer C API.
   - Tools menu, controller, separate multi-instance viewer windows, region tabs, navigation, inspector.
   - Window state persistence.
3. **RAM search**
   - `supershuckie-memory-tools` crate (codecs, tables, dense/sparse engine, rank paging, history) and its tests.
   - Worker, session validity, C API, search window.
4. **RAM watch**
   - Watch model, pointer paths, JSON persistence, traces, `pause_when`, C API, watch window, change log.
5. **Editing and freezing**
   - Codec encoding, edit/freeze C API, global undo/redo.
   - Editing in the viewer, watch and search windows; frozen highlighting.
   - Status bar indicators; recording policy dialogs; playback suspension.
6. **Polish and docs**
   - README section, including the replay compatibility note.
   - `docs/` page on region addresses, `.tbl` tables and pointer paths.
   - Optional REST endpoints.

---

## 10. Verification

### 10.1 Performance gates

Run `nds_bench` on the HeartGold replay from the perf report, with one frame in four drawn. **Every configuration must also pass `--verify` bit-identically.** Configurations that write (freezes) are checked by record → playback instead (§10.2).

| Configuration | Gate |
|---|---|
| Monitor attached, `enabled = false`, nothing traced or frozen | Mean and p99 within run-to-run noise |
| `--mm-window 16384 --mm-probes 128 --mm-hz 30` | Mean Δ ≤ 0.5 %, p99 within noise, over-budget % unchanged |
| `--mm-traces 64` | Mean Δ ≤ 1 % |
| `--mm-freezes 256` (restore every frame, worst case) | Mean Δ ≤ 1 %; exactly one write per changed freeze per frame |
| Single full snapshot | < 0.5 ms on the reference machine |
| `--mm-snapshot-every 1` | Stress only; documents why snapshots are on-demand |

Then test in the app with HeartGold at 4×: two viewers, search results, 20 display watches, 10 traced watches and 16 freezes open. *Frames over budget* and emulation FPS in the status bar tooltip must match the tools-closed numbers within noise over 2 minutes. Record the numbers in the PR.

### 10.2 Correctness tests

- **`supershuckie-memory-tools`:**
  - Every comparison × type × endianness × alignment.
  - Region-boundary straddling.
  - BCD, float epsilon, nibble wildcards, `.tbl` parsing.
  - **Encode/decode round-trips and encode errors** (out of range, invalid BCD, unmappable text).
  - Dense/sparse results match a brute-force reference implementation (property test).
  - Rank paging, history cap, pointer resolution, watch JSON round-trip (freezes load inactive).
- **Core (`core_smoke` + new tests):**
  - Region API ≡ `read_ram` for every region; **every address that was valid before this change maps to the same bytes as before**.
  - Monitor attached with 64 traces: record → playback → identical end state.
  - **Freeze determinism:** record with a freeze on a value the game changes, play back, and get a bit-identical end state. The replay contains exactly one `WriteMemory` per frame on which the value differed, and none on other frames.
  - **Poke-A-Byte freeze regression:** at most one `WriteMemory` per frozen address per frame while recording.
  - **`enqueue_write` during playback** doesn't grow `writes`, and nothing is applied on detach.
  - An unmapped `WriteMemory` during playback doesn't panic.
  - Edits during playback → `WriteFailed::Playback`. Edits to GBC I/O → `ReadOnlyRegion`.
  - A GBA palette edit changes the next rendered frame (patch path works).
  - A trace across a seek → `Discontinuity`, no `Changed`. `pause_when Equals` pauses on the exact frame.
  - Undo restores the exact old bytes.
- **Manual QA:**
  - Region tabs, shortcuts and Go-to across all three consoles; four viewer windows at once.
  - Scrolling and editing while paused (park/unpark).
  - Tools survive ROM load / reload core / unload; freezes deactivate on ROM change and persist on reload core.
  - Recording confirmation and "freezes active" dialogs; the RAM modified indicator.
  - Freezes are suspended during playback and resume afterwards.
  - Video export with tools open; Poke-A-Byte and tools running at the same time.

---

## 11. Alternatives considered and rejected

1. **UI thread reads emulator RAM through a raw pointer.**
   - It's a data race (undefined behavior while the core thread holds `&mut`).
   - Multi-byte values can be torn.
   - Pointers become invalid across core swaps.
   - It saves nothing measurable.
2. **Copy all RAM every frame.** On NDS at 4× that's about 1 GiB/s of memcpy, roughly 5–10 % of the frame budget.
3. **Search comparisons on the core thread.** They would stall frames for milliseconds.
4. **True write-locks and instruction-level watchpoints.** These need a callback on every emulated memory access in mGBA, melonDS and SameBoy. That costs time on every write even when unused, and it disturbs the PGO-trained melonDS build. Frame-boundary freezes and change detection cover the practical cases.
5. **Tool writes that bypass the recorder** (writing `core.write_ram` directly). Replays would silently desync on playback. All writes go through `enqueue_write`.
6. **Tabbed or docked single "memory tools" window.** Rejected in favor of separate windows, which are easier to position and to capture on stream.
7. **Reusing Poke-A-Byte's shared memory or freeze map as-is.** Its layout is driven by the external client, and its freeze loop has the problems in §2.4. The two share the address space and the fixed freeze helper instead.

---

## 12. Open questions (defaults assumed if unanswered)

1. **New GBC addresses** (§4.1). *Default:* WRAM (upper) extends the existing `0x10000` formula to the full RAM; OAM and I/O use their real addresses; cart RAM goes at synthetic `0x20000+`. Existing Poke-A-Byte addresses are unchanged. Other tools won't know the synthetic cart RAM address.
2. **Writes to new regions while recording** break playback on older builds (§8.4). *Default:* allow them and document it. The alternative is to block tool writes to new regions while recording.
3. **Freezes on ROM load.** *Default:* restored inactive (§7.4).
4. **REST exposure** of read, watch, edit and freeze. *Default:* a follow-up after Phase 6.
