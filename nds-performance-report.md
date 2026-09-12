# Frame-Rate Report: Holding 4× (≈240 fps) on Nintendo DS

**Scope:** Super Shuckie 0.4.11, branch `resume-from-replay`, Windows build (MSYS2 UCRT64, GCC 16.1). Measured on this machine (Core Ultra 9 285K, 8 P + 16 E cores, RTX 3080 Ti, Windows 11, *Balanced* power plan) with the user's own HeartGold replay `h-ampharos-line-1-21318.v4.replay` (8.9 h of emulated frames).
**Companion:** the same report with the frame-time chart — https://claude.ai/code/artifact/5f9145e5-5856-42a0-a8d5-70292e03a67f
**Status:** the recommendations were implemented the same day — see §0 for what landed and the after numbers.
**Method:** a new headless benchmark, `supershuckie-core/examples/nds_bench.rs`, drives the melonDS core exactly the way the core thread does (replay packets → input → `run_unlocked()` → framebuffer copy) with no UI, pacing or recorder, and reports per-frame timings, keyframe costs and state-level determinism checks. Reproduction commands are in §9.

---

## 0. Status: implemented (2026-09-12)

Everything in §4 except the optional `-DENABLE_JIT=OFF` experiment is in the tree, built and verified. Measured with the final library (`build/melonDS/src/libcore.a`: patched, PGO-trained on the HeartGold replay, LTO), same benchmark as §3:

| Segment | Before (§3) | After: every frame drawn | After: one frame in four drawn (what the app does at 4×) |
|---|---|---|---|
| First 12,000 frames | 2.69 ms mean, p99 4.78, 4.3 % over budget (371 fps) | 2.18 ms, p99 3.85, 0.4 % (458 fps) | **1.90 ms mean, p99 3.79, 0.6 % over budget (526 fps)** |
| From frame 400,000 (heaviest) | 3.21 ms, p99 5.19, 10.7 % (312 fps) | — | **2.20 ms, p99 4.06, 0.8 % (454 fps)** |

Every run above was checked against the replay's recorded keyframes (`--verify`): all bit-identical (the verifier compares the GX FIFOs logically, see §6). In the app: HeartGold and Platinum at 4× hold a steady **240 FPS** on the (now true) status-bar counter using **0.7–0.8 cores** in total, where the old loop pinned a full core spinning.

What changed, by item of §4:

| § | Change | Where |
|---|---|---|
| 4.1 | PGO + LTO build of melonDS, scripted: `scripts/build-melonds.ps1` (Windows) / `scripts/build-melonds.sh` (used by `build.sh`); `-flto=auto` on the app link | `scripts/`, `build.sh`, `supershuckie-qt/CMakeLists.txt`, README |
| 4.2 | Skip compositing of undisplayed frames: `GPU::SkipDrawing` + `SkipScanline_BGOBJ` (mirrors the affine-reference, mosaic and window side effects, honours display capture) as a melonDS patch; `melonds_rs_core_set_skip_drawing`; `EmulatorCore::set_skip_drawing`; the core draws one frame in `floor(speed)` from 2× up, seeks draw only their target frame, export and <2× draw everything; only drawn frames are published to the UI (`screen_generation`) | `melonds-rs/patches/0001-…`, `melonds-rs/interface.cpp`, `melonds-rs/src/lib.rs`, `supershuckie-core/src/{emulator.rs,emulator/nintendo_ds.rs,lib.rs,thread.rs}`, `supershuckie-frontend/src/lib.rs` |
| 4.3 | Keyframe buffers recycled through the recorder (`take_recycled_state` → `take_free_state_buffer`, pool of 3 in `SuperShuckieCore`); `create_save_state_into` on both bindings; the player no longer clones the 20 MB state per keyframe (`set_keyframe_states_wanted(false)`, `current_keyframe_state`) | `supershuckie-replay-recorder/src/replay_file/{record.rs,record/thread.rs,playback.rs}`, `mgba-rs`, `melonds-rs`, `supershuckie-core` |
| 4.4 | Core thread and melonDS render thread opt out of EcoQoS and run at above-normal priority; the process opts out too (`main.cpp`); sleep-then-spin pacing (`microseconds_until_next_frame`, wake 1 ms early, 8 ms cap) | `supershuckie-core/src/thread.rs`, `melonds-rs/interface.cpp`, `supershuckie-qt/src/main.cpp` |
| 4.5 | Status-bar FPS = emulated frames per second (tooltip: display rate, average/worst frame time, budget, frames over budget); `FrameTimeStats` on the core thread; REST `stats` gains `emulation_fps`, `frame_time_ms`, `frame_budget_ms`, `frames_over_budget`; UI uploads only newly drawn frames | `supershuckie-qt/src/main_window.cpp`, `supershuckie-frontend-c`, `supershuckie-frontend-webserver` |
| 4.6 | `CmdFIFOEntry` zero-initialised (part of the patch); `update_counters` only after a frame or command | melonDS patch, `thread.rs` |

