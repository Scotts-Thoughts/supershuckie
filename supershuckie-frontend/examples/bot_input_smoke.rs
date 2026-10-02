//! End-to-end checks of bot control through the frontend and its REST API, against a real
//! threaded core and real HTTP requests (no keyboard or mouse input):
//!
//! - every bot route is refused (403) until bot control is turned on, which it is not by default;
//! - `/step` pauses a running game, runs exactly the frames asked for and reads memory after them;
//! - `/read-memory` and `/screenshot`;
//! - real time (`/input`, `/press`) while the game runs;
//! - a replay recorded in lockstep is stamped at normal speed despite long pauses between steps,
//!   carries the bot's input, and plays back to the same frame count and memory;
//! - the routes are refused while that replay plays back, and again once bot control is off;
//! - round-trip times of single-frame and longer steps.
//!
//! ```text
//! bot_input_smoke <rom.gbc | rom.gba | rom.nds>
//! ```
//!
//! Needs 127.0.0.1:30158 free. Link it like the other frontend examples.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::num::NonZeroU8;
use std::thread::JoinHandle;
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

fn tick_for(frontend: &mut SuperShuckieFrontend, duration: Duration) {
    let until = Instant::now() + duration;
    while Instant::now() < until {
        frontend.tick().expect("tick");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Start a GET of `path` on another thread; the frontend must be ticked for it to be served.
fn request(path: &str) -> JoinHandle<(u16, Vec<u8>)> {
    let request = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    std::thread::spawn(move || {
        let mut stream = TcpStream::connect("127.0.0.1:30158").expect("connect");
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        let split = response.windows(4).position(|w| w == b"\r\n\r\n").expect("headers");
        let head = String::from_utf8_lossy(&response[..split]).into_owned();
        let status = head.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        let body = &response[split + 4..];
        let chunked = head.to_ascii_lowercase().contains("transfer-encoding: chunked");
        (status, if chunked { dechunk(body) } else { body.to_vec() })
    })
}

/// Undo HTTP/1.1 chunked transfer encoding (rouille uses it for larger bodies).
fn dechunk(mut body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let line_end = body.windows(2).position(|w| w == b"\r\n").expect("chunk size line");
        let size = usize::from_str_radix(std::str::from_utf8(&body[..line_end]).unwrap().split(';').next().unwrap().trim(), 16).expect("chunk size");
        body = &body[line_end + 2..];
        if size == 0 {
            return out
        }
        out.extend_from_slice(&body[..size]);
        body = &body[size + 2..];
    }
}

fn finish(frontend: &mut SuperShuckieFrontend, worker: JoinHandle<(u16, Vec<u8>)>) -> (u16, Vec<u8>) {
    while !worker.is_finished() {
        frontend.tick().expect("tick");
        std::thread::sleep(Duration::from_millis(1));
    }
    worker.join().unwrap()
}

/// GET `path` while ticking the frontend; the status and body.
fn rest(frontend: &mut SuperShuckieFrontend, path: &str) -> (u16, Vec<u8>) {
    finish(frontend, request(path))
}

/// GET `path`, expecting a JSON reply with `status`.
fn rest_json(frontend: &mut SuperShuckieFrontend, path: &str, status: u16) -> serde_json::Value {
    let (got, body) = rest(frontend, path);
    let text = String::from_utf8_lossy(&body);
    assert_eq!(got, status, "{path}: {text}");
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path}: bad JSON {text:?}: {e}"))
}

fn step(frontend: &mut SuperShuckieFrontend, query: &str) -> serde_json::Value {
    rest_json(frontend, &format!("/step?{query}"), 200)
}

fn frame_of(value: &serde_json::Value) -> u64 {
    value["frame"].as_u64().unwrap_or_else(|| panic!("no frame in {value}"))
}

