# Audio Playback — Implementation Spec

**Target:** Claude Code, working in the `SnowyMouse/supershuckie` repository.
**Components:** `supershuckie-core`, the three core bindings (`melonds-rs`, `mgba-rs`, safeboy via `supershuckie-core/src/emulator/game_boy_color.rs`), `supershuckie-frontend`, `supershuckie-frontend-c`, `supershuckie-qt`.

---

## 1. Goal

Super Shuckie currently emulates sound on all three consoles but never plays it: no core binding exposes samples, no crate opens an audio device, and `main.cpp` initialises SDL without `SDL_INIT_AUDIO`. Add audio playback with these requirements:

1. **Off by default.** A fresh install (or an existing `settings.json` without the new section) plays nothing. Audio only starts when the user turns it on, and the choice persists.
2. **A new top-level "Audio" menu** in the Qt frontend holding every audio setting: enable, mute, volume, mute-when-sped-up, buffer/latency. Nothing audio-related goes in the existing Settings menu.
3. **Replay determinism is untouched.** Reading audio out of a core must not change emulation state, save states, keyframes or replay verification. This is a hard requirement — every design decision below is filtered through it.
4. **Never stalls emulation.** The emulation thread must not block on the audio device, and the audio device must not block on the GUI thread (which is blocked by modal dialogs, window drags, and open menus on Windows).
5. **Mute when sped up.** A user who alternates between turbo and 1x play must not be barraged by fast-playing audio during the sped-up stretches: while the game runs at any speed other than 1x, audio is silent, and it comes back the moment the speed returns to 1x. This is a checkable option in the Audio menu, on by default.
6. Works in both the DLL build and the `SUPERSHUCKIE_STATIC` single-exe build.

Out of scope for this spec (listed as follow-ups in §9): audio in video export, output-device selection, a mute hotkey, dynamic rate control, melonDS interpolation modes.

---

## 2. How things work today (read this first)

Accurate as of branch `resume-from-replay` (September 2026). File paths are stable; line numbers are omitted.

### 2.1 Threading and where a frame is produced

- `supershuckie-core/src/lib.rs` — `SuperShuckieCore` wraps a `Box<dyn EmulatorCore>`. Every frame goes through `do_run_fn(run_fn)`, which is called from exactly three places: `run()` (paced, what the user watches), `run_unlocked()` (no pacing: GB mid-frame completion in `finish_current_frame`, video export in `export.rs`) and `run_unlocked_hidden()` (seeks in `go_to_replay_frame_inner`, frames not drawn). Audio must only be *audible* from the first of these.
- `supershuckie-core/src/thread.rs` — `ThreadedSuperShuckieCore` owns a dedicated thread (`ThreadedSuperShuckieCoreThread::run_thread`). The GUI talks to it through `ThreadCommand` (`SetSpeed`, `LoadSaveState`, `HardReset`, `IgnoreSpeedChangesInReplay(bool)`, …) sent over an mpsc channel; results come back through `Arc<Mutex<…>>`/atomics (screens, elapsed time, counters). `run_one()` calls `self.core.run()` and sleeps until the next frame is due. The thread keeps running while the GUI is blocked.
- Speed lives in `SuperShuckieCore::game_speed: Speed` (fixed-point `speed_over_256`, `supershuckie-replay-recorder/src/packet.rs`), set by `set_speed`, which the frontend drives from `apply_turbo` (base speed × turbo).
- `EmulatorCore::set_skip_drawing` is a *presentation* hint; it never changes emulation. The new audio hooks must follow the same contract.

### 2.2 What each core already does with sound

**Game Boy / Color — SameBoy via `safeboy 0.3.0-beta.6`** (`game_boy_color.rs`)

- `GameboyCallbacks` (already implemented by `CallbackHandler`) has a default-no-op `apu_sample(&mut self, instance, left: i16, right: i16)` that SameBoy calls once per output sample.
- Samples are only rendered when a sample rate is set: `RunnableInstanceFunctions::set_sample_rate(u32)` (SameBoy `GB_set_sample_rate`). Today it is never called, so the rate is 0 and the APU renders nothing (the APU *state* is still emulated).
- `GB_apu_output_t` lives after the `unsaved` section of `GB_gameboy_t`, so setting a sample rate does not change save states. `GB_set_clock_multiplier` → `GB_update_clock_rate` → `GB_set_sample_rate(current)` re-derives the sample timing, so at 2x speed SameBoy still emits ~48 000 samples per *real* second, pitched up — different from the other two cores (see §5).
- **Setting a sample rate does change emulation, though** (found during implementation, see §6): with a rate, `GB_apu_run` batches APU work in chunks of `max_cycles_per_sample` (~44 cycles at 48 kHz; 1024 with no rate), and the joypad-bounce emulation (`joypad.c`, `should_bounce`/`semi_random`) mixes the pending `gb->apu.apu_cycles` into the pseudo-random value that decides whether a key reads as bounced. Reads inside a bounce window therefore differ between sample rates (`gb_audio_probe`: Crystal diverges at PC `0x0a72`, the joypad routine, both 0 vs 48 kHz and 44.1 vs 48 kHz; 48 vs 48 kHz never). Every existing Game Boy replay was recorded with no sample rate, so the emulated instance must keep running without one.
- `GameBoyColor::run` may return mid-frame (`RunTime.frames == 0`); samples accrue regardless.

