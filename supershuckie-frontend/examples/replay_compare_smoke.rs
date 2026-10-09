//! End-to-end check of compared replays through the frontend and real threaded cores: replays
//! with different start markers, timer offsets and none at all are recorded, one is played and
//! the others are compared with it; their timers must read what the main replay's does while it
//! plays, while paused, after seeks backwards and forwards, while stopped and after resuming, and
//! the ones still going must play on once the main replay has ended. Closing the main replay
//! closes the compared ones.
//!
//! ```text
//! replay_compare_smoke <rom.gb | rom.gbc | rom.gba | rom.nds> [--speed <x>]
//! ```
//!
//! Exits non-zero on the first check that fails.

use std::num::NonZeroU8;
use std::time::{Duration, Instant};

use supershuckie_core::emulator::ScreenData;
use supershuckie_frontend::replay_compare::{CompareId, ReplayCompareStateView};
use supershuckie_frontend::{ScreenInfo, SuperShuckieFrontend, SuperShuckieFrontendCallbacks};
use supershuckie_replay_recorder::TimestampMillis;

#[derive(Default)]
struct Screens {
    compare_frames: u64
}

struct Callbacks(std::rc::Rc<std::cell::RefCell<Screens>>);

impl SuperShuckieFrontendCallbacks for Callbacks {
    fn refresh_screens(&mut self, _: &[ScreenData]) {}
    fn change_video_mode(&mut self, _: &[ScreenInfo], _: NonZeroU8) {}
    fn compare_refresh_screens(&mut self, _: CompareId, _: &[ScreenData]) {
        self.0.borrow_mut().compare_frames += 1;
    }
}

