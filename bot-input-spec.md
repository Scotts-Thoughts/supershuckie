# Bot input: implementation plan

Status: implemented 2026-10-01 (uncommitted). See "Implementation notes" at the end for what
differs from the plan below.

## Goal

Let an external program play the game through the existing external-commands
server (`127.0.0.1:30158`), in either of two modes, switchable at any time:

- **Real-time mode**: the game runs at its normal pace and the bot sends
  button presses whenever it likes, as a human would. Timing is "next frame
  after the request arrives", not frame-exact.
- **Lockstep mode**: the game is paused and only advances when the bot says
  "hold these buttons and run N frames". Frame-exact and reproducible; the bot
  can take as long as it wants between steps.

Both go through the core's normal input path, so bot inputs are recorded in
replays and published to Play Together like keyboard inputs.

## What the code does today

- All input ends in `SuperShuckieCore::update_input`
  (`supershuckie-core/src/lib.rs:1877`), which runs in `before_run`, once per
  frame, and never mid-frame. It encodes the merged input, hands it to the
  core, and writes it to the recorder and stream publisher.
- `compute_pending_input` (`lib.rs:1856`) merges four layers by OR:
  `base_input` (what `enqueue_input` last set), rapid fire, toggled input, and
  timed presses (`press_for_frames`).
- The frontend keeps the keyboard/controller state in `current_input` and
  sends the whole struct with `enqueue_input` on every change
  (`supershuckie-frontend/src/lib.rs:858`). A bot calling `enqueue_input`
  would therefore be overwritten by the next key event, and vice versa.
- The core thread (`supershuckie-core/src/thread.rs:1448`) handles one command
  per loop iteration, then runs a paced frame if not paused. While paused with
  no replay attached it parks for up to 100 ms between command checks.
- Pausing freezes the recording clock (`pause_timer`, `lib.rs:934`): every
  timestamp read while paused returns the same value.
- `run_unlocked` runs without pacing and drops audio; `finish_current_frame`
  already loops it until a frame boundary. `do_run_fn` returns without running
  anything while the console is linked or held for a link (`lib.rs:793`).
- HTTP requests are queued by the webserver and drained in the frontend's
  `tick()` on the UI thread (`supershuckie-frontend/src/lib.rs:1788`), which
  Qt calls on a 1 ms timer. Each handler blocks its HTTP thread on a reply
  channel for up to 60 s.
- There is no route for input, memory reads, or screen capture.

## Design

### 1. Bot input layer (core)

Add a fifth input layer so the bot and the keyboard never overwrite each other.

- `SuperShuckieCore`: new field `bot_input: Option<Input>` and
  `set_bot_input(&mut self, input: Option<Input>)`, modelled on
  `set_toggled_input` (sets `input_latched = false`; ignored during replay
  playback like `press_for_frames`).
- `compute_pending_input`: OR `bot_input` in after the toggled input.
- Cleared wherever toggled input is cleared (`reset_input`, `lib.rs:2279`).
- `ThreadCommand::SetBotInput(Option<Input>)` and
  `ThreadedSuperShuckieCore::set_bot_input`, which also calls `wake()`.

Timed presses (`press_for_frames`) need no change; they are already a separate
layer.

### 2. Real-time mode (routes only)

| Route | Parameters | Effect |
|---|---|---|
| `/input` | `buttons`, `touch`, `circle`, `cstick` | Replace the bot's held state. No parameters releases everything. |
| `/press` | `buttons`, `touch`, `frames` (default 4) | `press_for_frames`: hold for exactly that many frames, then release. |

- `buttons` is a comma list of `a b x y l r zl zr start select up down left
  right`. `touch=x,y` is in bottom-screen pixels; `circle=x,y` and
  `cstick=x,y` are `-127..=127`. Unknown names are a 400.
- Both reply `{"frame": <total frames at the time the input was queued>}`.
- Both are refused (409) while a replay is playing back, a video export is
  running, or no game is loaded.
- Neither unpauses the game or stops playback; the bot uses `/set-paused` for
  that. The keyboard's auto-unpause setting is unaffected.

### 3. Lockstep mode

**Route:** `/step?frames=N&buttons=...` (same input parameters as `/input`).

- `frames` defaults to 1, maximum 3600.
- If input parameters are given they replace the bot's held state first, and
  stay held after the step. Omitted means "keep what is held".
- If the game is running, the step pauses it first. Leaving lockstep is
  `/set-paused?paused=false`. This is what makes the two modes
  interchangeable: no mode flag, just "is it paused".
- Reply once the frames have run:
  `{"frame": u64, "frames_run": u64, "was_running": bool}`. `was_running`
  tells the bot that something (a person pressing a key with auto-unpause on)
  had resumed the game since its last step, so determinism was broken.
