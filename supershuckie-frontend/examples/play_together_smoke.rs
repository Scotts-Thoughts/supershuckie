//! Play Together end to end, headless: two or three real frontends in one process (each with its
//! own core thread, publisher and followers) on the loopback interface, the way the app runs them
//! (paced, ticked from a UI loop, no window). One hosts, the rest join, everybody follows
//! everybody else for a while, then everyone leaves and the followers' replay files are played
//! back to their end.
//!
//! ```text
//! play_together_smoke <rom.gbc|rom.gba> [--players 2] [--speed 4] [--seconds 20]
//! ```
//!
//! Exits non-zero when a frontend's own game ran below 90% of the requested speed (the local
//! game must stay smooth whatever the followers do), when a follower fell more than a second of
//! frames behind or ever desynced, when a follower's screen never arrived, or when a saved
//! friend replay does not play back to its end.
//!
//! Link it like `supershuckie-core`'s `nds_bench` (see that file's header).

use std::collections::BTreeMap;
use std::num::NonZeroU8;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use supershuckie_core::emulator::ScreenData;
use supershuckie_frontend::play_together::PeerId;
use supershuckie_frontend::{ScreenInfo, SuperShuckieFrontend, SuperShuckieFrontendCallbacks, SuperShuckieReplayState};
use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;

/// Counts the screens each peer delivered.
#[derive(Default)]
struct PeerScreens(Arc<Mutex<BTreeMap<PeerId, u64>>>);

impl SuperShuckieFrontendCallbacks for PeerScreens {
    fn refresh_screens(&mut self, _: &[ScreenData]) {}
    fn change_video_mode(&mut self, _: &[ScreenInfo], _: NonZeroU8) {}
    fn peer_refresh_screens(&mut self, peer: PeerId, _: &[ScreenData]) {
        *self.0.lock().unwrap().entry(peer).or_default() += 1;
    }
}

struct Player {
    name: String,
    frontend: SuperShuckieFrontend,
    screens: Arc<Mutex<BTreeMap<PeerId, u64>>>,
    /// Per followed peer: every frames-behind sample taken.
    behind: BTreeMap<PeerId, Vec<u64>>,
    fps_samples: Vec<f64>,
}

fn tick_all(players: &mut [Player]) {
    for p in players.iter_mut() {
        if let Err(e) = p.frontend.tick() {
            println!("[{}] tick error: {e}", p.name);
        }
    }
}

fn tick_all_for(players: &mut [Player], duration: Duration) {
    let until = Instant::now() + duration;
    while Instant::now() < until {
        tick_all(players);
        std::thread::sleep(Duration::from_millis(1));
    }
}

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

