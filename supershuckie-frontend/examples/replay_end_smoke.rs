//! The end of a replay through the frontend and a real threaded core, driven the way the REST API
//! clients (the queuer and the stream overlay) drive it: load a replay file as `/load-replay`
//! does, seek to / past its last frame as `/go-to-frame` does, unpause as `/set-paused` does and
//! mark the run's start/end as `/mark-start` and `/mark-end` do -- none of which may kill the
//! core thread ("The emulator thread has stopped; the ROM was unloaded" from `tick()`).
//!
//! ```text
//! replay_end_smoke <rom> <file.replay> <scenario> [settings.json to seed]
//! ```
//!
//! Scenarios: `last` (seek to frames-1, then unpause), `past` (seek to frames), `far` (seek to
//! frames+5000), `play` (unpause from frame 0 and run to the end), `margin` (seek to frames-600,
//! then play), `mark` (seek to the last frame, `/mark-end`, unpause, `/mark-start`: both are
//! refused while playing back, and a refusal must not read as the thread having died).
//!
//! Exits non-zero if the core died.

use std::num::NonZeroU8;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use supershuckie_core::emulator::ScreenData;
use supershuckie_frontend::{ScreenInfo, SuperShuckieFrontend, SuperShuckieFrontendCallbacks};

struct NoScreen;

impl SuperShuckieFrontendCallbacks for NoScreen {
    fn refresh_screens(&mut self, _: &[ScreenData]) {}
    fn change_video_mode(&mut self, _: &[ScreenInfo], _: NonZeroU8) {}
}

/// Tick like the app's UI loop does, printing anything tick reports; `true` if the core died.
fn tick_for(frontend: &mut SuperShuckieFrontend, duration: Duration, label: &str) -> bool {
    let until = Instant::now() + duration;
    let mut died = false;
    while Instant::now() < until {
        if let Err(e) = frontend.tick() {
            println!("[{label}] tick error: {e}");
            if e.to_string().contains("emulator thread has stopped") {
                died = true;
            }
        }
        std::thread::sleep(Duration::from_millis(4));
    }
    died
}

fn wait_settled(frontend: &mut SuperShuckieFrontend, label: &str, timeout: Duration) -> (u32, bool) {
    let started = Instant::now();
    let mut last = frontend.get_elapsed_frames();
    let mut stable = 0;
    let mut died = false;
    while started.elapsed() < timeout {
        died |= tick_for(frontend, Duration::from_millis(250), label);
        let now = frontend.get_elapsed_frames();
        if now == last {
            stable += 1;
            if stable >= 3 {
                break;
            }
        }
        else {
            stable = 0;
        }
        last = now;
    }
    (frontend.get_elapsed_frames(), died)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let rom = std::path::absolute(args.next().expect("rom")).unwrap();
    let replay = std::path::absolute(args.next().expect("replay")).unwrap();
    let scenario = args.next().expect("scenario");
    let seed_settings = args.next().map(PathBuf::from);

    let dir = std::env::temp_dir().join("supershuckie-frontend-replay-end-smoke");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // Data and config share one dir, as the app's UserData does.
    let user = dir.join("UserData");
    let rom_file_name = rom.file_name().unwrap().to_string_lossy().into_owned();
    let replays_dir = user.join(format!("{rom_file_name}-data")).join("replays");
    std::fs::create_dir_all(&replays_dir).unwrap();
    let replay_stem = replay.file_stem().unwrap().to_string_lossy().into_owned();
    std::fs::copy(&replay, replays_dir.join(format!("{replay_stem}.replay"))).unwrap();
    if let Some(seed) = seed_settings {
        std::fs::copy(seed, user.join("settings.json")).unwrap();
    }

    let mut frontend = SuperShuckieFrontend::new(user.clone(), user.clone(), Box::new(NoScreen));
    frontend.load_rom(&rom).expect("load rom");
    tick_for(&mut frontend, Duration::from_millis(300), "after load_rom");

    // /load-replay
    let loaded = frontend.load_replay_if_exists(&replay_stem, true).expect("load replay");
    assert!(loaded, "replay not found");
    let total = frontend.get_replay_playback_stats().expect("loaded").total_frames;
    println!("loaded {replay_stem}: {total} frames, paused={}", frontend.is_paused());
    let (f, died) = wait_settled(&mut frontend, "after load_replay", Duration::from_secs(10));
    println!("settled at frame {f} (died={died})");

    let seek_to = |frontend: &mut SuperShuckieFrontend, target: u32| {
        println!("/go-to-frame?frame={target}");
        frontend.go_to_replay_frame(target);
        let (f, died) = wait_settled(frontend, "seek", Duration::from_secs(600));
        println!("seek landed at frame {f}, replay frame {}, died={died}, finished={}", frontend.get_replay_frame(), frontend.get_replay_playback_stats().map(|s| s.total_frames).unwrap_or(0));
        died
    };

    let play = |frontend: &mut SuperShuckieFrontend, secs: u64| {
        println!("/set-paused?paused=false");
        frontend.set_paused(false);
        let mut died = false;
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(secs) {
            died |= tick_for(frontend, Duration::from_millis(500), "play");
            println!("  frame {} paused={} state={:?}", frontend.get_elapsed_frames(), frontend.is_paused(), frontend.get_replay_state());
            if died {
                break;
            }
        }
        died
    };

    let died = match scenario.as_str() {
        "last" => {
            let d = seek_to(&mut frontend, total.saturating_sub(1));
            d | play(&mut frontend, 4)
        }
        "past" => {
            let d = seek_to(&mut frontend, total);
            d | play(&mut frontend, 4)
        }
        "far" => {
            let d = seek_to(&mut frontend, total + 5000);
            d | play(&mut frontend, 4)
        }
        "play" => play(&mut frontend, 600),
        // What the overlay does through the REST API: `/mark-end` when it sees the run finish
        // (the replay's last frames) and `/mark-start` when the run begins after unpausing.
        // Neither is recording, so both are refused -- and that refusal must not read as death.
        "mark" => {
            let mut d = seek_to(&mut frontend, total.saturating_sub(1));
            let r = frontend.mark_replay_end();
            println!("mark_replay_end while playing back -> {r:?}");
            d |= tick_for(&mut frontend, Duration::from_millis(500), "after mark_replay_end");
            println!("  game_running={} state={:?}", frontend.is_game_running(), frontend.get_replay_state());
            frontend.set_paused(false);
            let r = frontend.mark_replay_start(supershuckie_replay_recorder::TimestampMillis(0));
            println!("mark_replay_start while playing back -> {r:?}");
            d |= tick_for(&mut frontend, Duration::from_millis(500), "after mark_replay_start");
            println!("  game_running={} state={:?}", frontend.is_game_running(), frontend.get_replay_state());
            d
        }
        "margin" => {
            let d = seek_to(&mut frontend, total.saturating_sub(600));
            d | play(&mut frontend, 60)
        }
        other => panic!("unknown scenario {other}")
    };

    println!("RESULT: scenario={scenario} core_died={died} game_running={}", frontend.is_game_running());
    if died || !frontend.is_game_running() {
        std::process::exit(1);
    }
}