Two things found on the way and fixed: seeking never restored the input recorded with the keyframe (melonDS states do not carry `KeyInput`), so the frames after a seek depended on the buttons held before it — `go_to_replay_frame_inner` now applies `metadata.input`; and `create_save_state` no longer allocates 32 MiB per call anywhere. One thing found and *not* fixed: after a seek, 9 bytes of HeartGold main RAM (0x021E1A27…) depend on what the emulator ran before the state load, with the unpatched melonDS as well — nothing in the save state differs, so it is state melonDS does not serialise; `core_smoke` reports it. Seeks from a fresh core are exactly reproducible.

Tests: the recorder suite (43, including a new buffer-recycling test), `core_smoke` (record with the pool → play back → identical end state; seeks reproducible; a real replay seek), and the `--verify` runs above. Standalone build: `build-static/supershuckie.exe`, copied to `A:\Dropbox\stp-projects\programs\supershuckie 2\supershuckie - 0.4.11stp+v4replays+perf.exe`.

Deferred: `-DENABLE_JIT=OFF` (small, untested), skipping 3D rasterisation on the render thread (≈7 %, needs the `FrameIdentical` interaction handled), throttling the 1 ms UI tick.

---

## 1. Bottom line

1. **Today the DS core keeps 4× on average but with a thin margin.** Real gameplay costs a mean of **2.5–3.2 ms per frame** against a **4.17 ms** budget, and **4–12 % of frames individually exceed the budget** (p90 sits right at 4.2 ms in the heavier segments). The core thread's 16-frame catch-up cushion hides short bursts, so the app *usually* holds 240 fps — but anything that eats the remaining ~25 % headroom (a heavy 3D scene, the 6.5 ms keyframe stall, OBS, Windows moving the thread to an E-core) shows up as lost speed. There is no single bottleneck; it is the interpreter's raw cost plus a handful of avoidable overheads.
2. **The JIT is not an option** — and this is now proven, not assumed. It is 1.9× faster (mean 1.42 ms) but it is *not reproducible against itself*: a fresh core loaded from a JIT core's own save state diverges within 120 frames (main RAM, GX FIFO and SPU state all differ), because melonDS compiles blocks by interpreting them on first execution and bakes that run's branch directions, IRQ arrival and memory timings into the block. Replays, seeking, resume and video export all depend on state loads, so the app's "Enable JIT (disables replays)" restriction is correct and cannot be lifted with settings. See §5.
3. **Four changes, none of which touch emulation behaviour, get the DS core from ~370 fps to an estimated ~550–600 fps mean with a p99 comfortably under budget:**

| # | Change | Measured / expected gain | Effort | Risk to replay determinism |
|---|---|---|---|---|
| A | Build melonDS with **PGO + LTO** (§4.1) | **+12–18 % throughput**, over-budget frames 4 % → 1 % on the first segment; measured bit-exact against 105 recorded keyframes | Low (build script) | None (verified) |
| B | **Skip compositing of frames that will never be shown** at >1× (§4.2) | 2D compositing is **24 % of frame time**, 3D another ~7 %; skipping 3 of 4 frames at 4× is worth **≈ +20–25 %** | Medium (small melonDS patch + app policy) | None if done as described (framebuffer is not emulation state) |
| C | **Stop allocating 32 MiB per keyframe** (§4.3) | Keyframe stall **6.5 ms → ~1.5 ms** (90 % of it is page-faulting a fresh allocation); same fix for the 20 MB clone the player does per keyframe in playback | Low | None |
| D | **Keep the core thread on a P-core and off Windows' "efficiency" path** (§4.4); sleep instead of spinning | Removes an external 30–40 % slowdown that hybrid-CPU scheduling can impose on a non-foreground app; frees one full core | Low | None |