**Game Boy Advance — mGBA** (`mgba-rs/interface.cpp`, `mgba-rs/src/lib.rs`)

- The core writes mixed stereo `int16` into `gba->audio.psg.buffer` (an `mAudioBuffer`, capacity `GBA_AUDIO_SAMPLES = 2048` frames) from its `_sample` timing event. `mCore` exposes `getAudioBuffer`, `audioSampleRate` (nominally 32 768 Hz; changes with the SOUNDBIAS resolution bits, so it must be re-read), `setAudioBufferSize`.
- No `mCoreSync` is attached (`setSync` never called), so `mCoreSyncProduceAudio` returns `true` and a full buffer simply drops new samples. Nothing about the buffer is serialised; reading it does not change state.
- `mgba-util/audio-resampler.h` (`mAudioResampler`, sinc or cosine) resamples one `mAudioBuffer` into another and is compiled into `libmgba` (`src/util/CMakeLists.txt`). This is what mGBA's own SDL frontend uses.
- `supershuckie-replay-recorder/src/keyframe_masks.rs` already treats the m4a `pcmBuffer` as transient; that is the *game's* mixer buffer inside EWRAM and is unrelated to this work.

**Nintendo DS — melonDS** (`melonds-rs/interface.cpp`, `melonds-rs/src/lib.rs`)

- `NDS::RunFrame` ends with `SPU.BufferAudio()`, which pushes the frame's samples (already resampled by blip_buf to `NDSArgs::OutputSampleRate`, default 48 000 Hz) into `SPU::OutputBuffer`, a ring of 2×`OutputBufferSize` frames (2048 at 48 kHz) that overwrites the oldest data when full.
- `SPU::ReadOutput(s16*, int frames)`, `GetOutputSize()`, `DrainOutput()`, `SetOutputSampleRate()`, `SetInterpolation()`. `ReadOutput` takes `AudioLock`; that mutex is uncontended here because we read on the emulation thread.
- `SPU::DoSavestate` saves registers, channels, capture units and `OutputLastSamples` — **not** the output ring or its read/write positions. Reading the ring is state-neutral. `GPU::SkipDrawing` does not touch the SPU.
- `Platform::Mutex_*` are implemented in `interface.cpp` with `std::mutex`, so `ReadOutput` works as-is.

**Null core** (`null.rs`) — must keep compiling with default trait impls.

### 2.3 Frontend, settings, C ABI, Qt

- `supershuckie-frontend/src/settings.rs` — `Settings` is serde JSON with `#[serde(default = …)]` on every section, so adding a section is backward-compatible. Sections are plain structs with `const DEFAULT_*: fn() -> T` defaults (see `EmulationSettings`). `SuperShuckieFrontend::write_config` persists.
- `supershuckie-frontend/src/lib.rs` — `SuperShuckieFrontend` owns the `ThreadedSuperShuckieCore` and **replaces it** whenever a ROM is loaded/reloaded (`switch_core` → `assign_core` → `after_switch_core`). Per-core settings that must survive a switch are re-applied in `after_switch_core` (`set_ignore_speed_changes_in_replay`, `set_auto_resync_keyframes_in_replay`). Settings getters/setters follow the `get_jit_enabled` / `set_jit_enabled` pattern.
- `supershuckie-frontend-c/src/frontend.rs` + **hand-written** header `supershuckie-frontend-c/include/supershuckie/frontend.h` — thin `extern "C"` wrappers (`supershuckie_frontend_get_nds_jit` / `set_nds_jit`). There is no cbindgen; both files are edited by hand and must stay in sync. `SuperShuckieFrontendCallbacksC` carries `refresh_screens` / `change_video_mode` function pointers.
- `supershuckie-qt/src/main.cpp` — `SDL_Init(SDL_INIT_EVENTS | SDL_INIT_GAMEPAD | SDL_INIT_VIDEO)`; SDL3 3.4.x from MSYS2 (`SDL_OpenAudioDeviceStream`, `SDL_SetAudioStreamGetCallback`, `SDL_SetAudioStreamGain`, `SDL_SetAudioStreamFrequencyRatio` are all available). SDL is only used from C++; no Rust crate links SDL.
- `supershuckie-qt/src/main_window.cpp` — `set_up_menu()` builds File / Gameplay / Save states / Replays / Settings in that order; each has a `set_up_*_menu()`. Checkable settings actions are wired `connect(action, SIGNAL(triggered()), this, SLOT(do_toggle_…()))` and their state is initialised from the frontend after construction (`this->nds_jit->setChecked(supershuckie_frontend_get_nds_jit(...))`) and, for hotkey-toggled ones, re-synced every `tick()`. `tick()` runs from a 1 ms `QTimer` and is stopped (`stop_timer()`) while modal progress dialogs run. `supershuckie-qt/CMakeLists.txt` lists sources explicitly.

---

## 3. Design