- 409 when it cannot step: replay playing back or stalled, following another
  player, linked or held for a link cable, video export running, no game.
  The link check is mandatory, not cosmetic: `do_run_fn` does nothing while
  linked, so an unguarded step loop would never finish.

**Core thread:** stepping is loop state, not a blocking command, so the thread
keeps handling commands (reads, cancel, close) between frames.

- `ThreadCommand::StepFrames { input: Option<Option<Input>>, frames: u64,
  reply: Box<dyn FnOnce(StepOutcome) + Send> }`.
- `CoreLoop` gains `pending_step: Option<PendingStep>` (`frames_left`,
  `frames_run`, `reply`). The handler validates, pauses if running (same code
  as `ThreadCommand::Pause`), calls `finish_current_frame` if the Game Boy
  core is mid-frame so the new input is not skipped by the mid-frame check in
  `update_input`, applies the bot input, and stores the step.
- `run_thread`: in the paused branch, before parking, if `pending_step` is
  set call a new `CoreLoop::run_one_step`, which loops `run_unlocked` /
  `run_unlocked_hidden` until `last_run_time().frames > 0`, updates
  `emulated_frames` and counters, and decrements `frames_left`. Only the last
  frame, plus `draw_lead_frames()` before it, is drawn. When `frames_left`
  reaches 0, `housekeeping()` publishes the screen and runs Poke-A-Byte and
  the memory monitor, then `reply` is called.
- A second `StepFrames` while one is pending is refused (409). `Start`,
  `LoadSaveState`, `HardReset`, attaching a replay, and `Close` cancel the
  pending step; its reply reports the frames actually run and
  `cancelled: true`.
- Poke-A-Byte freezes and shared-memory reads must still happen once per
  stepped frame (`handle_pokeabyte_integration`), not once per step, or
  frozen values drift during long steps.

**Clock:** stepped frames need timestamps, and the paused clock is frozen.

- New `SuperShuckieCore::advance_paused_timer(micros)`: adds to
  `paused_timer_at`, carrying the sub-millisecond remainder in a new field.
- `run_one_step` calls it with `nominal_frame_micros()` (`lib.rs:1085`) before
  each frame. A replay recorded in lockstep then plays back at normal speed,
  however long the bot spent thinking. This is the same idea the link code
  uses for the lent partner's clock.
- `unpause_timer` already rebases from `paused_timer_at`, so switching back to
  real-time mode stays monotone.

**Audio:** stepped frames are silent (`run_unlocked` drops samples). An
`audible` option can be added later via `run_unlocked_audible`.

### 4. Frontend and webserver plumbing

- `supershuckie-frontend-webserver/src/lib.rs`: new
  `SuperShuckieServerCommand::{BotInput, BotPress, Step}` variants carrying a
  plain `BotInputParams` struct (the crate does not depend on the core, so it
  cannot carry `Input`), a shared `parse_bot_input` function with unit tests
  next to the bookmark-route tests, and the three routes.
- `supershuckie-frontend/src/lib.rs` `tick()`: convert `BotInputParams` to
  `Input`, apply the refusal rules, and forward to the core. For `Step`, the
  frontend does not wait: it passes the core a closure that converts
  `StepOutcome` to JSON and sends it on the webserver's reply channel, so the
  core thread answers the HTTP thread directly and the UI thread never blocks.
- Bot input is cleared on ROM load/unload and when external commands are
  switched off, so a crashed bot cannot leave a button held across games.
- `js/client.js` and `client.d.ts`: `input()`, `press()`, `step()`.
- `docs/external_commands.md`: the three routes plus a short "writing a bot"
  section explaining the two modes.

