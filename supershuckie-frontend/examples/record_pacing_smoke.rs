//! Frame rate, pacing smoothness and replay round trip on the real frontend and core thread, the
//! way the app runs them (paced, ticked from a UI loop), with no window: run the game at the
//! given speed with nothing else going on, then again while recording a replay, then play the
//! recording back to its end and report every phase's emulated frames per second, frame-time
//! average and the share of frames that blew their budget. The recorded file is then read back
//! and its packets counted, so a recorder that writes more than about one `ChangeInput` per
//! emulated frame (the 0.4.14 regression) shows up as a number, not a feeling.
//!
//! ```text
//! record_pacing_smoke <rom.nds|rom.gba|rom.gbc> [--speed 4] [--seconds 20]
//! ```
//!
//! Exits non-zero when the recording could not be made, when playback did not reach the end of
//! the recording, or when any phase ran at less than 90% of the requested speed.
//!
//! Link it like `supershuckie-core`'s `nds_bench` (see that file's header).

use std::collections::BTreeMap;
use std::num::NonZeroU8;
use std::time::{Duration, Instant};

use supershuckie_core::emulator::ScreenData;
use supershuckie_frontend::{ScreenInfo, SuperShuckieFrontend, SuperShuckieFrontendCallbacks, SuperShuckieReplayState};
use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::Packet;

struct NoScreen;

impl SuperShuckieFrontendCallbacks for NoScreen {
    fn refresh_screens(&mut self, _: &[ScreenData]) {}
    fn change_video_mode(&mut self, _: &[ScreenInfo], _: NonZeroU8) {}
}