```
 emulation thread (Rust)                         SDL audio thread (C++)
 ───────────────────────                         ──────────────────────
 EmulatorCore::take_audio()  ──► SuperShuckieCore ──► AudioOutput ring ──► get-callback ──► SDL_AudioStream ──► device
   SameBoy callback Vec           (audible? speed?)    Arc<…>, Mutex<VecDeque<i16>>    (supershuckie_audio_output_read)   gain, freq ratio
   mGBA resampler → 48 kHz        push / discard       drop-oldest on overflow
   melonDS SPU::ReadOutput                             pad silence on underrun
```

Decisions, each with the reason:

- **All cores deliver interleaved stereo `i16` at a single fixed rate, `AUDIO_SAMPLE_RATE = 48_000`.** melonDS already outputs 48 kHz; SameBoy takes the rate directly; mGBA gets an `mAudioResampler` in `interface.cpp`. One format means one ring, one SDL spec, no rate plumbing through the C ABI.
- **Pull model on SDL's audio thread**, not push from the Qt tick. The ring is an `Arc<AudioOutput>` that is safe to read from any thread; the SDL stream's get-callback pulls from it through a C function that takes the ring handle, not the `SuperShuckieFrontend`. A blocked GUI thread (menus, dialogs, window drag, `stop_timer()`) therefore cannot cause dropouts. The emulation thread only ever does a short `Mutex` push.
- **The ring outlives cores.** `SuperShuckieFrontend` creates one `Arc<AudioOutput>` at construction and hands a clone to every `ThreadedSuperShuckieCore` it creates (`after_switch_core`), so the C++ side opens the device once and keeps one handle for the life of the window.
- **Drain happens inside `SuperShuckieCore::do_run_fn`**, the single choke point for every frame, with an `audible` flag: `run()` → audible; `run_unlocked` / `run_unlocked_hidden` → samples are taken and discarded. Seeks, exports and GB frame-completion never reach the speakers, and cores' internal buffers never fill with stale data.
- **Volume and mute are applied in SDL (`SDL_SetAudioStreamGain`)**, not by scaling samples in Rust. Rust stores the settings; C++ applies them. Mute = gain 0 (device stays open so unmuting is instant).
- **"Mute when sped up" is a first-class option, on by default.** Someone who turbos through a hallway and then plays the next room at 1x should not be barraged by chipmunked audio in between, so whenever `game_speed != 1.0` (turbo *or* a base speed other than 1x) `SuperShuckieCore` discards the samples instead of pushing them — the same rule for all three cores, no consumer-side logic, and the ring is cleared on the transition so the last 1x samples don't play as a tail. Sound resumes on the first 1x frame. Unchecking the option keeps audio playing while sped up; then the C++ side sets `SDL_SetAudioStreamFrequencyRatio(speed)` for GBA/NDS (which produce `speed`× more samples per real second) and `1.0` for Game Boy (SameBoy already scales, §2.2). This is why `AudioOutput` publishes the current speed and console kind to the consumer.
- **Latency is a bounded ring, not a clock.** Target fill ≈ 3 frames (~50 ms); hard cap from the latency setting. Overflow drops the *oldest* samples (keeps the ring from accumulating lag when the audio clock is slightly slower than the emulation clock); underrun pads silence (the get-callback simply supplies fewer bytes and SDL fills silence). With a ±0.1 % clock mismatch this produces one small glitch every few minutes; dynamic rate control (§9) can remove it later.
- **Disabled means disabled.** When audio is off, `EmulatorCore::set_audio_enabled(false)` tells SameBoy to stop rendering (sample rate 0) and mGBA to skip resampling, so the default configuration costs nothing on the emulation thread, and the SDL device is not opened at all.

---

## 4. Changes by component

### 4.1 `supershuckie-core`

**`emulator.rs`** — extend `EmulatorCore` with two default-implemented methods (keeps `NullEmulatorCore` and any external implementor compiling):

```rust
/// Sample rate, in Hz, at which every core delivers audio through `take_audio`.
pub const AUDIO_SAMPLE_RATE: u32 = 48_000;

/// Turn audio generation on or off. Off by default. When off, `take_audio` yields nothing and the
/// core may skip rendering entirely. Like `set_skip_drawing`, this must never affect emulation,
/// timing or save states.
fn set_audio_enabled(&mut self, _enabled: bool) {}

/// Append every interleaved stereo `i16` sample (left, right, …) at [`AUDIO_SAMPLE_RATE`] produced
/// since the previous call, then forget them. Must not affect emulation state.
fn take_audio(&mut self, _into: &mut Vec<i16>) {}
```

Add a doc note to `set_skip_drawing`/`take_audio` cross-referencing the determinism contract.

**New `audio.rs`** (behind `feature = "std"`, next to `thread.rs`; re-export from `lib.rs`):