fn tick_for(frontend: &mut SuperShuckieFrontend, duration: Duration) {
    let until = Instant::now() + duration;
    while Instant::now() < until {
        frontend.tick().expect("tick");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn tick_until(frontend: &mut SuperShuckieFrontend, what: &str, mut done: impl FnMut(&mut SuperShuckieFrontend) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !done(frontend) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        frontend.tick().expect("tick");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Record `frames` frames, marking the timer's start (with `offset` ms already on it) and end at
/// the given frames.
fn record(frontend: &mut SuperShuckieFrontend, name: &str, frames: u32, start: Option<(u32, u64)>, end: Option<u32>) -> String {
    let file = frontend.start_recording_replay(Some(name)).expect("record").to_string();
    frontend.set_paused(false);
    // The frame counter restarts with the recording; wait until the cached one has caught up.
    tick_until(frontend, "the frame counter to restart", |f| f.get_elapsed_frames() < 60);
    let mut started = start.is_none();
    let mut ended = end.is_none();
    tick_until(frontend, "recording", |f| {
        let at = f.get_elapsed_frames();
        if !started && let Some((frame, offset)) = start && at >= frame {
            f.mark_replay_start(TimestampMillis(offset)).expect("mark start");
            started = true;
        }
        if !ended && let Some(frame) = end && at >= frame {
            f.mark_replay_end().expect("mark end");
            ended = true;
        }
        at >= frames
    });
    frontend.stop_recording_replay().expect("stop recording");
    frontend.set_paused(true);
    file.rsplit_once('.').map(|(s, _)| s.to_owned()).unwrap_or(file)
}

fn state(frontend: &SuperShuckieFrontend) -> ReplayCompareStateView {
    frontend.replay_compare_state()
}

/// Largest gap between the main timer and each compared replay's timer over `duration`, ticking.
/// Gaps are only counted for replays whose line-up target lies inside them (in sync).
fn max_gap_while(frontend: &mut SuperShuckieFrontend, duration: Duration) -> i64 {
    let until = Instant::now() + duration;
    let mut worst = 0i64;
    while Instant::now() < until {
        frontend.tick().expect("tick");
        let s = state(frontend);
        for r in &s.replays {
            if r.status == "in_sync" {
                worst = worst.max((r.timer_ms - s.main_timer_ms).abs());
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    worst
}

fn settle(frontend: &mut SuperShuckieFrontend) {
    tick_for(frontend, Duration::from_millis(600));
}

/// After settling while paused: every in-sync compared replay sits on the first frame at or after
/// the main timer's reading (one frame at most ahead; frames recorded sped up are a few
/// milliseconds of replay time on average but up to a refresh apart, so a frame is allowed 17 ms
/// more than the average).
fn check_settled(frontend: &mut SuperShuckieFrontend, what: &str, frame_ms: i64) {
    settle(frontend);
    let s = state(frontend);
    for r in &s.replays {
        if r.status != "in_sync" {
            println!("  {what}: {} is {} (timer {} vs main {})", r.name, r.status, r.timer_ms, s.main_timer_ms);
            continue
        }
        let gap = r.timer_ms - s.main_timer_ms;
        println!("  {what}: {} timer {} ms, main {} ms, gap {gap} ms", r.name, r.timer_ms, s.main_timer_ms);
        assert!(gap >= 0 && gap <= frame_ms + 17, "{what}: {} is {gap} ms from the main timer", r.name);
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let rom = std::path::absolute(args.next().expect("usage: replay_compare_smoke <rom> [--speed x]")).unwrap();
    let mut speed = 4.0;
    while let Some(arg) = args.next() {
        if arg == "--speed" {
            speed = args.next().and_then(|s| s.parse().ok()).expect("--speed <x>");
        }
    }

    let dir = std::env::temp_dir().join("supershuckie-frontend-replay-compare-smoke");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let screens = std::rc::Rc::new(std::cell::RefCell::new(Screens::default()));
    let mut frontend = SuperShuckieFrontend::new(dir.join("data"), dir.join("config"), Box::new(Callbacks(screens.clone())));
    frontend.load_rom(&rom).expect("load rom");
    frontend.set_auto_pause_on_record_setting(false);
    frontend.set_speed_settings(speed, speed);
    frontend.set_paused(false);
    tick_for(&mut frontend, Duration::from_millis(200));

    // Nothing to compare against yet.
    let not_yet = frontend.replay_compare_add(&dir.join("nothing.replay"), None, false);
    assert!(not_yet.is_err(), "comparing without a main replay must fail");

    // A: timer from frame 120 to 540 of 600. B: timer from frame 300 (already at 1 s) on, 900
    // frames. C: no timer, 400 frames.
    let a = record(&mut frontend, "compare-a", 600, Some((120, 0)), Some(540));
    let b = record(&mut frontend, "compare-b", 900, Some((300, 1_000)), None);
    let c = record(&mut frontend, "compare-c", 400, None, None);

    assert!(frontend.load_replay_if_exists(&a, false).expect("load a"));
    frontend.set_paused(true);
    let b_path = frontend.replay_path_for_name(&b).expect("path");
    let c_path = frontend.replay_path_for_name(&c).expect("path");
    let b_id = frontend.replay_compare_add(&b_path, None, false).map_err(|e| e.message()).expect("add b");
    let c_id = frontend.replay_compare_add(&c_path, None, false).map_err(|e| e.message()).expect("add c");
    assert_ne!(b_id, c_id);
    assert_eq!(frontend.replay_compare_count(), 2);

    let s = state(&frontend);
    assert!(s.main_has_timer, "A has a timer");
    let b_view = s.replays.iter().find(|r| r.id == b_id).unwrap();
    let c_view = s.replays.iter().find(|r| r.id == c_id).unwrap();
    assert!(b_view.has_timer && !c_view.has_timer);
    let a_final = s.main_final_ms.expect("A's timer was stopped");
    println!("A final {a_final} ms, B total {} ms, C total {} ms", b_view.total_ms, c_view.total_ms);

    // Nominal frame length in replay time (the replays were recorded at `speed`).
    let total = frontend.get_replay_playback_stats().expect("loaded");
    let frame_ms = (total.total_milliseconds as i64 / total.total_frames.max(1) as i64).max(1) + 1;
    println!("A: {} frames, {} ms ({frame_ms} ms/frame)", total.total_frames, total.total_milliseconds);

    // Paused at the start (before A's timer starts): B sits before its own start marker.
    check_settled(&mut frontend, "paused at the start", frame_ms);

    // Playing.
    frontend.set_paused(false);
    tick_until(&mut frontend, "A's timer to start", |f| f.replay_compare_state().main_timer_ms > 200);
    let gap = max_gap_while(&mut frontend, Duration::from_millis(800));
    println!("playing: worst gap {gap} ms");
    assert!(gap <= frame_ms * 4, "compared replays drifted {gap} ms while playing");

    // Paused mid-way.
    frontend.set_paused(true);
    check_settled(&mut frontend, "paused mid-way", frame_ms);

    // Seeks: back before A's timer starts, forward, back again, one frame back.
    frontend.go_to_replay_frame(60);
    check_settled(&mut frontend, "seek to 60", frame_ms);
    frontend.go_to_replay_frame(480);
    check_settled(&mut frontend, "seek to 480", frame_ms);
    frontend.go_to_replay_frame(200);
    check_settled(&mut frontend, "seek back to 200", frame_ms);
    frontend.advance_playback_frames(-1);
    check_settled(&mut frontend, "one frame back", frame_ms);
    frontend.advance_playback_frames(1);
    check_settled(&mut frontend, "one frame on", frame_ms);

    // Stopped: the user plays on live, the compared replays hold at the resume point.
    frontend.stop_replay_playback();
    frontend.set_paused(false);
    let held = state(&frontend).replays.iter().find(|r| r.id == b_id).unwrap().replay_ms;
    tick_for(&mut frontend, Duration::from_millis(500));
    let still = state(&frontend).replays.iter().find(|r| r.id == b_id).unwrap().replay_ms;
    println!("stopped: B held at {held} -> {still} ms");
    assert_eq!(held, still, "B moved while the main replay was stopped");
    frontend.set_paused(true);
    frontend.resume_replay_playback().expect("resume");
    check_settled(&mut frontend, "resumed (paused)", frame_ms);

    // Play to A's end: B (longer) plays on; C (shorter) has ended.
    let frames_before = screens.borrow().compare_frames;
    frontend.set_paused(false);
    tick_until(&mut frontend, "A to end", |f| f.replay_compare_state().overrun);
    let at_end = state(&frontend).replays.iter().find(|r| r.id == b_id).unwrap().replay_ms;
    tick_for(&mut frontend, Duration::from_millis(500));
    let s = state(&frontend);
    let later = s.replays.iter().find(|r| r.id == b_id).unwrap();
    println!("overrun: B {at_end} -> {} ms ({}); C {}", later.replay_ms, later.status, s.replays.iter().find(|r| r.id == c_id).unwrap().status);
    assert!(later.replay_ms > at_end || later.status == "ended", "B did not play on after A ended");
    assert_eq!(s.replays.iter().find(|r| r.id == c_id).unwrap().status, "ended");
    assert!(screens.borrow().compare_frames > frames_before, "no compared screens were handed over");

    // Seeking the main replay back ends the overrun and brings everyone back.
    frontend.set_paused(true);
    frontend.go_to_replay_frame(300);
    check_settled(&mut frontend, "back from the end", frame_ms);
    assert!(!state(&frontend).overrun);

    // Removing one, then closing the main replay closes the rest.
    frontend.replay_compare_remove(c_id);
    assert_eq!(frontend.replay_compare_count(), 1);
    frontend.close_replay();
    tick_for(&mut frontend, Duration::from_millis(50));
    assert_eq!(frontend.replay_compare_count(), 0, "closing the main replay closes the compared ones");

    println!("OK");
}