Plus one instrumentation fix (§4.5): the FPS shown in the status bar counts *UI refreshes*, not emulated frames, so it under-reports whenever the UI thread is slow. Until that is fixed, "the counter dropped below 240" and "the game slowed down" are not the same statement.

---

## 2. How a DS frame runs today

```
Qt UI thread (1 ms QTimer → tick)                 Core thread (ThreadedSuperShuckieCoreThread::run_thread)
──────────────────────────────────                ──────────────────────────────────────────────────────
tick(): poll SDL, REST, pokeabyte flag            loop {
  refresh_screen(): if frame counter changed        try_recv command
    lock screens mutex                                poll recorder errors, seek requests
    QPixmap::convertFromImage ×2 (copy)               refresh_screen_data()   ← try_lock, swap pixel Vecs
    setPixmap → repaint                               pokeabyte RAM reads, counters (mutex lock)
  count 1 "FPS"                                       core.run():
                                                        NintendoDS::run(): if now < next_deadline → return 0 frames  ← BUSY SPIN
                                                        else run_unlocked(): melonDS RunFrame + copy 2×192 KB framebuffer
                                                      every 120 frames while recording: create_save_state() (20 MB) → recorder thread
                                                  }
```

Key facts established from the code:

- **Pacing is a busy-wait.** `NintendoDS::run()` (`supershuckie-core/src/emulator/nintendo_ds.rs:60`) returns `frames: 0` until the next deadline; `run_thread` (`thread.rs:574`) never sleeps while running, so between frames the thread spins through `try_recv`, two mutex lock/unlocks (`update_counters`, pokeabyte session) and an `Instant::now()` millions of times per second. This does not limit throughput — when behind, frames run back-to-back and `last_frame_microseconds` is clamped to at most 16 frames of debt — but it pins one core at 100 % regardless of speed.
- **melonDS is built as a plain `-O3` static library** (`build/melonDS`, GCC 16.1): no LTO (melonDS' CMake force-disables it for GCC ≥ 15 because of an old ICE that no longer reproduces), no `-march`, no profile. JIT support is compiled in but disabled at runtime (`nintendo_ds_settings.jit = false`), which means every RAM write still pays the JIT's `CheckAndInvalidate` table lookup.
- **The threaded software 3D renderer is on** (`interface.cpp:38`, `SetThreaded(true)`); 3D rasterisation runs on a second thread, hand-shaking per scanline through semaphores.
- **No audio is emitted**, but the SPU still mixes (cheap).
- **Every keyframe (120 frames) does `Vec::with_capacity(32 MiB)` + a 20 MB `DoSavestate`** on the core thread (`melonds-rs/src/lib.rs:80`) before handing the buffer to the recorder thread.
- **Every keyframe passed during playback is folded and then cloned (20 MB)** to produce a `Packet::Keyframe` that the core ignores unless auto-resync is on (`playback.rs:611-624`).
- **The status-bar FPS** is `frames_in_last_second`, incremented once per UI tick that observed a new frame (`main_window.cpp:1158`), capped by tick rate and repaint cost.

---

## 3. Measurements

All runs: HeartGold-2026 v1.4.1, inputs from the replay, interpreter unless stated, 600 warm-up frames, timings of `run_unlocked()` only (melonDS `RunFrame` + framebuffer copy). Budget at 4× = 4.166 ms. Run-to-run noise ≈ ±4 %.

### 3.1 The first 12,000 frames (start of the run)

| Build | Mean | p50 | p90 | p99 | Max | Frames > budget | Mean fps |
|---|---|---|---|---|---|---|---|
| **Current build** (`-O3`) | 2.69 ms | 2.89 | 3.87 | 4.78 | 7.3 | **4.3 %** | **371** |
| `-march=x86-64-v3` | 2.63 | 2.87 | 3.86 | 4.80 | 7.7 | 5.1 % | 380 |
| LTO | 2.58 | 2.81 | 3.80 | 4.74 | 7.4 | 3.6 % | 387 |
| PGO | 2.43 | 2.67 | 3.38 | 4.33 | 6.1 | 1.7 % | 412 |
| **PGO + LTO** | **2.30–2.36** | 2.51 | 3.25 | 4.06–4.23 | 5.9 | **0.7–1.1 %** | **424–435** |
| JIT (for reference only) | 1.42 | 1.40 | 2.08 | 2.85 | 6.8 | 0.2 % | 702 |
| Direct boot / intro (2D only) | 1.37 | 1.21 | 2.07 | 3.78 | 4.5 | 0.2 % | 729 |

### 3.2 Later segments (6,000 frames each) — the run gets heavier

| Start frame (≈ time in) | Current build: mean / p90 / p99 / > budget | PGO + LTO: mean / p90 / p99 / > budget |
|---|---|---|---|
| 400,000 (1 h 51 m) | 3.21 ms / 4.20 / 5.19 / **10.7 %** (312 fps) | 2.68 / 3.58 / 4.51 / 2.3 % (373 fps) |
| 900,000 (4 h 10 m) | 2.45 / 4.18 / 5.23 / 10.4 % (408 fps) | 2.23 / 3.75 / 4.67 / 4.7 % (448 fps) |
| 1,400,000 (6 h 29 m) | 2.87 / 4.00 / 5.40 / 7.4 % (349 fps) | 2.52 / 3.49 / 4.77 / 2.6 % (397 fps) |
| 1,800,000 (8 h 20 m) | 2.59 / 4.43 / 5.89 / **12.0 %** (386 fps) | 2.34 / 3.94 / 5.46 / 9.1 % (428 fps) |

In three of four segments the current build's **p90 is at or above the 4× budget**: one frame in ten is late, and the 16-frame cushion is what keeps the game at 4×. That is the concrete reason the margin feels fragile.

### 3.3 Where the frame goes (experiment build, rendering stubbed out)

| Variant (first 12,000 frames) | Mean | Δ vs normal | Mean fps |
|---|---|---|---|
| Normal | 2.63 ms | — | 381 |
| 2D compositing skipped (when no display capture) | 2.00 ms | **−24 %** | 500 |
| 3D rasterisation skipped (render thread) | 2.43 ms | −7 % | 411 |
| Both | 2.08 ms | −21 % | 480 |

The remaining ~2.0 ms is ARM9/ARM7 interpretation, 2D/3D register-level work, DMA, timers and SPU — i.e. the part that must run for every frame. Rendering is the only large slice that is optional for frames nobody sees.

### 3.4 Periodic stalls on the core thread

| Event | Cost | Frequency at 4× | Cause |
|---|---|---|---|
| Keyframe `create_save_state()` while recording | **6.1–6.6 ms mean, 8–11 ms max** (1.5–2.5 frames of budget) | every 0.5 s | fresh `Vec::with_capacity(32 MiB)` each time; measured separately: fresh allocation + 20 MB write = **5.26 ms**, the same copy into a reused buffer = **0.54 ms** |
| Keyframe materialisation during playback | ~7 ms (fold + 20 MB clone) | every 0.5 s | `chain.state.clone()` in `ReplayFilePlayer::materialize` even when the consumer discards it |
| Replay packet decode (excl. keyframes) | ≈ 0.06 ms/frame | every frame | fine |
| Framebuffer copy in `run_unlocked` | ≈ 0.03 ms/frame | every frame | fine |

### 3.5 Determinism checks

| Test | Result |
|---|---|
| Interpreter vs the app's recorded keyframes (105 keyframes over 12,600 frames) | **all match** (after masking the GX-FIFO padding bytes, see §6) |
| PGO + LTO build vs the same recorded keyframes | **all match** — the optimised build is emulation-equivalent |
| Interpreter, fresh core loaded from a warm core's snapshot, 6,000 frames lock-step | **matches throughout** (50 comparisons) |
| **JIT**, same test | **diverges at +120 frames**: 49 bytes of main RAM, 1,080 bytes of GX FIFO/GP3D state, SPU state |
| JIT vs interpreter-recorded keyframes | diverges from the first keyframe (expected) |

### 3.6 OS facts that matter

- `std::thread::sleep` on this machine overshoots by ~0.5 ms (100 µs → 550 µs, 1 ms → 1.5 ms): a sleep-until-1 ms-before-deadline strategy is viable, a naïve `sleep(remaining)` is not.
- Power plan is *Balanced*. On a hybrid CPU under Balanced, Windows 11 freely schedules threads of non-foreground processes onto E-cores (Skymont ≈ 60–70 % of a Lion Cove core at lower clocks). Nothing in the app opts out.

---

## 4. Recommendations (in priority order)

### 4.1 Build melonDS with PGO + LTO — do this first

Measured +12–18 % on the core, tail frames cut by 2–4×, and verified bit-exact against the archive's keyframes. Nothing about emulation changes; only code layout and inlining. Recipe (already exercised in `build/melonDS-pgo`):

```
# 1. instrumented build
cmake -G Ninja ./melonds-rs/melonDS -B build/melonDS-pgo -DENABLE_JIT=ON -DENABLE_OGLRENDERER=OFF \
      -DENABLE_GDBSTUB=OFF -DBUILD_QT_SDL=OFF -DCMAKE_BUILD_TYPE=Release \
      -DCMAKE_CXX_FLAGS="-fprofile-generate -fprofile-update=atomic" -DCMAKE_C_FLAGS="-fprofile-generate -fprofile-update=atomic"
cmake --build build/melonDS-pgo -j
# 2. train: link nds_bench against it (see §9) and run a representative replay, e.g.
nds_bench <rom> --replay <replay> --frames 36000 --warmup 0 --keyframes 120     (~10 min instrumented)
# 3. optimised rebuild in the same directory (the .gcda files sit next to the objects)
cmake ... -B build/melonDS-pgo -DCMAKE_CXX_FLAGS="-fprofile-use -fprofile-correction -fprofile-partial-training -Wno-missing-profile -flto=auto" (same for C)
cmake --build build/melonDS-pgo -j
# 4. point the app build at it: -DMELONDS_CORE=build/melonDS-pgo/src/libcore.a (or copy over build/melonDS/src/libcore.a)
```

Notes:
- The final `supershuckie.exe` link is done by g++ through CMake, so LTO objects in `libcore.a` are handled by the linker plugin automatically (link time rises to ~30 s). The static (`SUPERSHUCKIE_STATIC`) build works the same way.
- Train on more than one game (HeartGold and a Gen-5 title at least) — `-fprofile-partial-training` keeps untrained paths at normal `-O3` so nothing gets slower.
- Re-collect the profile only when melonDS sources change; check the `.gcda` files into a `pgo/` folder or regenerate in `build.sh`.
- **`-march=x86-64-v3` is not worth it**: +3 % (noise-level) and it enables FMA contraction, which could in principle perturb float paths (audio resampling, nothing in the integer 3D/CPU core — but the gain does not justify auditing that).
- **Optional: `-DENABLE_JIT=OFF`.** The interpreter path would drop the per-write `CheckAndInvalidate` lookup and the memory layout changes. Untested here because `melonds-rs/interface.cpp` hard-codes `#define JIT_ENABLED 1` (the `NDS` class layout depends on it, so both must agree). Since the JIT cannot be used with replays anyway (§5), removing it entirely is reasonable; expect a small single-digit gain. If you keep it compiled in, keep the menu item.
- Rust side: `supershuckie-frontend-c` is already `lto = true, opt-level = 3`; adding `codegen-units = 1` is free and harmless.

### 4.2 Do not render frames that will never be displayed

At 4× the UI can show at most 60 of the 240 emulated frames per second (`refresh_screen` samples the latest frame per tick; the monitor is 60 Hz). §3.3 shows compositing costs ~24 % of every frame and 3D rasterisation ~7 % more. Skipping the *drawing* of three frames in four is worth roughly +20–25 % on top of 4.1.

What has to stay untouched for determinism (the framebuffer itself is not part of the save state, so nothing in `create_save_state` sees the difference):

- Everything before the draw in `GPU2D_Soft::DrawScanline` (VRAM-flat coherence, `CaptureLatch`, `GetLine()` — the 3D scanline handshake **must** still run or the render thread deadlocks), and `UpdateMosaicCounters`.
- **Display capture** (`CaptureCnt & (1<<31)`, latched at line 0): captured frames are written back into VRAM and games read them, so a frame with capture latched is always rendered fully. The experiment build already honoured this.
- **Display mode 3** (main-memory display FIFO) drains a FIFO fed by DMA — timing-visible — so that mode must render normally too (the experiment skipped it; a real patch must not).
- Video export and the last frame of a seek must render.

Implementation sketch: a `bool SkipDrawing` on `GPU` (not serialised), set by a new `melonds_rs_core_set_skip_drawing(core, bool)` before `RunFrame`; the app's core thread sets it to `true` for frames it will not present. A simple, stable policy: while `speed > 1`, present every `round(speed)`-th frame (4× → every 4th frame → an even 60 fps on screen, which also looks smoother than today's "whatever frame the 1 ms tick happened to catch"). The same flag lets `run_unlocked` skip the 393 KB framebuffer copy for skipped frames. The 3D rasteriser can additionally early-out on the render thread (post the 192+1 scanline semaphores and return), but that is the smaller win and can come later.