```rust
pub struct AudioOutput {
    samples: Mutex<VecDeque<i16>>,     // interleaved stereo
    max_frames: AtomicUsize,           // hard cap; set from the latency setting
    speed: AtomicU32,                  // f32 bits, current emulation multiplier (for the consumer)
    fast_forward_scales_pitch: AtomicBool, // false for Game Boy (SameBoy already scales), true otherwise
}
impl AudioOutput {
    pub fn new(max_latency_ms: u32) -> Self;
    pub fn set_max_latency_ms(&self, ms: u32);
    /// Append samples; if the ring exceeds `max_frames`, drop the oldest so at most `max_frames` remain.
    pub fn push(&self, samples: &[i16]);
    /// Pop up to `into.len()/2` frames into `into`; returns frames written. The caller pads silence.
    pub fn read(&self, into: &mut [i16]) -> usize;
    pub fn clear(&self);
    pub fn queued_frames(&self) -> usize;
    pub fn speed(&self) -> f32;  pub fn set_speed(&self, f32);
    pub fn fast_forward_scales_pitch(&self) -> bool;  pub fn set_fast_forward_scales_pitch(&self, bool);
}
```

Unit tests in the same file: overflow drops oldest and keeps the newest `max_frames`; `read` returns fewer frames than asked when short; `clear`; odd-length inputs are rejected (`debug_assert!(samples.len() % 2 == 0)`).

**`lib.rs` (`SuperShuckieCore`)** — new fields `audio_output: Option<Arc<AudioOutput>>`, `audio_scratch: Vec<i16>`, `audio_mute_when_sped_up: bool` (default `true`), `audio_enabled: bool`. Changes:

- `do_run_fn(run_fn, audible: bool)`: after `run_fn`, `self.core.take_audio(&mut self.audio_scratch)`; push to the ring only if `audible && audio_enabled && (game_speed == Speed::from_multiplier_float(1.0) || !audio_mute_when_sped_up)`; always `audio_scratch.clear()` afterwards. `run()` passes `true`; `run_unlocked` and `run_unlocked_hidden` pass `false`.
- `set_speed`: also `audio_output.set_speed(multiplier)`, and when `audio_mute_when_sped_up` is set and the new speed is not 1x, clear the ring so the sped-up section starts silent immediately rather than after the queued ≤ 64 ms plays out.
- `set_audio_output(Option<Arc<AudioOutput>>)`, `set_audio_enabled(bool)` (forwards to `core.set_audio_enabled`, clears the ring on disable), `set_audio_mute_when_sped_up(bool)` (clears the ring on change so the switch is immediate).
- **Clear the ring** (so stale sound is never heard after a discontinuity) in: `load_save_state`, `hard_reset`, `go_to_replay_frame_inner`, `attach_replay_player`, `detach_replay_player`. `set_audio_output` sets `fast_forward_scales_pitch` from `core.replay_console_type()` (`false` for the three Game Boy variants).
- The unsafe constant-`zeroed` bios checksum etc. are untouched; no change to recording, keyframes or `splice_live_transient_buffers`.

**`thread.rs`** — new `ThreadCommand::{SetAudioOutput(Option<Arc<AudioOutput>>), SetAudioEnabled(bool), SetAudioMuteWhenSpedUp(bool)}` with matching `ThreadedSuperShuckieCore` methods, handled in `handle_command` by forwarding to `SuperShuckieCore`. No change to `run_one`.

### 4.2 Core bindings

**Game Boy / Color — `emulator/game_boy_color.rs`** (as implemented; the first draft set a sample rate on the emulated instance, which §2.2 explains is not replay-safe)

- The emulated instance (`core`) never gets a sample rate, so audio on or off it is byte-identical to every build so far. When audio is on, a second `ShadowAudio` SameBoy instance of the same ROM/boot ROM/model runs at `AUDIO_SAMPLE_RATE` in lockstep: one `GB_run` of the shadow per `GB_run` of the emulated instance (`GameBoyColor::step`), same input mask, same clock multiplier, `TurboMode::Enabled` (never sleeps to pace itself), rendering disabled. Only the shadow's `apu_sample` callback collects samples.
- Divergence detection is exact and cheap: identical instruction streams have identical `GB_run` cycle counts, so after each step `shadow.cycles != core.cycles` means the shadow took another path (the bounce quirk), and it is resynced by loading the emulated instance's state. A full game-visible-memory + register comparison every 60 frames is the backstop for a divergence that did not change step sizes. State loads and resets resync the shadow too.
- The state copied into the shadow has `GB_apu_t::apu_cycles` zeroed (`zero_pending_apu_cycles`): the emulated instance may have up to 1024 APU cycles pending, and a 48 kHz instance asserts on more than ~175 at once; the shadow's APU then sits at most a quarter millisecond behind, which is inaudible.
- `set_audio_enabled(true)` builds the shadow (and needs the ROM, boot ROM and model kept on the struct); `false` drops it, so disabled audio costs nothing. `take_audio` moves the shadow's vec. `audio_resyncs()` exposes the resync count for diagnostics.
- Cost: a second SameBoy instance while audio is on (~2x Game Boy CPU, still far below the DS core).

**Game Boy Advance — `mgba-rs/interface.cpp` + `mgba-rs/src/lib.rs` + `emulator/game_boy_advance.rs`**

