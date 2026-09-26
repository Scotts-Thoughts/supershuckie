//! Load a 3DS game through the frontend the way the app does (core on its own thread, ticked
//! from a UI-style loop) and report what reaches the screen callbacks: sizes, how often they
//! fire, whether the pixels are anything but black, and whether they keep changing.
//! `n3ds_frontend_check <rom.3ds> [--seconds 30] [--mash] [--a-key 88] [--start-key 67] [--speed 1]`
//! `--mash` taps A (a keyboard keycode bound to A in the seeded settings) twice a second and
//! Start now and then, enough to get from the title screen into a saved game.

use std::num::NonZeroU8;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use supershuckie_core::emulator::ScreenData;
use supershuckie_frontend::{ScreenInfo, SuperShuckieFrontend, SuperShuckieFrontendCallbacks, UserInput};

#[derive(Default)]
struct Seen {
    refreshes: u64,
    modes: Vec<String>,
    last_non_black: Vec<usize>,
    last_sizes: Vec<(usize, usize)>,
    last_hash: Vec<u64>,
    last_pixels: Vec<(usize, usize, Vec<u32>)>,
    /// Non-black pixel count of the top screen for the last refreshes, oldest first.
    recent_top: std::collections::VecDeque<usize>,
}

/// Write a screen as a 24-bit BMP.
fn write_bmp(path: &std::path::Path, width: usize, height: usize, pixels: &[u32]) {
    let row = (width * 3 + 3) & !3;
    let size = 54 + row * height;
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(size as u32).to_le_bytes());
    out.extend_from_slice(&[0u8; 4]);
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(width as i32).to_le_bytes());
    out.extend_from_slice(&(height as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&[0u8; 24]);
    for y in (0..height).rev() {
        let start = out.len();
        for x in 0..width {
            let p = pixels[y * width + x];
            out.extend_from_slice(&[(p & 0xFF) as u8, ((p >> 8) & 0xFF) as u8, ((p >> 16) & 0xFF) as u8]);
        }
        while out.len() - start < row {
            out.push(0);
        }
    }
    std::fs::write(path, out).expect("write bmp");
}

struct Watch(Arc<Mutex<Seen>>);

fn hash(pixels: &[u32]) -> u64 {
    pixels.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, p| (h ^ u64::from(*p)).wrapping_mul(0x0100_0000_01b3))
}