fn main() {
    let rom = std::path::absolute(std::env::args().nth(1).expect("usage: bot_input_smoke <rom>")).unwrap();
    let rom_file_name = rom.file_name().unwrap().to_str().unwrap().to_owned();
    let extension = rom.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    // Work RAM to read, and the picture size, of each console.
    let (ram, picture) = match extension.as_str() {
        "gb" | "gbc" => (0xC000u32, (160u32, 144u32)),
        "gba" => (0x0200_0000, (240, 160)),
        "nds" => (0x0200_0000, (256, 384)),
        other => panic!("unsupported ROM type {other}")
    };

    let dir = std::env::temp_dir().join("supershuckie-frontend-bot-smoke");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut frontend = SuperShuckieFrontend::new(dir.join("data"), dir.join("config"), Box::new(NoScreen));
    assert!(frontend.get_external_commands_enabled().is_ok_and(|enabled| enabled), "REST API unavailable (port 30158 in use?)");
    assert!(!frontend.get_bot_control_enabled(), "bot control must be off by default");

    frontend.load_rom(&rom).expect("load rom");
    frontend.set_auto_pause_on_record_setting(false);
    frontend.set_paused(false);
    tick_for(&mut frontend, Duration::from_millis(300));

    // --- Off by default: every bot route is refused, and nothing is paused or pressed. ---
    for path in ["/step", "/input?buttons=a", "/press?buttons=a", "/read-memory?address=0&length=1", "/screenshot"] {
        let refused = rest_json(&mut frontend, path, 403);
        assert!(refused["error"].as_str().unwrap().contains("bot control is off"), "{path}: {refused}");
    }
    assert!(!frontend.is_paused(), "a refused step must not pause the game");
    // Bad arguments are a 400 whatever the setting.
    rest_json(&mut frontend, "/input?buttons=jump", 400);
    rest_json(&mut frontend, "/step?frames=3601", 400);
    println!("off by default: ok");

    frontend.set_bot_control_enabled(true);
    let replay_name = frontend.start_recording_replay(Some("bot-smoke")).expect("record").to_string();
    tick_for(&mut frontend, Duration::from_millis(300));
    assert!(!frontend.is_paused());

    // --- Lockstep. ---
    let first = step(&mut frontend, "frames=1");
    assert_eq!(first["frames_run"], 1);
    assert_eq!(first["was_running"], true, "the game was running: {first}");
    assert_eq!(first["cancelled"], false);
    assert!(frontend.is_paused(), "a step leaves the game paused");

    let mut frame = frame_of(&first);
    let started = Instant::now();
    const SINGLE_STEPS: u64 = 200;
    for i in 0..SINGLE_STEPS {
        let buttons = if i % 8 < 4 { "a" } else { "" };
        let outcome = step(&mut frontend, &format!("frames=1&buttons={buttons}&read={ram:#x}:16"));
        assert_eq!(outcome["was_running"], false, "nothing resumed the game: {outcome}");
        assert_eq!(frame_of(&outcome), frame + 1, "one frame per step: {outcome}");
        assert_eq!(outcome["reads"][0]["data"].as_str().map(str::len), Some(32), "{outcome}");
        frame += 1;
    }
    let single = started.elapsed() / SINGLE_STEPS as u32;

    let started = Instant::now();
    const LONG_STEPS: u64 = 10;
    for _ in 0..LONG_STEPS {
        let outcome = step(&mut frontend, "frames=60&buttons=right");
        assert_eq!(frame_of(&outcome), frame + 60);
        frame += 60;
    }
    let long = started.elapsed() / LONG_STEPS as u32;
    let frame_time = rest_json(&mut frontend, "/stats", 200)["frame_time_ms"].clone();

    // The bot thinks for a while between two steps: the replay must not carry that time.
    tick_for(&mut frontend, Duration::from_millis(400));
    let outcome = step(&mut frontend, "frames=5&buttons=");
    assert_eq!(frame_of(&outcome), frame + 5);
    frame += 5;

    // A zero-frame step only reads; /read-memory agrees with it.
    let zero = step(&mut frontend, &format!("frames=0&read={ram:#x}:64"));
    assert_eq!((frame_of(&zero), zero["frames_run"].as_u64()), (frame, Some(0)));
    let memory = rest_json(&mut frontend, &format!("/read-memory?address={ram}&length=64"), 200);
    assert_eq!(memory["data"], zero["reads"][0]["data"], "the two ways of reading memory agree");
    rest_json(&mut frontend, "/read-memory?address=0xFFFFFFF0&length=4", 404);
    println!("lockstep: ok ({SINGLE_STEPS} single-frame steps at {single:?} each, {LONG_STEPS} 60-frame steps at {long:?} each; the core thread's own time per stepped frame {frame_time} ms)");

    // --- Screenshot. ---
    let (status, png) = rest(&mut frontend, "/screenshot");
    assert_eq!(status, 200);
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    let width = u32::from_be_bytes(png[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(png[20..24].try_into().unwrap());
    assert_eq!((width, height), picture, "the picture size");
    std::fs::write(dir.join("screenshot.png"), &png).unwrap();
    println!("screenshot: ok ({width}x{height}, {} bytes, {})", png.len(), dir.join("screenshot.png").display());

    // --- A second step while one is running is refused; unpausing cancels the running one. ---
    let long_step = request("/step?frames=3600");
    let deadline = Instant::now() + Duration::from_millis(5);
    while Instant::now() < deadline {
        frontend.tick().expect("tick");
    }
    let (second_status, second_body) = rest(&mut frontend, "/step?frames=1");
    let unpause = rest(&mut frontend, "/set-paused?paused=false");
    assert_eq!(unpause.0, 204);
    let (status, body) = finish(&mut frontend, long_step);
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let long_outcome: serde_json::Value = serde_json::from_slice(&body).unwrap();
    if long_outcome["cancelled"] == true {
        assert!(long_outcome["frames_run"].as_u64().unwrap() < 3600);
        assert_eq!(second_status, 409, "a second step while one runs: {}", String::from_utf8_lossy(&second_body));
        println!("cancel: ok (unpausing ended a 3600-frame step after {} frames: {})", long_outcome["frames_run"], long_outcome["cancel_reason"]);
    }
    else {
        println!("cancel: not exercised (the 3600-frame step finished before the unpause arrived; second step got {second_status})");
    }
    assert!(!frontend.is_paused());

    // --- Real time: the game keeps running. ---
    let before = frontend.get_elapsed_frames();
    let held = rest_json(&mut frontend, "/input?buttons=b,up", 200);
    assert!(frame_of(&held) >= before as u64);
    tick_for(&mut frontend, Duration::from_millis(100));
    let pressed = rest_json(&mut frontend, "/press?buttons=start&frames=2", 200);
    tick_for(&mut frontend, Duration::from_millis(100));
    rest_json(&mut frontend, "/input", 200);
    tick_for(&mut frontend, Duration::from_millis(100));
    assert!(frontend.get_elapsed_frames() > before + 10, "the game ran on in real time");
    assert!(!frontend.is_paused(), "real-time input does not pause");
    println!("real time: ok (input at frame {}, press at frame {})", frame_of(&held), frame_of(&pressed));

    // --- End in lockstep at a frame boundary, note the state, stop recording. ---
    let last = step(&mut frontend, &format!("frames=1&buttons=&read={ram:#x}:256"));
    let final_frame = frame_of(&last);
    let final_memory = last["reads"][0]["data"].as_str().unwrap().to_owned();
    frontend.stop_recording_replay().expect("stop recording");

    let replay_path = dir.join("data").join(format!("{rom_file_name}-data")).join("replays").join(&replay_name);
    let bytes = std::fs::read(&replay_path).expect("read the replay");
    let mut player = ReplayFilePlayer::new(&bytes, false).expect("parse the replay");
    let total_frames = player.get_total_frames();
    let mut largest_delta = 0;
    let mut nonzero_inputs = 0;
    while let Some(packet) = player.next_packet().expect("packet") {
        match packet {
            Packet::NextFrame { timestamp_delta } => largest_delta = largest_delta.max(timestamp_delta.0),
            Packet::ChangeInput { data } if data.iter().any(|b| *b != 0) => nonzero_inputs += 1,
            _ => {}
        }
    }
    assert!(largest_delta < 200, "a frame carries {largest_delta} ms: the time between steps leaked into the replay");
    assert!(nonzero_inputs > 0, "the bot's input was recorded");
    println!("replay: ok ({total_frames} frames, largest frame gap {largest_delta} ms, {nonzero_inputs} inputs with a button held)");

    // --- Playback: refused routes, then the same end state. ---
    let replay_stem = replay_name.trim_end_matches(".replay");
    assert!(frontend.load_replay_if_exists(replay_stem, false).expect("load replay"));
    assert_eq!(frontend.get_replay_state(), SuperShuckieReplayState::Playback);
    rest_json(&mut frontend, "/input?buttons=a", 409);
    rest_json(&mut frontend, "/press?buttons=a", 409);
    rest_json(&mut frontend, "/step", 409);
    frontend.set_paused(false);
    let deadline = Instant::now() + Duration::from_secs(120);
    let stats = loop {
        tick_for(&mut frontend, Duration::from_millis(50));
        let stats = rest_json(&mut frontend, "/stats", 200);
        if stats["is_playback_finished"] == true {
            break stats;
        }
        assert!(Instant::now() < deadline, "playback never finished: {stats}");
    };
    let played_memory = rest_json(&mut frontend, &format!("/read-memory?address={ram}&length=256"), 200);
    assert_eq!(stats["total_elapsed_frames"].as_u64(), Some(final_frame), "playback ends on the frame recording did");
    assert_eq!(played_memory["data"].as_str(), Some(final_memory.as_str()), "playback ends in the state recording did");
    println!("playback: ok (refused while playing; ended at frame {final_frame} with the same memory)");

    // --- Off again. ---
    frontend.close_replay();
    frontend.set_bot_control_enabled(false);
    rest_json(&mut frontend, "/step", 403);
    println!("bot_input_smoke: all checks passed");
}