- `MGBACoreRaw` gains `bool audio_enabled = false; mAudioResampler resampler; mAudioBuffer resampled;`. In `mgba_rs_core_new`: `mAudioBufferInit(&resampled, 8192, 2)` (~170 ms of headroom), `mAudioResamplerInit(&resampler, mINTERPOLATOR_SINC)`, `mAudioResamplerSetDestination(&resampler, &resampled, 48000.0)`. Deinit both in `mgba_rs_core_free`. Include `<mgba-util/audio-buffer.h>` and `<mgba-util/audio-resampler.h>`.
- `extern "C" void mgba_rs_core_set_audio_enabled(MGBACoreRaw*, bool)`: store; on either transition clear both `core->getAudioBuffer(core)` and `resampled` so a re-enable does not replay up to 2048 stale frames.
- `extern "C" size_t mgba_rs_core_read_audio(MGBACoreRaw*, int16_t* out, size_t max_frames)`: if disabled return 0; else `mAudioResamplerSetSource(&resampler, core->getAudioBuffer(core), core->audioSampleRate(core), true)` (re-read every call — the rate follows SOUNDBIAS), `mAudioResamplerProcess(&resampler)`, then `return mAudioBufferRead(&resampled, out, max_frames)`. The sinc interpolator keeps a few source frames as look-ahead; that is expected.
- `lib.rs`: declare both and wrap as `set_audio_enabled(&mut self, bool)` and `read_audio(&mut self, out: &mut [i16]) -> usize` (frames).
- `GameBoyAdvance::take_audio`: loop `read_audio` into a 2048-frame stack/`Vec` scratch until it returns 0, extending `into`. One frame is ~804 output frames, so this is normally a single iteration. `set_audio_enabled` forwards.

**Nintendo DS — `melonds-rs/interface.cpp` + `melonds-rs/src/lib.rs` + `emulator/nintendo_ds.rs`**

- `melonds_rs_core_new`: set `nds_args.OutputSampleRate = 48000.0` explicitly (documents the contract; it is the default). Leave `Interpolation = None` and `BitDepth = Auto` (follow-up in §9).
- `extern "C" size_t melonds_rs_core_read_audio(MelonDSCoreHolder*, int16_t* out, size_t max_frames)` → `core->nds->SPU.ReadOutput(out, (int)max_frames)`; `extern "C" void melonds_rs_core_drain_audio(MelonDSCoreHolder*)` → `SPU.DrainOutput()`.
- `lib.rs`: wrap as `read_audio(&mut self, &mut [i16]) -> usize` and `drain_audio(&mut self)`.
- `NintendoDS::take_audio`: if enabled, loop `read_audio` (2048-frame scratch) until 0; if disabled, do nothing (the SPU ring just wraps, exactly as today). `set_audio_enabled`: store the flag; on enable call `drain_audio` first so up to 2048 stale frames are not played. `hard_reset` already goes through `NDS::Reset` → `SPU::Reset` → `InitOutput` which zeroes the ring.
- Do **not** call `SPU::SetOutputSkew`/`TrimOutput`; leave sync to the ring in §4.1.

### 4.3 `supershuckie-frontend`

**`settings.rs`** — new section, default-constructed when absent from `settings.json`:

```rust
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioSettings {
    #[serde(default = "AudioSettings::DEFAULT_ENABLED")]       pub enabled: bool,            // false
    #[serde(default = "AudioSettings::DEFAULT_MUTED")]         pub muted: bool,              // false
    #[serde(default = "AudioSettings::DEFAULT_VOLUME")]        pub volume: u8,               // 100 (percent, 0..=100)
    #[serde(default = "AudioSettings::DEFAULT_MUTE_WHEN_SPED_UP")] pub mute_when_sped_up: bool, // true
    #[serde(default = "AudioSettings::DEFAULT_LATENCY_MS")]    pub latency_ms: u16,          // 64
}
```

`Settings` gets `#[serde(default = "AudioSettings::default")] pub audio: AudioSettings`. Clamp `volume` to 100 and `latency_ms` to `16..=500` on load.

**`lib.rs`** — `SuperShuckieFrontend` gains `audio_output: Arc<AudioOutput>` (constructed in `new` from `settings.audio.latency_ms`). Add:

- `after_switch_core`: `self.core.set_audio_output(Some(self.audio_output.clone()))`, `set_audio_mute_when_sped_up(settings.audio.mute_when_sped_up)`, `set_audio_enabled(settings.audio.enabled)`. Also `self.audio_output.clear()` (a new core means a new timeline).
- Getters/setters following the `jit` pattern: `get/set_audio_enabled` (forwards to the core, clears the ring on disable), `get/set_audio_muted`, `get/set_audio_volume(u8)`, `get/set_audio_mute_when_sped_up` (forwards to the core), `get/set_audio_latency_ms` (also `audio_output.set_max_latency_ms`). Each setter early-returns on no change. Persistence needs no extra work: settings are written once, in `MainWindow::closeEvent` via `supershuckie_frontend_write_settings`, the same as every other toggle.
- `audio_output(&self) -> &Arc<AudioOutput>` for the C layer.
- `before_unload_or_reload_rom` / `unload_rom`: clear the ring.

### 4.4 `supershuckie-frontend-c` (both `src/frontend.rs` and `include/supershuckie/frontend.h`)