/// Tick like the app's UI loop does (a few times a millisecond is plenty; the app ticks every ms).
fn tick_for(frontend: &mut SuperShuckieFrontend, duration: Duration) {
    let until = Instant::now() + duration;
    while Instant::now() < until {
        if let Err(e) = frontend.tick() {
            println!("tick error: {e}");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

struct Phase {
    name: &'static str,
    frames: u64,
    seconds: f64,
    average_ms: f64,
    max_ms: f64,
    budget_ms: f64,
    over_budget: u64,
    measured: u64,
    /// Wall-clock gaps between consecutive emulated frames that exceeded 2.5x the budget:
    /// (frame index the gap ended on, gap in ms, the core's own time for that frame in ms).
    stalls: Vec<(u64, f64, f64)>,
}

impl Phase {
    fn fps(&self) -> f64 {
        self.frames as f64 / self.seconds
    }

    fn over_budget_percent(&self) -> f64 {
        self.over_budget as f64 * 100.0 / self.measured.max(1) as f64
    }

    fn print(&self) {
        println!(
            "{:<12} {:>8.1} fps  {:>6} frames in {:>5.1} s  avg {:>6.2} ms  max {:>7.2} ms  budget {:>5.2} ms  over budget {:>5.2}% ({}/{})",
            self.name, self.fps(), self.frames, self.seconds, self.average_ms, self.max_ms, self.budget_ms, self.over_budget_percent(), self.over_budget, self.measured
        );
        if !self.stalls.is_empty() {
            println!("    {} stalls (> 2.5x budget between frames); first 40 as frame/gap ms/core ms, delta to previous stall in frames:", self.stalls.len());
            let mut previous: Option<u64> = None;
            for (frame, gap, core) in self.stalls.iter().take(40) {
                let delta = previous.map(|p| format!("{:>+6}", *frame as i64 - p as i64)).unwrap_or_else(|| "      ".into());
                println!("      {frame:>8}  {gap:>7.2} ms  core {core:>6.2} ms  {delta}");
                previous = Some(*frame);
            }
            let mut modulo: BTreeMap<u64, usize> = BTreeMap::new();
            for (frame, _, _) in &self.stalls {
                *modulo.entry(frame % 120).or_default() += 1;
            }
            println!("    stall frame mod 120 histogram (keyframe interval): {modulo:?}");
        }
    }
}

/// Run for `seconds` (or until `done` says to stop) and measure.
fn measure(frontend: &mut SuperShuckieFrontend, name: &'static str, seconds: f64, mut done: impl FnMut(&SuperShuckieFrontend) -> bool) -> Phase {
    tick_for(frontend, Duration::from_millis(50));
    let start_stats = frontend.get_frame_time_stats();
    let start_frames = frontend.get_elapsed_frames() as u64;
    let started = Instant::now();
    let mut max_ms = 0.0f64;
    let mut stalls = Vec::new();
    let budget_us = start_stats.budget_micros as f64;
    let mut last_frames = frontend.get_elapsed_frames() as u64;
    let mut last_advance = Instant::now();
    let mut since_check = Instant::now();
    while started.elapsed().as_secs_f64() < seconds {
        if let Err(e) = frontend.tick() {
            println!("tick error: {e}");
        }
        let now = Instant::now();
        let frames = frontend.get_elapsed_frames() as u64;
        if frames != last_frames {
            let gap = now.duration_since(last_advance).as_secs_f64() * 1000.0;
            let stats = frontend.get_frame_time_stats();
            max_ms = max_ms.max(stats.max_frame_micros as f64 / 1000.0);
            if budget_us > 0.0 && gap * 1000.0 > budget_us * 2.5 * (frames - last_frames) as f64 {
                stalls.push((frames, gap, stats.last_frame_micros as f64 / 1000.0));
            }
            last_frames = frames;
            last_advance = now;
        }
        if since_check.elapsed() > Duration::from_millis(100) {
            since_check = now;
            if done(frontend) {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let seconds = started.elapsed().as_secs_f64();
    let stats = frontend.get_frame_time_stats();
    Phase {
        name,
        frames: (frontend.get_elapsed_frames() as u64).wrapping_sub(start_frames),
        seconds,
        average_ms: stats.average_frame_micros as f64 / 1000.0,
        max_ms,
        budget_ms: stats.budget_micros as f64 / 1000.0,
        over_budget: stats.frames_over_budget.wrapping_sub(start_stats.frames_over_budget),
        measured: stats.frames_measured.wrapping_sub(start_stats.frames_measured),
        stalls,
    }
}

fn count_packets(path: &std::path::Path) -> (u64, BTreeMap<&'static str, u64>, f64, u64) {
    let bytes = std::fs::read(path).expect("read the recording");
    let mut player = ReplayFilePlayer::new(&bytes, false).expect("parse the recording");
    player.set_keyframe_states_wanted(false);
    let total = player.get_total_frames();
    println!("{}: v{}, {} frames, {} keyframes, {} bytes", path.display(), player.get_replay_version(), total, player.all_keyframes().len(), bytes.len());

    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut frames = 0u64;
    let mut change_inputs_this_frame = 0u64;
    let mut max_change_inputs = 0u64;
    while let Some(packet) = player.next_packet().expect("read packet") {
        let name = match packet {
            Packet::NoOp => "NoOp",
            Packet::NextFrame { .. } => "NextFrame",
            Packet::WriteMemory { .. } => "WriteMemory",
            Packet::ChangeInput { .. } => "ChangeInput",
            Packet::ChangeSpeed { .. } => "ChangeSpeed",
            Packet::ResetConsole => "ResetConsole",
            Packet::LoadSaveState { .. } => "LoadSaveState",
            Packet::Bookmark { .. } => "Bookmark",
            Packet::BookmarkTable { .. } => "BookmarkTable",
            Packet::Keyframe { .. } => "Keyframe",
            Packet::DeltaKeyframe { .. } => "DeltaKeyframe",
            Packet::RegionDeltaKeyframe { .. } => "RegionDeltaKeyframe",
            Packet::CompressedBlob { .. } => "CompressedBlob",
            Packet::IncrementCounter { .. } => "IncrementCounter",
            Packet::SerialIn { .. } => "SerialIn",
        };
        *counts.entry(name).or_default() += 1;
        match packet {
            Packet::ChangeInput { .. } => change_inputs_this_frame += 1,
            Packet::NextFrame { .. } => {
                frames += 1;
                max_change_inputs = max_change_inputs.max(change_inputs_this_frame);
                change_inputs_this_frame = 0;
            }
            _ => {}
        }
    }
    let per_frame = counts.get("ChangeInput").copied().unwrap_or(0) as f64 / frames.max(1) as f64;
    (total, counts, per_frame, max_change_inputs)
}

/// The app's UI toolkit asks Windows for 1 ms timer resolution; without it, every sleep the
/// cores' own pacing does (SameBoy sleeps inside `GB_run`) rounds up to the default 15.6 ms.
#[cfg(windows)]
fn request_fine_timer_resolution() {
    #[link(name = "winmm")]
    unsafe extern "system" {
        fn timeBeginPeriod(period: u32) -> u32;
    }
    // SAFETY: plain Win32 call with a constant argument.
    unsafe {
        let _ = timeBeginPeriod(1);
    }
}

#[cfg(not(windows))]
fn request_fine_timer_resolution() {}

fn main() {
    let mut args = std::env::args().skip(1);
    let rom = std::path::absolute(args.next().expect("usage: record_pacing_smoke <rom> [--speed n] [--seconds n] [--coarse-timer]")).unwrap();
    let mut speed = 4.0f64;
    let mut seconds = 20.0f64;
    let mut coarse_timer = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--speed" => speed = args.next().unwrap().parse().unwrap(),
            "--seconds" => seconds = args.next().unwrap().parse().unwrap(),
            // Leave Windows at its default 15.6 ms timer granularity, to see what the pacing does
            // in a process that never asked for 1 ms.
            "--coarse-timer" => coarse_timer = true,
            other => panic!("unexpected {other}"),
        }
    }

    if !coarse_timer {
        request_fine_timer_resolution();
    }
    let dir = std::env::temp_dir().join("supershuckie-record-pacing-smoke");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let user = dir.join("UserData");

    let mut frontend = SuperShuckieFrontend::new(user.clone(), user.clone(), Box::new(NoScreen));
    frontend.set_speed_settings(speed, 2.0);
    frontend.set_auto_pause_on_record_setting(false);
    frontend.load_rom(&rom).expect("load rom");
    frontend.set_paused(false);
    println!("{} at {speed}x, {seconds} s per phase", rom.display());

    // Past the boot sequence, and let the pacing settle.
    tick_for(&mut frontend, Duration::from_secs(5));
    println!("warm: frame {} paused={} running={}", frontend.get_elapsed_frames(), frontend.is_paused(), frontend.is_game_running());

    let mut failed = false;
    let mut phases = Vec::new();

    phases.push(measure(&mut frontend, "live", seconds, |_| false));

    let name = "pacing-smoke";
    match frontend.start_recording_replay(Some(name)) {
        Ok(n) => println!("recording {n}"),
        Err(e) => {
            println!("start_recording_replay failed: {e}");
            std::process::exit(1);
        }
    }
    frontend.set_paused(false);
    assert_eq!(frontend.get_replay_state(), SuperShuckieReplayState::Recording);
    phases.push(measure(&mut frontend, "recording", seconds, |_| false));
    let recorded_frames = frontend.get_elapsed_frames();
    if let Err(e) = frontend.stop_recording_replay() {
        println!("stop_recording_replay failed: {e}");
        failed = true;
    }
    tick_for(&mut frontend, Duration::from_millis(200));
    println!("stopped recording at frame {recorded_frames}");

    phases.push(measure(&mut frontend, "live again", seconds, |_| false));

    let replay_path = frontend.get_replays_dir_for_current_rom().expect("replays dir").join(format!("{name}.replay"));
    let (total, counts, per_frame, max_per_frame) = count_packets(&replay_path);
    for (name, count) in &counts {
        println!("  {name:<20} {count}");
    }
    println!("  ChangeInput per frame: {per_frame:.3} average, {max_per_frame} max");
    if per_frame > 2.0 || max_per_frame > 4 {
        println!("FAIL: the recorder wrote the input more than once per emulated frame");
        failed = true;
    }
    let recorded_frames = recorded_frames as u64;
    if total + 1 < recorded_frames || total > recorded_frames + 1 {
        println!("FAIL: the recording has {total} frames but {recorded_frames} were emulated while recording");
        failed = true;
    }

    // Play it back to the end, as the app does after "load replay".
    let loaded = frontend.load_replay_if_exists(name, true).expect("load replay");
    assert!(loaded, "the recording was not found");
    tick_for(&mut frontend, Duration::from_millis(300));
    let stats = frontend.get_replay_playback_stats().expect("attached");
    println!("playing back {name}: {} frames, paused={}", stats.total_frames, frontend.is_paused());
    frontend.set_paused(false);
    let playback_budget = seconds * 1.5 + 5.0;
    let playback = measure(&mut frontend, "playback", playback_budget, |f| f.is_paused() && f.get_replay_frame() >= stats.total_frames.saturating_sub(1));
    let end_frame = frontend.get_replay_frame();
    println!("playback ended at replay frame {end_frame} of {} (paused={}, state={:?})", stats.total_frames, frontend.is_paused(), frontend.get_replay_state());
    if end_frame + 1 < stats.total_frames {
        println!("FAIL: playback stopped {} frames before the end of the recording", stats.total_frames - end_frame);
        failed = true;
    }
    phases.push(playback);

    // Seek back into the middle and play out the rest, as a timeline click does.
    let middle = stats.total_frames / 2;
    frontend.go_to_replay_frame(middle);
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(60) {
        tick_for(&mut frontend, Duration::from_millis(100));
        if frontend.get_replay_frame() <= middle + 2 && frontend.get_replay_frame() + 60 >= middle {
            break;
        }
    }
    println!("seek to {middle}: landed at {} after {:.2} s", frontend.get_replay_frame(), started.elapsed().as_secs_f64());
    frontend.set_paused(false);
    let from_middle = measure(&mut frontend, "from middle", playback_budget, |f| f.is_paused() && f.get_replay_frame() >= stats.total_frames.saturating_sub(1));
    let end_frame = frontend.get_replay_frame();
    if end_frame + 1 < stats.total_frames {
        println!("FAIL: playback after the seek stopped {} frames before the end", stats.total_frames - end_frame);
        failed = true;
    }
    phases.push(from_middle);

    println!();
    let target = speed * 60.0;
    for phase in &phases {
        phase.print();
        if phase.fps() < target * 0.9 {
            println!("FAIL: {} ran at {:.1} fps, under 90% of the {target:.0} fps target", phase.name, phase.fps());
            failed = true;
        }
    }

    println!("RESULT: {}", if failed { "FAIL" } else { "OK" });
    if failed {
        std::process::exit(1);
    }
}
