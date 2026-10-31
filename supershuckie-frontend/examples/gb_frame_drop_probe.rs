//! Where a paced game loses frames: run the game through the real frontend and core thread (no
//! window) under a series of conditions, adding one at a time, and report for each the average
//! fps, the worst one-second window, how many frames it fell short of the target, and the
//! process CPU time per emulated frame.
//!
//! ```text
//! gb_frame_drop_probe <rom> [--speed 4] [--seconds 20] [--compression 9]
//! ```
//!
//! Conditions, in order: live; live with a Poke-A-Byte client attached (a fake one on a spare
//! port that sets up a Game Boy-sized read list, the way Poke-A-Byte does); recording at the given
//! compression level with Poke-A-Byte; the same with audio on (a second SameBoy instance for Game
//! Boy games).
//!
//! Link it like `supershuckie-core`'s `nds_bench` (see that file's header).

use std::net::{Ipv4Addr, UdpSocket};
use std::num::NonZeroU8;
use std::time::{Duration, Instant};

use supershuckie_core::emulator::ScreenData;
use supershuckie_frontend::{ScreenInfo, SuperShuckieFrontend, SuperShuckieFrontendCallbacks};

struct NoScreen;

impl SuperShuckieFrontendCallbacks for NoScreen {
    fn refresh_screens(&mut self, _: &[ScreenData]) {}
    fn change_video_mode(&mut self, _: &[ScreenInfo], _: NonZeroU8) {}
}

/// A port the user's own Poke-A-Byte setup is not on.
const PROBE_POKEABYTE_PORT: u16 = 56456;

fn tick_for(frontend: &mut SuperShuckieFrontend, duration: Duration) {
    let until = Instant::now() + duration;
    while Instant::now() < until {
        if let Err(e) = frontend.tick() {
            println!("tick error: {e}");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(windows)]
fn process_cpu_seconds() -> f64 {
    #[repr(C)]
    #[derive(Default)]
    struct FileTime { low: u32, high: u32 }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut core::ffi::c_void;
        fn GetProcessTimes(process: *mut core::ffi::c_void, creation: *mut FileTime, exit: *mut FileTime, kernel: *mut FileTime, user: *mut FileTime) -> i32;
    }
    let (mut c, mut e, mut k, mut u) = (FileTime::default(), FileTime::default(), FileTime::default(), FileTime::default());
    // SAFETY: plain Win32 call writing into locals.
    unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) };
    let ticks = |t: &FileTime| ((t.high as u64) << 32 | t.low as u64) as f64 / 10_000_000.0;
    ticks(&k) + ticks(&u)
}

#[cfg(not(windows))]
fn process_cpu_seconds() -> f64 { 0.0 }

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

/// Ask the frontend's Poke-A-Byte server for a session the way Poke-A-Byte does: a SETUP with the
/// Game Boy's work RAM, HRAM and cartridge RAM. Returns the socket (keep it alive).
fn attach_fake_pokeabyte(port: u16) -> UdpSocket {
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind the fake client");
    socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    const READ_BLOCK_SIZE: usize = 0xC;
    const MAX_BLOCKS: usize = 128;
    let blocks: [(u32, u32, u32); 3] = [(0, 0xC000, 0x2000), (0x2000, 0xFF80, 0x7F), (0x2080, 0xA000, 0x2000)];
    let mut packet = vec![0u8; 32 + READ_BLOCK_SIZE * MAX_BLOCKS];
    packet[0] = 1; // protocol version
    packet[4] = 2; // SETUP
    packet[8..12].copy_from_slice(&(blocks.len() as u32).to_le_bytes());
    packet[12..16].copy_from_slice(&(-1i32).to_le_bytes()); // no frame skip
    for (i, (offset, address, length)) in blocks.iter().enumerate() {
        let base = 32 + i * READ_BLOCK_SIZE;
        packet[base..base + 4].copy_from_slice(&offset.to_le_bytes());
        packet[base + 4..base + 8].copy_from_slice(&address.to_le_bytes());
        packet[base + 8..base + 12].copy_from_slice(&length.to_le_bytes());
    }
    socket.send_to(&packet, (Ipv4Addr::LOCALHOST, port)).expect("send SETUP");
    socket
}

struct Phase {
    name: String,
    frames: u64,
    seconds: f64,
    worst_window_fps: f64,
    windows_below: usize,
    windows: usize,
    short_by: f64,
    cpu_ms_per_frame: f64,
    longest_gap_ms: f64,
}