impl SuperShuckieFrontendCallbacks for Watch {
    fn refresh_screens(&mut self, screens: &[ScreenData]) {
        let mut seen = self.0.lock().unwrap();
        seen.refreshes += 1;
        seen.last_non_black = screens.iter().map(|s| s.pixels.iter().filter(|p| (**p & 0x00FF_FFFF) != 0).count()).collect();
        seen.last_sizes = screens.iter().map(|s| (s.width, s.height)).collect();
        seen.last_hash = screens.iter().map(|s| hash(&s.pixels)).collect();
        seen.last_pixels = screens.iter().map(|s| (s.width, s.height, s.pixels.clone())).collect();
        if let Some(top) = seen.last_non_black.first().copied() {
            seen.recent_top.push_back(top);
            while seen.recent_top.len() > 24 {
                seen.recent_top.pop_front();
            }
        }
    }
    fn change_video_mode(&mut self, screens: &[ScreenInfo], scale: NonZeroU8) {
        let mut seen = self.0.lock().unwrap();
        seen.modes.push(format!("{} screen(s) {:?} scale {scale}", screens.len(), screens.iter().map(|s| (s.width, s.height, s.encoding)).collect::<Vec<_>>()));
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let rom = std::path::PathBuf::from(args.next().expect("rom path"));
    let mut seconds = 30u64;
    let mut mash = false;
    let mut a_key = 88i32;
    let mut start_key = 67i32;
    let mut speed: Option<f64> = None;
    let mut dump: Option<std::path::PathBuf> = None;
    let mut pokeabyte: Option<u16> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--seconds" => seconds = args.next().unwrap().parse().unwrap(),
            "--mash" => mash = true,
            "--a-key" => a_key = args.next().unwrap().parse().unwrap(),
            "--start-key" => start_key = args.next().unwrap().parse().unwrap(),
            "--speed" => speed = Some(args.next().unwrap().parse().unwrap()),
            "--dump" => dump = Some(args.next().unwrap().into()),
            "--pokeabyte" => pokeabyte = Some(args.next().unwrap().parse().unwrap()),
            other => panic!("unexpected {other}"),
        }
    }
    let dir = std::env::temp_dir().join("supershuckie-n3ds-frontend-check");
    std::fs::create_dir_all(&dir).unwrap();
    let user = dir.join("UserData");

    let seen = Arc::new(Mutex::new(Seen::default()));
    let mut frontend = SuperShuckieFrontend::new(user.clone(), user.clone(), Box::new(Watch(seen.clone())));
    frontend.set_auto_pause_on_record_setting(false);
    if let Some(speed) = speed {
        frontend.set_speed_settings(speed, 2.0);
    }
    match frontend.load_rom(&rom) {
        Ok(()) => println!("loaded {}", rom.display()),
        Err(e) => {
            println!("load failed: {e}");
            return;
        }
    }
    frontend.set_paused(false);
    println!("paused={} running={} mash={mash}", frontend.is_paused(), frontend.is_game_running());
    if let Some(port) = pokeabyte {
        frontend.set_pokeabyte_port(port).expect("pokeabyte port");
        match frontend.set_pokeabyte_enabled(true) {
            Ok(()) => println!("serving Poke-A-Byte on UDP {port}"),
            Err(e) => println!("Poke-A-Byte: {e}"),
        }
    }

    let start = Instant::now();
    let mut next_report = Duration::from_secs(5);
    let mut last_report_hash = Vec::new();
    let mut presses = 0u32;
    let mut key_down: Option<(i32, Instant)> = None;
    let mut next_press = Instant::now() + Duration::from_secs(3);
    while start.elapsed() < Duration::from_secs(seconds) {
        if let Err(e) = frontend.tick() {
            println!("tick error: {e}");
        }
        if mash {
            let now = Instant::now();
            if let Some((key, since)) = key_down {
                if now.duration_since(since) >= Duration::from_millis(60) {
                    frontend.on_user_input(UserInput::Keyboard { keycode: key }, 0.0);
                    key_down = None;
                }
            } else if now >= next_press {
                presses += 1;
                let key = if presses % 12 == 0 { start_key } else { a_key };
                frontend.on_user_input(UserInput::Keyboard { keycode: key }, 1.0);
                key_down = Some((key, now));
                next_press = now + Duration::from_millis(500);
            }
        }
        std::thread::sleep(Duration::from_millis(1));
        if start.elapsed() >= next_report {
            next_report += Duration::from_secs(5);
            let s = seen.lock().unwrap();
            let stats = frontend.get_frame_time_stats();
            let changed = s.last_hash != last_report_hash;
            last_report_hash = s.last_hash.clone();
            if let Some(dir) = &dump {
                std::fs::create_dir_all(dir).unwrap();
                for (i, (w, h, px)) in s.last_pixels.iter().enumerate() {
                    write_bmp(&dir.join(format!("t{:03}-screen{i}.bmp", start.elapsed().as_secs())), *w, *h, px);
                }
            }
            println!(
                "t={:>3}s frames={} paused={} running={} refreshes={} presses={presses} sizes={:?} non_black={:?} changed_since_last_report={changed} avg_frame_us={} max_frame_us={} over_budget={}",
                start.elapsed().as_secs(), frontend.get_elapsed_frames(), frontend.is_paused(), frontend.is_game_running(),
                s.refreshes, s.last_sizes, s.last_non_black, stats.average_frame_micros, stats.max_frame_micros, stats.frames_over_budget
            );
            println!("   recent top-screen non-black counts: {:?}", s.recent_top);
        }
    }
    let s = seen.lock().unwrap();
    for m in &s.modes {
        println!("video mode: {m}");
    }
    println!("done: refreshes={} last non-black per screen {:?}", s.refreshes, s.last_non_black);
}