Ring handle, thread-safe, independent of the frontend object:

```c
struct SuperShuckieAudioOutputRaw;
/** Retain a handle to the frontend's audio ring. Safe to read from any thread; release with
 *  supershuckie_audio_output_release. The handle stays valid across ROM loads. */
struct SuperShuckieAudioOutputRaw *supershuckie_frontend_retain_audio_output(const struct SuperShuckieFrontendRaw *frontend);
void supershuckie_audio_output_release(struct SuperShuckieAudioOutputRaw *audio);
/** Pop up to `frames` stereo frames (2 * frames int16 values) into `out`. Returns frames written;
 *  the caller pads the rest with silence. Never blocks for long; callable from an audio callback. */
size_t supershuckie_audio_output_read(struct SuperShuckieAudioOutputRaw *audio, int16_t *out, size_t frames);
uint32_t supershuckie_audio_output_sample_rate(void);            /* 48000 */
/** Current emulation speed multiplier and whether sped-up playback should scale pitch on the
 *  consumer side (false on Game Boy, whose core already does). Only consulted when
 *  "mute when sped up" is off. */
float supershuckie_audio_output_speed(const struct SuperShuckieAudioOutputRaw *audio);
bool supershuckie_audio_output_fast_forward_scales_pitch(const struct SuperShuckieAudioOutputRaw *audio);
```

Implemented with `Arc::into_raw` / `Arc::from_raw` (`retain` clones the `Arc`; `release` drops it). Settings, following `get_nds_jit`/`set_nds_jit`:

```c
bool supershuckie_frontend_get_audio_enabled(const struct SuperShuckieFrontendRaw *);      void supershuckie_frontend_set_audio_enabled(struct SuperShuckieFrontendRaw *, bool);
bool supershuckie_frontend_get_audio_muted(const struct SuperShuckieFrontendRaw *);        void supershuckie_frontend_set_audio_muted(struct SuperShuckieFrontendRaw *, bool);
uint8_t supershuckie_frontend_get_audio_volume(const struct SuperShuckieFrontendRaw *);    void supershuckie_frontend_set_audio_volume(struct SuperShuckieFrontendRaw *, uint8_t percent);
/** Mute audio whenever the game runs at a speed other than 1x (turbo or a non-1x base speed). Default true. */
bool supershuckie_frontend_get_audio_mute_when_sped_up(const struct SuperShuckieFrontendRaw *); void supershuckie_frontend_set_audio_mute_when_sped_up(struct SuperShuckieFrontendRaw *, bool);
uint16_t supershuckie_frontend_get_audio_latency_ms(const struct SuperShuckieFrontendRaw *); void supershuckie_frontend_set_audio_latency_ms(struct SuperShuckieFrontendRaw *, uint16_t);
```

Keep the doc-comment style of the existing header.

### 4.5 `supershuckie-qt`

**`main.cpp`** — add `SDL_INIT_AUDIO` to `SDL_Init`. Before it, `SDL_SetHint(SDL_HINT_AUDIO_DEVICE_SAMPLE_FRAMES, "512")` (~10.7 ms device period at 48 kHz; the ring above it is the real latency knob).

**New `audio_output.hpp/.cpp`** (add to `CMakeLists.txt` source list) — class `AudioOutput` owning the SDL side:

- `AudioOutput(SuperShuckieAudioOutputRaw *ring)` stores the retained handle; destructor closes the stream and releases the handle.
- `open()`: `SDL_AudioSpec{SDL_AUDIO_S16, 2, 48000}`, `SDL_OpenAudioDeviceStream(SDL_AUDIO_DEVICE_DEFAULT_PLAYBACK, &spec, &AudioOutput::get_callback, this)`, then `SDL_ResumeAudioStreamDevice`. On failure log `SDL_GetError()` and return false; the menu item shows the failure (see below). `close()` destroys the stream (`SDL_DestroyAudioStream` also closes a device opened this way).
- `get_callback(void*, SDL_AudioStream*, int additional_amount, int)` (audio thread): compute frames = `additional_amount / 4`, read into a member scratch buffer (pre-sized, no allocation on the audio thread) via `supershuckie_audio_output_read`, zero the remainder, `SDL_PutAudioStreamData`. Nothing else — no Qt, no frontend pointer.
- `set_gain(float)` → `SDL_SetAudioStreamGain` (0 when muted, else `volume / 100.0f`); `set_frequency_ratio(float)`; `clear()` → `SDL_ClearAudioStream` (used when the emulator discontinues: ROM switch, state load — driven from `MainWindow` at the same points it already handles those events, cheap to call).
- `is_open()`.

**`main_window.cpp/.hpp`**