fn make_player(dir: &Path, name: &str, rom: &Path, speed: f64) -> Player {
    let user = dir.join(name);
    let _ = std::fs::remove_dir_all(&user);
    std::fs::create_dir_all(&user).unwrap();
    // Several frontends in one process: the REST server and Poke-A-Byte bind fixed ports, so
    // only the settings can keep them apart (both default on).
    std::fs::write(
        user.join("settings.json"),
        r#"{"pokeabyte": {"enabled": false}, "external_commands": {"enabled": false}, "play_together": {"display_name": "NAME", "bind_address": "127.0.0.1"}}"#.replace("NAME", name)
    ).unwrap();
    let screens = Arc::new(Mutex::new(BTreeMap::new()));
    let mut frontend = SuperShuckieFrontend::new(user.clone(), user.clone(), Box::new(PeerScreens(screens.clone())));
    frontend.set_speed_settings(speed, 2.0);
    frontend.set_auto_pause_on_record_setting(false);
    frontend.load_rom(rom).expect("load rom");
    frontend.set_paused(false);
    Player { name: name.to_owned(), frontend, screens, behind: BTreeMap::new(), fps_samples: Vec::new() }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let rom = std::path::absolute(args.next().expect("usage: play_together_smoke <rom> [--players n] [--speed n] [--seconds n]")).unwrap();
    let mut players_wanted = 2usize;
    let mut speed = 4.0f64;
    let mut seconds = 20.0f64;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--players" => players_wanted = args.next().unwrap().parse().unwrap(),
            "--speed" => speed = args.next().unwrap().parse().unwrap(),
            "--seconds" => seconds = args.next().unwrap().parse().unwrap(),
            other => panic!("unexpected {other}"),
        }
    }
    assert!((2..=8).contains(&players_wanted), "2 to 8 players");
    request_fine_timer_resolution();

    let dir = std::env::temp_dir().join("supershuckie-play-together-smoke");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let names = ["Host", "Ash", "Misty", "Brock", "Gary", "Red", "Blue", "May"];
    let mut players: Vec<Player> = names.iter().take(players_wanted).map(|n| make_player(&dir, n, &rom, speed)).collect();
    println!("{} at {speed}x, {players_wanted} players, {seconds} s", rom.display());

    // Past the boot sequence.
    tick_all_for(&mut players, Duration::from_secs(3));

    let mut failed = false;

    // Host on a free port (found by binding one and letting it go), then everyone else joins.
    let free_port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let code = match players[0].frontend.play_together_host(free_port, "Host") {
        Ok(code) => code.to_string(),
        Err(e) => {
            println!("FAIL: host: {e}");
            std::process::exit(1);
        }
    };
    let port = code.rsplit(':').next().unwrap();
    let join_code = format!("127.0.0.1:{port}");
    println!("hosting at {code}; joining with {join_code}");
    tick_all_for(&mut players, Duration::from_millis(200));
    for p in players.iter_mut().skip(1) {
        let name = p.name.clone();
        if let Err(e) = p.frontend.play_together_join(&join_code, &name) {
            println!("FAIL: {name} could not join: {e}");
            std::process::exit(1);
        }
    }

    // Everyone should end up following everyone else.
    let started = Instant::now();
    loop {
        tick_all(&mut players);
        let all_following = players.iter().all(|p| {
            let state = p.frontend.play_together_state();
            state.participants.len() == players_wanted - 1 && state.participants.iter().all(|q| q.status == "following" || q.status == "waiting")
        });
        if all_following {
            break;
        }
        if started.elapsed() > Duration::from_secs(30) {
            println!("FAIL: not everyone is following after 30 s:");
            for p in &players {
                let state = p.frontend.play_together_state();
                println!("  [{}] role={} participants={:?} errors={:?}", p.name, state.role, state.participants.iter().map(|q| format!("{}:{}:{}", q.name, q.status, q.status_text)).collect::<Vec<_>>(), state.errors);
            }
            std::process::exit(1);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    println!("everyone is following after {:.1} s", started.elapsed().as_secs_f64());
    for p in &players {
        let state = p.frontend.play_together_state();
        println!("  [{}] {} as {} (peer {}), following {}", p.name, state.role, state.local_name, state.local_peer_id, state.participants.iter().map(|q| format!("{} ({})", q.name, q.replay_file.clone().unwrap_or_else(|| "no file".into()))).collect::<Vec<_>>().join(", "));
    }

    // Measure.
    let started = Instant::now();
    let mut since_sample = Instant::now();
    let mut since_fps = Instant::now();
    while started.elapsed().as_secs_f64() < seconds {
        tick_all(&mut players);
        if since_sample.elapsed() >= Duration::from_millis(100) {
            since_sample = Instant::now();
            for p in players.iter_mut() {
                for q in p.frontend.play_together_state().participants {
                    p.behind.entry(q.peer_id).or_default().push(q.frames_behind);
                }
            }
        }
        if since_fps.elapsed() >= Duration::from_secs(1) {
            since_fps = Instant::now();
            for p in players.iter_mut() {
                let fps = p.frontend.get_emulation_fps();
                if fps > 0.0 {
                    p.fps_samples.push(fps);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    // Report.
    let target = speed * 60.0;
    for p in &players {
        let avg = if p.fps_samples.is_empty() { 0.0 } else { p.fps_samples.iter().sum::<f64>() / p.fps_samples.len() as f64 };
        let min = p.fps_samples.iter().cloned().fold(f64::INFINITY, f64::min);
        println!("[{}] own game: {avg:.1} fps average, {min:.1} fps worst second (target {target:.0})", p.name);
        if avg < target * 0.9 {
            println!("FAIL: [{}] own game ran below 90% of the target", p.name);
            failed = true;
        }
        let state = p.frontend.play_together_state();
        for q in &state.participants {
            let samples = p.behind.get(&q.peer_id).cloned().unwrap_or_default();
            let mut sorted = samples.clone();
            sorted.sort();
            let median = sorted.get(sorted.len() / 2).copied().unwrap_or(0);
            let max = sorted.last().copied().unwrap_or(0);
            let screens = p.screens.lock().unwrap().get(&q.peer_id).copied().unwrap_or(0);
            println!(
                "[{}] following {}: status {} · behind median {median} max {max} frames · {} snapshots · {} desyncs · {:.0} fps · {screens} screens",
                p.name, q.name, q.status, q.snapshots_applied, q.hash_mismatches, q.fps
            );
            if max > (speed * 60.0) as u64 {
                println!("FAIL: [{}] fell more than a second behind {}", p.name, q.name);
                failed = true;
            }
            if median > 10 {
                println!("FAIL: [{}] is usually more than 10 frames behind {}", p.name, q.name);
                failed = true;
            }
            if q.hash_mismatches > 0 {
                println!("FAIL: [{}] desynced from {}", p.name, q.name);
                failed = true;
            }
            if screens == 0 {
                println!("FAIL: [{}] never received {}'s screen", p.name, q.name);
                failed = true;
            }
        }
        if !state.errors.is_empty() {
            println!("[{}] errors: {:?}", p.name, state.errors);
        }
    }

    // A race start from the host: everyone resets (which shows up as a ResetConsole packet in
    // every friend's replay file, checked below).
    players[0].frontend.play_together_reset_all(1).expect("reset all");
    let started = Instant::now();
    let mut counted_down = false;
    while started.elapsed() < Duration::from_millis(1500) {
        tick_all(&mut players);
        counted_down |= players.iter().all(|p| p.frontend.play_together_reset_countdown_ms() > 0);
        std::thread::sleep(Duration::from_millis(1));
    }
    if !counted_down {
        println!("FAIL: the reset countdown never showed on every player");
        failed = true;
    }
    tick_all_for(&mut players, Duration::from_secs(2));

    // Leave, then play every saved friend replay back to its end.
    let replay_files: Vec<(String, PathBuf, u64)> = players.iter().flat_map(|p| {
        let state = p.frontend.play_together_state();
        let dir = p.frontend.get_replays_dir_for_current_rom().expect("replays dir");
        state.participants.into_iter().filter_map(move |q| q.replay_file.map(|f| (format!("{} following {}", p.name, q.name), dir.join(f), q.elapsed_frames)))
    }).collect();
    for p in players.iter_mut() {
        p.frontend.play_together_leave();
    }
    tick_all_for(&mut players, Duration::from_millis(500));

    for (what, path, elapsed_frames) in &replay_files {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                println!("FAIL: {what}: cannot read {}: {e}", path.display());
                failed = true;
                continue;
            }
        };
        let player = match ReplayFilePlayer::new(&bytes, false) {
            Ok(p) => p,
            Err(e) => {
                println!("FAIL: {what}: cannot parse {}: {e:?}", path.display());
                failed = true;
                continue;
            }
        };
        let total = player.get_total_frames();
        let mut player = player;
        player.set_keyframe_states_wanted(false);
        let mut resets = 0;
        let mut inputs = 0u64;
        while let Some(packet) = player.next_packet().expect("read packet") {
            match packet {
                supershuckie_replay_recorder::Packet::ResetConsole => resets += 1,
                supershuckie_replay_recorder::Packet::ChangeInput { .. } => inputs += 1,
                _ => {}
            }
        }
        println!(
            "{what}: {} is v{}, {total} frames, {} keyframes, {resets} resets, {:.3} ChangeInput/frame, {} bytes (publisher was at frame {elapsed_frames})",
            path.file_name().unwrap().to_string_lossy(), player.get_replay_version(), player.all_keyframes().len(), inputs as f64 / total.max(1) as f64, bytes.len()
        );
        if total < 100 {
            println!("FAIL: {what}: the file is too short");
            failed = true;
        }
        if resets != 1 {
            println!("FAIL: {what}: expected the race-start reset once in the file, found {resets}");
            failed = true;
        }
        if inputs as f64 > 2.0 * total as f64 {
            println!("FAIL: {what}: the input was streamed more than once per frame");
            failed = true;
        }
    }

    // Play the first friend replay back in a fresh frontend.
    if let Some((what, path, _)) = replay_files.first() {
        let mut viewer = make_player(&dir, "Viewer", &rom, speed);
        let replays = viewer.frontend.get_replays_dir_for_current_rom().expect("replays dir");
        let name = "friend-smoke";
        std::fs::copy(path, replays.join(format!("{name}.replay"))).unwrap();
        tick_all_for(std::slice::from_mut(&mut viewer), Duration::from_secs(1));
        match viewer.frontend.load_replay_if_exists(name, false) {
            Ok(true) => {}
            other => {
                println!("FAIL: {what}: load_replay_if_exists: {other:?}");
                std::process::exit(1);
            }
        }
        let stats = viewer.frontend.get_replay_playback_stats().expect("attached");
        assert_eq!(viewer.frontend.get_replay_state(), SuperShuckieReplayState::Playback);
        viewer.frontend.set_paused(false);
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs_f64(seconds * 2.0 + 10.0) {
            tick_all(std::slice::from_mut(&mut viewer));
            if viewer.frontend.is_paused() && viewer.frontend.get_replay_frame() >= stats.total_frames.saturating_sub(1) {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let end = viewer.frontend.get_replay_frame();
        println!("{what}: played back to frame {end} of {}", stats.total_frames);
        if end + 1 < stats.total_frames {
            println!("FAIL: {what}: playback stopped {} frames early", stats.total_frames - end);
            failed = true;
        }
    }

    if failed {
        println!("FAILED");
        std::process::exit(1);
    }
    println!("OK");
}
