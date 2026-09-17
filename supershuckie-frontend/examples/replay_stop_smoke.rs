//! End-to-end check of stopping and resuming a replay through the frontend and a real threaded
//! core: a stopped replay stays loaded (its position and length are still reported, seeking still
//! works) while the game runs on live, the resume point follows seeks and not the live frame
//! counter, jumping back to it rewinds live play without resuming, resuming puts the emulator back
//! there, the pause state is left alone, and closing a stopped replay remembers its resume point
//! for "continue last replay".
//!
//! ```text
//! replay_stop_smoke <rom.gbc | rom.gba>
//! ```
//!
//! Link it like `supershuckie-core`'s `bookmark_smoke` (see that file's header).

use std::num::NonZeroU8;
use std::time::{Duration, Instant};

use supershuckie_core::emulator::ScreenData;
use supershuckie_frontend::{ScreenInfo, SuperShuckieFrontend, SuperShuckieFrontendCallbacks, SuperShuckieReplayState};

struct NoScreen;

impl SuperShuckieFrontendCallbacks for NoScreen {
    fn refresh_screens(&mut self, _: &[ScreenData]) {}
    fn change_video_mode(&mut self, _: &[ScreenInfo], _: NonZeroU8) {}
}

fn tick_for(frontend: &mut SuperShuckieFrontend, duration: Duration) {
    let until = Instant::now() + duration;
    while Instant::now() < until {
        frontend.tick().expect("tick");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn tick_until(frontend: &mut SuperShuckieFrontend, what: &str, mut done: impl FnMut(&mut SuperShuckieFrontend) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done(frontend) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        frontend.tick().expect("tick");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn main() {
    let rom = std::path::absolute(std::env::args().nth(1).expect("usage: replay_stop_smoke <rom>")).unwrap();

    let dir = std::env::temp_dir().join("supershuckie-frontend-replay-stop-smoke");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut frontend = SuperShuckieFrontend::new(dir.join("data"), dir.join("config"), Box::new(NoScreen));
    frontend.load_rom(&rom).expect("load rom");
    frontend.set_auto_pause_on_record_setting(false);
    frontend.set_paused(false);
    tick_for(&mut frontend, Duration::from_millis(300));

    // Nothing loaded: stopping/resuming/jumping back are no-ops.
    frontend.stop_replay_playback();
    assert!(!frontend.is_replay_playback_stopped());
    frontend.resume_replay_playback().expect("resume without a replay is a no-op");
    frontend.go_to_replay_resume_point();

    // --- Record something to play back. ---
    let replay_name = frontend.start_recording_replay(Some("stop-smoke")).expect("record").to_string();
    frontend.set_paused(false);
    tick_until(&mut frontend, "300 recorded frames", |f| f.get_elapsed_frames() >= 300);
    frontend.stop_recording_replay().expect("stop recording");
    let replay_stem = replay_name.rsplit_once('.').map(|(s, _)| s.to_owned()).unwrap_or(replay_name);

    // --- Play it back. ---
    assert!(frontend.load_replay_if_exists(&replay_stem, false).expect("load replay"));
    let total = frontend.get_replay_playback_stats().expect("loaded").total_frames;
    assert!(total >= 300, "recorded {total} frames");
    assert_eq!(frontend.get_replay_state(), SuperShuckieReplayState::Playback);
    assert!(!frontend.is_replay_playback_stopped());
    frontend.set_paused(false);
    tick_until(&mut frontend, "playback to reach frame 60", |f| f.get_replay_frame() >= 60);
    assert_eq!(frontend.get_replay_frame(), frontend.get_elapsed_frames(), "while playing, the replay position is the frame counter");

    // --- Stop: the replay stays loaded, the game runs on live. ---
    frontend.stop_replay_playback();
    assert!(frontend.is_replay_playback_stopped());
    assert_eq!(frontend.get_replay_state(), SuperShuckieReplayState::Playback, "a stopped replay is still loaded");
    assert_eq!(frontend.get_replay_playback_stats().map(|s| s.total_frames), Some(total), "...and its length is still reported");
    assert!(!frontend.is_paused(), "stopping does not pause the game");
    // The stop is blocking, so the position it reports is exactly where playback was cut.
    frontend.tick().expect("tick");
    let resume_at = frontend.get_replay_frame();
    let frames_at_stop = frontend.get_elapsed_frames();
    assert!(resume_at >= 60 && resume_at <= frames_at_stop, "resume point {resume_at}, frame counter {frames_at_stop}");
    println!("stopped at frame {resume_at}");

    tick_until(&mut frontend, "live frames after stopping", |f| f.get_elapsed_frames() >= frames_at_stop + 30);
    assert_eq!(frontend.get_replay_frame(), resume_at, "the resume point does not follow live play");
    assert!(frontend.get_elapsed_milliseconds() > 0);

    // Live means the user's things work again: a save state can be loaded.
    let state_name = frontend.create_save_state(Some("stopped")).expect("save state").to_string();
    let state_stem = state_name.rsplit_once('.').map(|(s, _)| s.to_owned()).unwrap_or(state_name);
    assert!(frontend.load_save_state_if_exists(&state_stem).expect("save states are loadable while stopped"));

    // --- Seek while stopped: the resume point moves, the user stays in control. ---
    frontend.go_to_replay_frame(20);
    tick_until(&mut frontend, "the seek to frame 20", |f| f.get_replay_frame() == 20);
    assert!(frontend.is_replay_playback_stopped(), "a seek does not resume playback");
    let after_seek = frontend.get_elapsed_frames();
    assert!(after_seek >= 20 && after_seek < 30, "the frame counter restarts from the seek target ({after_seek})");
    tick_until(&mut frontend, "live frames after seeking", |f| f.get_elapsed_frames() >= 20 + 30);
    assert_eq!(frontend.get_replay_frame(), 20, "the seek target is the new resume point");
    println!("seeked to frame 20 while stopped, then ran on live to {}", frontend.get_elapsed_frames());

    // --- Jump back to the resume point: live play rewinds, still stopped, still in control. ---
    frontend.go_to_replay_resume_point();
    tick_until(&mut frontend, "the jump back to frame 20", |f| f.get_elapsed_frames() < 30);
    assert!(frontend.is_replay_playback_stopped(), "jumping back does not resume playback");
    assert_eq!(frontend.get_replay_frame(), 20, "the resume point stays put");
    assert!(frontend.get_elapsed_frames() >= 20);
    tick_until(&mut frontend, "live frames after jumping back", |f| f.get_elapsed_frames() >= 20 + 30);
    assert!(frontend.is_replay_playback_stopped());
    assert_eq!(frontend.get_replay_frame(), 20);
    println!("jumped back to frame 20, then ran on live to {}", frontend.get_elapsed_frames());

    // --- Resume: back at the resume point, playing, pause state untouched. ---
    frontend.resume_replay_playback().expect("resume");
    assert!(!frontend.is_replay_playback_stopped());
    assert!(!frontend.is_paused());
    assert!(frontend.get_replay_frame() >= 20 && frontend.get_replay_frame() < 25, "resumed at {}", frontend.get_replay_frame());
    tick_until(&mut frontend, "playback after resuming", |f| f.get_replay_frame() >= 40);
    assert_eq!(frontend.get_replay_frame(), frontend.get_elapsed_frames());
    println!("resumed at frame 20 and played on to {}", frontend.get_replay_frame());

    // --- Stop and resume while paused: nothing moves, and it stays paused. ---
    frontend.set_paused(true);
    // Let the last frame's stats land before reading them.
    tick_for(&mut frontend, Duration::from_millis(50));
    let paused_at = frontend.get_replay_frame();
    frontend.stop_replay_playback();
    assert!(frontend.is_replay_playback_stopped() && frontend.is_paused());
    tick_for(&mut frontend, Duration::from_millis(100));
    assert_eq!(frontend.get_elapsed_frames(), paused_at, "a paused game stays put after stopping");
    assert_eq!(frontend.get_replay_frame(), paused_at);
    frontend.resume_replay_playback().expect("resume while paused");
    assert!(!frontend.is_replay_playback_stopped() && frontend.is_paused(), "resuming leaves the pause state alone");
    tick_for(&mut frontend, Duration::from_millis(100));
    assert_eq!(frontend.get_replay_frame(), paused_at);
    frontend.set_paused(false);
    tick_until(&mut frontend, "playback after unpausing", |f| f.get_replay_frame() >= paused_at + 10);
    println!("stop/resume while paused ok");

    // --- Close a stopped replay: "continue last replay" goes back to its resume point. ---
    frontend.stop_replay_playback();
    frontend.tick().expect("tick");
    let resume_at = frontend.get_replay_frame();
    tick_until(&mut frontend, "live frames before closing", |f| f.get_elapsed_frames() >= resume_at + 30);
    frontend.close_replay();
    assert_eq!(frontend.get_replay_state(), SuperShuckieReplayState::NoReplay);
    assert!(!frontend.is_replay_playback_stopped());
    assert!(frontend.can_continue_last_replay());
    assert!(frontend.continue_last_replay().expect("continue last replay"));
    assert_eq!(frontend.get_replay_state(), SuperShuckieReplayState::Playback);
    assert!(!frontend.is_replay_playback_stopped());
    assert!(frontend.is_paused(), "continuing reopens the replay paused");
    // The seek back is fire-and-forget on the core thread; it is paused, so the position settles.
    tick_until(&mut frontend, "continue last replay to seek", |f| f.get_replay_frame() >= resume_at);
    let continued_at = frontend.get_replay_frame();
    assert!(continued_at >= resume_at && continued_at <= resume_at + 1, "continued at {continued_at}, expected the resume point {resume_at}");
    println!("closed while stopped at {resume_at}; continue last replay reopened it at {continued_at}");

    frontend.close_replay();
    frontend.unload_rom();
    println!("all replay stop/resume checks passed");
    if std::env::var_os("SUPERSHUCKIE_SMOKE_KEEP").is_some() {
        println!("kept {}", dir.display());
    }
    else {
        let _ = std::fs::remove_dir_all(&dir);
    }
}