This is a fork-local melonDS patch (~30 lines). Upstream has no frame-skip because it has no reason to; keep the patch small and documented in `melonds-rs/`.

### 4.3 Remove the periodic stalls

1. **Recording keyframes — reuse buffers.** `Core::create_save_state` allocates 32 MiB every time; 90 % of the 6.5 ms is the kernel faulting in fresh pages. Give the core a pool of 2–3 pre-touched buffers: `create_save_state_into(&mut Vec<u8>)` in `melonds-rs`, `ByteVec::Heap` handed to the recorder thread as now, and the recorder thread sends the `Vec` back over a channel once the diff is taken (`ThreadedReplayFileRecorderThread` already owns a channel pair). Expected: 6.5 ms → ~1.5 ms per keyframe. The same applies to the GBA/GBC cores and to the frontend's `create_save_state_now()` history feature.
2. **Playback keyframes — stop cloning.** `ReplayFilePlayer::materialize` folds the delta into the chain and then clones the whole 20 MB state to build a `Packet::Keyframe`, even though `SuperShuckieCore::handle_replay` discards it unless `auto_resync_keyframes_in_replays` is on. Either return a borrowed view (`Packet::KeyframeRef` / `&chain.state`) or let the core tell the player up front whether it wants keyframe states. Keep folding (it is what makes the next seek cheap), drop the clone. Expected: ~7 ms → ~2 ms per keyframe in playback.
3. **`splice_live_transient_buffers`** (resync path) creates a full live state per resync keyframe; with resync off (the user's setting) it never runs, so it is fine — just do not enable resync by default.

### 4.4 Make Windows treat the core thread as what it is

Everything above is in-process. The largest *external* risk to "never lose 4×" on this machine is scheduling: a busy-spinning thread in a process that is not the foreground window, on a Balanced power plan, is exactly what Windows 11's hybrid scheduler is willing to park on an E-core, and the 3D render thread (bursty, semaphore-driven) is an even more likely candidate. A P-core → E-core move is a 30–40 % single-thread loss, which is more than the whole headroom in §3.2.

- In the core thread (Rust, `thread.rs` start) and in `Platform::Thread_Create` (`interface.cpp`, the 3D render thread): `SetThreadInformation(ThreadPowerThrottling, {ControlMask = THREAD_POWER_THROTTLING_EXECUTION_SPEED, StateMask = 0})` — an explicit opt-out of EcoQoS, the quality-of-service class in which Windows prefers efficiency cores and lower clocks — plus `SetThreadPriority(THREAD_PRIORITY_ABOVE_NORMAL)`. Do the same for the process (`SetProcessInformation(ProcessPowerThrottling)`) so the recorder/zstd thread is covered. This is ~20 lines behind `#[cfg(windows)]`/`#ifdef _WIN32`.
- **Replace the busy-wait with sleep-then-spin.** Compute the deadline in `NintendoDS::run`/`GameBoyAdvance::run`, `thread::sleep(remaining − 1 ms)` when `remaining > 1.5 ms`, then spin. Measured sleep overshoot here is ~0.5 ms, so cadence precision is unchanged while the thread stops burning a core (and stops looking "busy but low-value" to the scheduler). Also avoids the two per-iteration mutex acquisitions.
- Ask the user to set the Windows power mode to *Best performance* while streaming; it changes the hybrid scheduling policy globally and costs nothing.
- Verify rather than assume: with the app running, Process Explorer (or `Get-Process supershuckie | % Threads`) shows each thread's *Ideal Processor*; on this CPU the P-cores are normally enumerated as logical processors 0–7.

### 4.5 Measure the right thing

- **Status-bar FPS:** derive it from `get_elapsed_time().frames` deltas (true emulated frames per wall second), and show the UI refresh rate separately if wanted. Today's counter cannot exceed the tick rate and drops whenever repainting is slow, so it reads "< 240" in situations where the game is actually running at 4×.
- Add cheap per-frame stats on the core thread (EMA of frame time, count of frames over budget, current catch-up debt in frames) and expose them through the REST `stats` endpoint. That turns "did we lose 4×?" into a number the streaming tooling can log.
- Throttle the UI: the 1 ms `QTimer` makes the UI thread upload and repaint up to ~240 times per second; presenting at 60 Hz (or on a `QTimer` matched to the frame-skip policy of 4.2) removes ~15 % of a core of UI work and makes on-screen motion at 4× even.

### 4.6 Small things, all safe

- `run_unlocked` copies both framebuffers every frame; with 4.2's flag, copy only presented frames.
- Zero `CmdFIFOEntry` before use in melonDS (`entry._contents = 0` at `GPU3D.cpp:2607` and the other constructors): the three padding bytes per GX-FIFO entry are uninitialised stack garbage that `DoSavestate` copies verbatim (§6). Zeroing them is behaviour-neutral, makes keyframe deltas slightly smaller, and lets a byte-exact verifier work without masks.
- `update_counters()` is called every loop iteration; it only needs to run after a frame.

---

## 5. Why the JIT stays off (and why no JIT setting fixes it)

`ARMJIT::CompileBlock` (`melonds-rs/melonDS/src/ARMJIT.cpp:532-822`) compiles a block by **interpreting the instructions once** and recording what happened: which way each conditional branch went (`branch_FollowCondTaken/NotTaken` extend the block along the direction taken *that time*), the data-access cycle counts observed (`instrs[i].DataCycles = cpu->DataCycles`), literal values, and it **stops the block early if an IRQ became pending or the CPU halted during that first run** (`while (... && !cpu->Halted && (!cpu->IRQ || (cpu->CPSR & 0x80)))`). Compiled blocks are then reused, and IRQs are only serviced between blocks (`ARM.cpp:640-652`), whereas the interpreter checks after every instruction.

So the *shape* of the block cache — and with it interrupt timing — depends on execution history. Any save-state load resets the cache (`NDS::DoSavestate` → `JIT.Reset()`), after which blocks are re-shaped by the new history. That is exactly what the lock-step test in §3.5 shows: two JIT cores fed identical inputs, one warm and one freshly loaded from the other's own snapshot, disagree on main RAM within 120 frames. The interpreter passes the same test.

Consequences: a JIT recording cannot be replayed from its initial keyframe (cold cache vs the warm cache that recorded it), seeking to a keyframe cannot reproduce the recording, resume-from-replay would fork the history, and video export would not match what was played. Changing `MaxBlockSize`, `BranchOptimizations` or `LiteralOptimizations` does not remove the compile-time-observed cycle counts or the IRQ cut-off, so no runtime setting makes it reproducible; making it so would mean redesigning block formation in the JIT. Not a project-sized task for this codebase.

The same reasoning rules out **upgrading melonDS** as a speed lever: newer melonDS timing changes would desync the existing 346 GiB archive (`core_name` is checked on attach for this reason). Speed work must be behaviour-preserving — which is what §4 sticks to.

---

## 6. Side finding: uninitialised bytes in melonDS save states

`CmdFIFOEntry` is `union { u64 _contents; struct { u32 Param; u8 Command; }; }`; the `CmdFIFO`/`CmdPIPE`/`CmdStallQueue` `DoSavestate` calls `VarArray` the raw entries, so bytes 5–7 of each entry are whatever was on the stack when `entry.Command`/`entry.Param` were assigned. They differ between builds (and even between links of the same library) while all real state is identical. A verifier must mask these 324 entries × 3 bytes inside the `GP3D` section (the bench does; see `states_match` in `nds_bench.rs`), and they also add a little noise to every v4 region delta. Zeroing the entry on construction fixes both; it is an upstream-worthy one-liner.

---

## 7. What was tried and is *not* recommended

| Idea | Verdict |
|---|---|
| Turn on the JIT | 1.9× faster, non-reproducible (§5). No. |
| `-march=x86-64-v3` | +3 %, within noise; adds FMA contraction risk. Skip. |
| OpenGL / compute 3D renderer | Needs a GL context on the core thread and a read-back per frame; 3D is only ~7 % of the main thread's time with the threaded software renderer. Not worth it. |
| melonDS frame-skip | Does not exist upstream; the equivalent is §4.2 done carefully. |
| Reducing `frames_per_keyframe` cost by changing the format | Not needed once the allocation is fixed (§4.3); the 20 MB copy itself is 0.5 ms. |
| Running audio/SPU less often | SPU mixing is small and it is emulation state; leave it. |

---

## 8. Suggested order

1. **PGO + LTO build** (half a day incl. wiring the profile step into `build.sh`/README; nothing else changes). Verify with `nds_bench --verify` on two or three archive replays before shipping.
2. **Keyframe buffer reuse + playback clone removal** (half a day; pure Rust).
3. **Thread power-throttling opt-out, priorities, sleep-then-spin pacing, true-FPS counter and frame-time stats** (half a day; small, Windows-specific bits behind cfg).
4. **Skip-drawing for undisplayed frames** (1–2 days: melonDS patch, FFI flag, presentation policy, export/seek exceptions; test with display-capture-heavy scenes such as the Pokétch and battle transitions, and with `--verify`).

Expected after 1–3: mean ≈ 2.3 ms (≈ 430 fps), p99 ≈ 4.2 ms, no 6 ms stalls, immune to background-app throttling. After 4: mean ≈ 1.8–1.9 ms (≈ 550 fps), p99 well under budget — roughly a 2× safety margin over 4× on this machine.

---

## 9. Reproducing the numbers

Files added by this investigation:

- `supershuckie-core/examples/nds_bench.rs` — the benchmark / determinism checker (`--present-every` exercises the skip-drawing path).
- `supershuckie-core/examples/core_smoke.rs` — `SuperShuckieCore` smoke test: recording with buffer reuse, playback, seeks.
- `scripts/build-melonds.ps1`, `scripts/build-melonds.sh` — the PGO + LTO melonDS build (apply patches, instrument, train, rebuild).
- `melonds-rs/patches/` — the local melonDS changes (§0) as a patch file, with a README.
- `build/melonDS` now holds the trained profile (`*.gcda`) and the PGO+LTO `libcore.a`; the experiment trees from §3 were removed.

Linking the example against the CMake-built static libraries (from PowerShell, MSYS2 UCRT64 on PATH; `cargo` must be run from PowerShell on this machine):

```
cargo rustc --release -p supershuckie-core --example nds_bench -- `
  -C link-arg=-Wl,--start-group -C link-arg=build/melonDS/src/libcore.a `
  -C link-arg=build/melonDS/src/teakra/src/libteakra.a -C link-arg=build/mgba/libmgba.a `
  -C link-arg=-lstdc++ -C link-arg=-lshlwapi -C link-arg=-lws2_32 -C link-arg=-lmingwex -C link-arg=-lmingw32 `
  -C link-arg=-lmsvcrt -C link-arg=-lucrt -C link-arg=-lkernel32 -C link-arg=-luser32 -C link-arg=-lgcc -C link-arg=-lgcc_eh `
  -C link-arg=-Wl,--end-group
# (add -C link-arg=-fuse-linker-plugin -C link-arg=-flto=auto -C link-arg=-O3 for an LTO libcore.a; the same runtime libs again after that)

target\release\examples\nds_bench.exe <rom.nds> --replay <file.replay> --frames 12000 --warmup 600            # throughput
target\release\examples\nds_bench.exe <rom.nds> --replay <file.replay> --frames 12000 --keyframes 120 --verify  # + keyframe cost + determinism vs the recording
target\release\examples\nds_bench.exe <rom.nds> --replay <file.replay> --frames 6000 --keyframes 120 --repro 3000 [--jit]  # warm-vs-fresh reproducibility
target\release\examples\nds_bench.exe <rom.nds> --replay <file.replay> --start 900000 --frames 6000             # a later segment
```

Caveat on absolute numbers: the benchmark measures the core in isolation on an otherwise idle machine. In the app, add the recorder thread (zstd 9, off-thread), the UI thread and whatever else is streaming; the *relative* gains carry over, the absolute headroom in production is smaller than the tables suggest — which is the argument for doing all four items rather than picking one.