- Member `std::unique_ptr<AudioOutput> audio;`, created right after the frontend in the constructor with `supershuckie_frontend_retain_audio_output(frontend)`. Open it immediately only if `supershuckie_frontend_get_audio_enabled` is true. Destroy before the frontend in the destructor.
- `set_up_menu()`: call `set_up_audio_menu()` between `set_up_replays_menu()` and `set_up_settings_menu()`. The **Audio** menu:

  ```
  Audio
  ├─ [ ] Enable audio                      do_toggle_audio_enabled   (checkable; default off)
  ├─ [ ] Mute                              do_toggle_audio_muted     (checkable; enabled only while audio is on)
  ├─ [x] Mute when sped up                 do_toggle_audio_mute_when_sped_up (checkable; default ON)
  ├─ Volume ▸  10% … 100% in 10% steps     NumberedAction → set_audio_volume (radio-style, checked = current)
  ├─ ──────
  ├─ Buffer ▸ (•) Low (32 ms)  ( ) Normal (64 ms)  ( ) High (128 ms)   do_set_audio_latency
  ```

  Volume and Buffer reuse the existing `NumberedAction` helper (`void (MainWindow::*)(std::uint8_t)`, as `Video scaling` does); encode Buffer as an index 0..2 and map to 32/64/128 ms in the slot. Group the radio-style items with `QActionGroup`. Keep raw `QAction *` members for the checkables so their state can be re-synced.
- `do_toggle_audio_enabled`: call the frontend setter, then `audio->open()` / `audio->close()`. If `open()` fails, uncheck the action, store `false` back into the frontend and show a `QMessageBox::warning` with `SDL_GetError()`; the user can retry.
- `do_toggle_audio_muted`, `set_audio_volume`: frontend setter + `audio->set_gain(...)`.
- `do_toggle_audio_mute_when_sped_up`: frontend setter only (the gate lives in `SuperShuckieCore`); when it turns back on while the game is already sped up, also `audio->clear()` so the stream goes quiet at once.
- `tick()`: once per tick, if the stream is open, read `supershuckie_audio_output_speed` and `…_fast_forward_scales_pitch` and, when "Mute when sped up" is off, call `set_frequency_ratio(scales_pitch ? speed : 1.0f)` only when the value changed (avoid a per-ms SDL call). When it is on, reset the ratio to 1.0 once and do nothing more — Rust already discards. Also re-sync the `Mute` check box the way `swap_nds_screens` is re-synced, so a future hotkey (§9) slots in.
- Where the window already reacts to ROM switch / close / unload (`load_rom`, `do_close_rom`, `do_unload_rom`) and to save-state loads, call `audio->clear()` if open. Optional but removes the last ≤ 64 ms of the previous timeline.
- Status: while audio is enabled and the device failed to open, disable Mute/Volume; nothing else in the status bar.

**`CMakeLists.txt`** — add `src/audio_output.cpp`. No new libraries: SDL3 (shared or `SDL3-static`) already carries the WASAPI/DirectSound backends. Verify the `SUPERSHUCKIE_STATIC` link still succeeds; if the static SDL needs extra system libs for audio on MinGW (`ole32`, `uuid` are typical) add them next to `dwmapi shlwapi`.

---

## 5. Behaviour rules (what the user experiences)

| Situation | Result | Mechanism |
|---|---|---|
| Fresh install / old `settings.json` | Silent; Audio ▸ Enable unchecked; no device opened | `AudioSettings::enabled = false`; cores told `set_audio_enabled(false)` |
| Enable audio | Sound starts within one ring fill (~50 ms) | device opened; core enabled; ring was cleared |
| Pause / replay frozen / playback stalled | Ring drains then silence; resume plays fresh audio | no frames → no pushes; get-callback pads silence |
| Turbo held or base speed ≠ 1x, "Mute when sped up" on (default) | Silent for exactly the sped-up stretch; normal sound returns on the first 1x frame | `SuperShuckieCore` discards; ring cleared on the transition so no tail plays |
| Speed ≠ 1x, "Mute when sped up" off | Pitched-up audio on all consoles | GBA/NDS: `SDL_SetAudioStreamFrequencyRatio(speed)`; GB: ratio 1.0, SameBoy already pitches |
| Seek in a replay, load save state, hard reset, ROM switch | No stale audio | `run_unlocked_hidden` discards; ring cleared at the discontinuity |
| Video export | Silent (export frames are `run_unlocked`) | `audible = false` |
| Replay recording / playback | Identical to today | audio never touches emulation state (§6) |
| Audio device unplugged | SDL migrates default-device streams automatically; if it cannot, silence | SDL3 default-device behaviour; no handling needed in v1 |
| Emulation running faster than the audio clock | Oldest samples dropped when the ring exceeds the cap; a click at most every few minutes | drop-oldest policy |

---

## 6. Determinism and replay safety (must hold)