fn measure(frontend: &mut SuperShuckieFrontend, name: String, seconds: f64, target_fps: f64) -> Phase {
    tick_for(frontend, Duration::from_millis(500));
    let start_frames = frontend.get_elapsed_frames() as u64;
    let start_cpu = process_cpu_seconds();
    let started = Instant::now();
    let mut window_start = started;
    let mut window_frames = start_frames;
    let mut window_fps = Vec::new();
    let mut last_frames = start_frames;
    let mut last_advance = started;
    let mut longest_gap = Duration::ZERO;
    while started.elapsed().as_secs_f64() < seconds {
        if let Err(e) = frontend.tick() {
            println!("tick error: {e}");
        }
        let now = Instant::now();
        let frames = frontend.get_elapsed_frames() as u64;
        if frames != last_frames {
            longest_gap = longest_gap.max(now - last_advance);
            last_frames = frames;
            last_advance = now;
        }
        if now - window_start >= Duration::from_secs(1) {
            window_fps.push((frames - window_frames) as f64 / (now - window_start).as_secs_f64());
            window_start = now;
            window_frames = frames;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let seconds = started.elapsed().as_secs_f64();
    let frames = frontend.get_elapsed_frames() as u64 - start_frames;
    let cpu = process_cpu_seconds() - start_cpu;
    Phase {
        name,
        frames,
        seconds,
        worst_window_fps: window_fps.iter().copied().fold(f64::INFINITY, f64::min),
        windows_below: window_fps.iter().filter(|f| **f < target_fps - 1.5).count(),
        windows: window_fps.len(),
        short_by: (target_fps * seconds - frames as f64).max(0.0),
        cpu_ms_per_frame: cpu * 1000.0 / frames.max(1) as f64,
        longest_gap_ms: longest_gap.as_secs_f64() * 1000.0,
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let rom = std::path::absolute(args.next().expect("usage: gb_frame_drop_probe <rom> [--speed n] [--seconds n] [--compression n]")).unwrap();
    let mut speed = 4.0f64;
    let mut seconds = 20.0f64;
    let mut compression = 9;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--speed" => speed = args.next().unwrap().parse().unwrap(),
            "--seconds" => seconds = args.next().unwrap().parse().unwrap(),
            "--compression" => compression = args.next().unwrap().parse().unwrap(),
            other => panic!("unexpected {other}"),
        }
    }

    request_fine_timer_resolution();
    let dir = std::env::temp_dir().join("supershuckie-gb-frame-drop-probe");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let user = dir.join("UserData");

    let mut frontend = SuperShuckieFrontend::new(user.clone(), user.clone(), Box::new(NoScreen));
    frontend.set_speed_settings(speed, 2.0);
    frontend.set_auto_pause_on_record_setting(false);
    frontend.set_replay_compression_level(compression);
    frontend.set_audio_enabled(false);
    frontend.set_pokeabyte_port(PROBE_POKEABYTE_PORT).expect("pokeabyte port");
    let _ = frontend.set_pokeabyte_enabled(false);
    frontend.load_rom(&rom).expect("load rom");
    frontend.set_paused(false);
    tick_for(&mut frontend, Duration::from_secs(5));

    // Game Boy: 4194304 Hz / 70224 cycles per frame.
    let target = 4194304.0 / 70224.0 * speed;
    println!("{} at {speed}x (target {target:.1} fps), {seconds} s per phase, compression {compression}", rom.display());

    let mut phases = Vec::new();
    phases.push(measure(&mut frontend, "live".into(), seconds, target));

    frontend.set_pokeabyte_enabled(true).expect("enable pokeabyte");
    tick_for(&mut frontend, Duration::from_millis(300));
    let _client = attach_fake_pokeabyte(PROBE_POKEABYTE_PORT);
    phases.push(measure(&mut frontend, "live + pokeabyte".into(), seconds, target));

    frontend.start_recording_replay(Some("probe")).expect("record");
    frontend.set_paused(false);
    phases.push(measure(&mut frontend, format!("recording (level {compression}) + pokeabyte"), seconds, target));

    frontend.set_audio_enabled(true);
    phases.push(measure(&mut frontend, "recording + pokeabyte + audio".into(), seconds, target));
    let _ = frontend.stop_recording_replay();
    tick_for(&mut frontend, Duration::from_millis(300));

    println!();
    for p in &phases {
        println!(
            "{:<38} {:>6.1} fps  worst 1-s window {:>6.1}  windows below target {:>2}/{:<2}  short by {:>5.0} frames  CPU {:>5.2} ms/frame  longest gap {:>6.2} ms",
            p.name, p.frames as f64 / p.seconds, p.worst_window_fps, p.windows_below, p.windows, p.short_by, p.cpu_ms_per_frame, p.longest_gap_ms
        );
    }
}