No C API or Qt change is required. Existing settings gate it (the "external
commands" toggle).

### 5. Reading the game (needed for a useful bot)

Poke-A-Byte already exposes memory, but a lockstep bot wants state in the same
round trip.

- `/read-memory?address=..&length=..` using the existing
  `ThreadedSuperShuckieCore::read_ram` (`thread.rs:1055`). Check that it wakes
  the paused thread; if not, replies take up to 100 ms.
- `/step` accepts `read=addr:len,addr:len`, returned as hex in the reply, read
  after the last frame.
- `/screenshot` (PNG of the current screens via `read_screens`) is optional
  and last. I have not checked whether a PNG encoder is already a dependency;
  if not, the attribution script must cover the new one.

## Order of work

1. Core: bot input layer, `advance_paused_timer`, tests.
2. Core thread: `SetBotInput`, `StepFrames`, `run_one_step`, cancel rules,
   tests.
3. Webserver: params, parsing, routes, tests.
4. Frontend: `tick()` handlers, clearing rules.
5. `/read-memory` and `read=` on `/step`.
6. Client JS, docs.
7. Headless smoke example and measurements.
8. Optional: `/screenshot`.

Steps 1, 3 and the `/input` + `/press` half of 4 deliver real-time mode on
their own.

## Tests

Core unit tests (the mock cores at `lib.rs:2927` and `lib.rs:3082` are enough):

- Bot input ORs with keyboard input; a key event does not clear it and
  `set_bot_input(None)` does not clear the keyboard.
- A step of N advances `total_frames` by exactly N, and the input the core saw
  on each of those frames is the step's input.
- Record a replay in lockstep, play it back, and compare final state and
  frame count. Recorded timestamps advance by the nominal frame period.
- Step issued mid-frame (Game Boy mock): the partial frame finishes first and
  the input applies to all N counted frames.
- Step while linked, playing back, or following is refused and returns.
- `Start` during a pending step cancels it with the right `frames_run`.

Webserver unit tests: parameter parsing, bad button names, range limits.

Headless end-to-end: `supershuckie-frontend/examples/bot_input_smoke.rs`, in
the style of `play_together_smoke.rs`: load a ROM, enable external commands,
drive `/step` and `/press` over HTTP, assert frame counts and a memory value.
No synthetic keyboard or mouse input.

## Risks and open questions

- **Round-trip cost.** Each `/step` crosses the HTTP poll thread (1 ms sleep
  between polls), the UI tick (1 ms timer), and the core thread wake. I expect
  a few milliseconds per call, so single-frame stepping should still beat
  real time on the handheld cores, but this is unmeasured. If it is too slow
  the fixes are larger `frames` per step, or a persistent socket later.
- **3DS step time.** 3600 frames of the 3DS core may exceed the 60 s reply
  timeout. Either lower the cap per console or reply early with the frames
  run so far.
- **Timestamps when recording.** Advancing the paused clock is new behaviour
  for the recorder; the monotone-timestamp regression test at `lib.rs:3196`
  should be extended to cover it.
- **Play Together.** Stepped frames are published like any other, but
  followers pace on arrival, so a slow bot looks like a stalling stream. Not
  addressed here.
- **Not included:** scheduling a real-time input for a specific future frame,
  save/load state routes, audio during steps. Each is a small follow-up once
  this is in.

## Implementation notes (2026-10-01)

What was built differs from the plan above in these ways:

- **Own setting, off by default.** Instead of relying only on the external-commands toggle (which
  is on by default), every bot route needs **Settings › Allow bot control**
  (`settings.json` → `bot_control.enabled`, default `false`). It is greyed out while external
  commands are off. Turning it off releases the bot's input and cancels a running step. This
  needed a C API pair (`supershuckie_frontend_get/set_bot_control_enabled`) and a Qt menu action
  (`allow-bot-control`). While it is off, the routes answer `403`.
- **All five routes:** `/input`, `/press`, `/step`, `/read-memory` and `/screenshot`.
  `/screenshot` uses a small built-in PNG writer (stored deflate, no new dependency, so the
  attribution script is unaffected).
- **`/step`:** `frames` may be `0` (pause, set input, read). The reply also carries
  `cancelled` / `cancel_reason`. A step that runs past 50 s answers with the frames run so far,
  which covers the 3DS timeout risk. Steps are also cancelled by lending the core, starting a link
  hold, and following another player.
- **Core API:** `SuperShuckieCore::{set_bot_input, bot_input, can_step, run_stepped_frame,
  advance_paused_timer, read_memory}`. `ThreadedSuperShuckieCore::{set_bot_input, step_frames,
  cancel_step}`. `StepOutcome`, `StepReply`, `MAX_STEP_FRAMES`, `STEP_TIME_LIMIT`.
  `read_ram` and `press_for_frames` now wake a paused thread.
- **Frontend:** handlers live in `supershuckie-frontend/src/bot.rs`.

Verified:

- Core: 9 new tests (layering, exact steps, lockstep replay timestamps plus playback,
  mid-frame Game Boy, refusals, timer carry, and three thread-level tests including cancel by
  Start).
- Webserver: 2 new parsing tests. Frontend: 3 new tests (PNG, screen stacking, step JSON).
- Workspace total: 325 tests passed, 0 failed.
- `examples/bot_input_smoke.rs` passes on GBC (pokered), GBA (Emerald) and DS (Platinum).
- Round trip per single-frame `/step`: about 2.8 ms (GBC), 4.4 ms (GBA), 5.1 ms (DS).
- A 60-frame step takes about 50–60 ms; the core needs about 1 ms per stepped frame.
- The Qt app was run headless (offscreen) to check the menu, the 403 → 200 switch, greying out,
  and the setting persisting across a relaunch.
- Not verified on the 3DS: this Mac has no Azahar sources, so the builds used a scratch mirror
  with a stubbed `azahar-rs/interface.cpp`.