- The only new calls into cores are `take_audio` / `set_audio_enabled`, which read output buffers (melonDS ring, mGBA `mAudioBuffer`, SameBoy callback vec) or set host-only output rates. §2.2 documents, per core, why none of these are part of the save state or influence emulation. Keep it that way: no `SetOutputSkew`, no `mCoreSync`, no `GB_set_turbo_mode` changes.
- `take_audio` is called after **every** `do_run_fn`, including seeks and export, so a core's internal buffer state is the same whether or not the user can hear it. (This is also why disabled audio must still be safe when `take_audio` is a no-op: the buffers are ring-shaped and overwrite.)
- Regression checks (all run, all passing at the time of writing):
  - `nds_bench --audio --verify` (the `--audio` flag enables audio and drains it every frame): the whole 17 263-frame HeartGold replay, also with `--present-every 4`, 142 keyframes ok, 0 desynced, 802.3 samples per frame.
  - `gb_audio_check <rom>`: two `GameBoyColor` cores from one snapshot, audio on vs off, scripted input, save states compared every 60 frames for 12 000 frames — byte-identical on Crystal 2.2.1, Crystal 2.0.0 and Blue 2027; the Crystal runs needed exactly one shadow resync (the bounce divergence), Blue none.
  - `gb_audio_probe <rom>`: the raw SameBoy demonstration of the quirk (two instances, `RATE_A`/`RATE_B` env vars), kept as documentation of why the shadow exists.
  - GBA: not measured on a ROM (none on this machine); by construction `mgba_rs_core_read_audio` only pops from a buffer the core fills either way, and `mCoreSync` stays absent.

---

## 7. Order of work

Each step builds and runs on its own; keep the app silent-by-default at every step.

1. **Core contract + ring** — `emulator.rs` trait methods, `audio.rs` with tests, `SuperShuckieCore` wiring (`audible`, clears, speed gate), `ThreadCommand`s. `cargo test -p supershuckie-core`.
2. **Game Boy** — Rust only, but not the smallest change after all: the shadow instance of §4.2. Wire the frontend settings + C API + a minimal Audio menu (Enable only) + `AudioOutput.cpp` so there is something to hear. Verify determinism with `gb_audio_check`.
3. **Nintendo DS** — two C++ functions, two Rust wrappers.
4. **Game Boy Advance** — resampler in `interface.cpp`.
5. **Rest of the menu** — Mute, Mute when sped up, Volume, frequency ratio in `tick` for the sped-up-audio case, Buffer. Persistence in `settings.json`.
6. **Static build check** and README note (a one-liner under a new "Audio" heading: off by default, where the menu is).

Steps 3 and 4 are independent of each other and of 5.

Build notes: the Windows builds go through MSYS2 UCRT64 as the README describes; `cargo` from the PowerShell side (see the project memory notes). A change to `interface.cpp` in `mgba-rs`/`melonds-rs` is picked up by the `cc` build script; the vendored `libmgba`/`libcore` themselves do not need rebuilding.

---

## 8. Verification checklist

- [ ] Fresh `settings.json`: app starts silent, Audio ▸ Enable unchecked, `SDL_GetAudioStreamDevice` never called (no device in the OS mixer).
- [ ] Enable on each console: sound within ~100 ms; no crackle at 1x for 5 minutes on NDS (the heaviest core).
- [ ] Pause/unpause; hold turbo (2x/4x) with "Mute when sped up" on: silence starts within one device period and 1x audio returns cleanly on release, repeatedly, with no tail and no growing delay; then with it off: pitch rises (GB not double-pitched) and returns (watch `queued_frames` in a debug log if unsure).
- [ ] Load save state, quick-slot load, hard reset, seek in a replay, switch ROMs — no stale audio, no crash, handle still valid.
- [ ] Open a menu and drag the window for 5 s while playing — no dropout (pull model).
- [ ] Video export with audio enabled — export unaffected, no audio during export.
- [ ] Replay determinism (§6) on all three consoles.
- [ ] Settings round-trip: toggle every item, restart, items restored; old `settings.json` without `"audio"` loads with defaults.
- [ ] `SUPERSHUCKIE_STATIC` build links and plays.
- [ ] Audio disabled again → device closed (gone from the OS mixer), CPU back to baseline.

---

## 9. Follow-ups (not in this spec)

- **Audio in video export** — `take_audio` already yields per-frame 48 kHz samples; `FfmpegVideoSink` can take a second pipe (`-f s16le -ar 48000 -ac 2`). Needs a matching `VideoFrameSink` extension.
- **Dynamic rate control** — nudge `SDL_SetAudioStreamFrequencyRatio` by ±0.5 % from the ring fill level to remove the periodic drop/pad glitch entirely (the technique mGBA and Dolphin use).
- **Output device submenu** — `SDL_GetAudioPlaybackDevices` / `SDL_GetAudioDeviceName`, persisted by name; reopen on change.
- **Mute hotkey** — a `Control::ToggleMute` variant (touches `settings.rs`, `control_settings.rs`, the header and `controller_settings_window.cpp`); the `tick()` re-sync in §4.5 already anticipates it.
- **melonDS interpolation / bit depth** — `SPU::SetInterpolation`, `SetDegrade10Bit`; per-console submenu.
- **Web server** — expose `audio_enabled` in `stats` if anyone asks; not needed.

---

## 10. Decisions made here (so they are not re-litigated)

- 48 kHz fixed everywhere, resampling inside the bindings — not a per-core rate through the ABI.
- Pull from SDL's thread through a retained ring handle — not push from the 1 ms Qt tick.
- Gain in SDL, not in Rust.
- "Mute when sped up" is a checkable option, on by default; sped-up audio is opt-in because it pitches. The gate is one comparison in `SuperShuckieCore`, not consumer logic.
- Ring cap policy is drop-oldest; no time-stretching in v1.
- Audio disabled must be zero-cost and must leave the emulation path byte-identical to today's.
