//! Simulates a display refreshing at a fixed rate against the real frontend and core thread, with
//! no window, and reports how many refreshes each drawn frame would have been on screen for.
//!
//! In `tick` mode (the default presenting path) frames reach the UI as they arrive and the
//! simulated display picks up whatever is newest at each refresh; in `sync` mode the frontend's
//! on-demand presenting is used and `present_latest_frame` is called once per simulated refresh.
//! A 60 fps game on a 120 Hz display should show every frame for 2 refreshes; stretches of 1 and 3
//! are the judder the "Sync display to monitor refresh" option exists to remove.
//!
//! ```text
//! present_pacing_smoke <rom> [--hz 119.98] [--seconds 60] [--speed 1] [--mode tick|sync|both]
//! ```
//!
//! Link it like `supershuckie-core`'s `nds_bench` (see that file's header).

use std::collections::BTreeMap;
use std::num::NonZeroU8;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use supershuckie_core::emulator::ScreenData;
use supershuckie_frontend::{ScreenInfo, SuperShuckieFrontend, SuperShuckieFrontendCallbacks};

struct CountingScreen(Arc<AtomicU64>);

impl SuperShuckieFrontendCallbacks for CountingScreen {
    fn refresh_screens(&mut self, _: &[ScreenData]) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
    fn change_video_mode(&mut self, _: &[ScreenInfo], _: NonZeroU8) {}
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

/// Tick the frontend every millisecond until `until`, then spin for the last stretch so the
/// simulated refresh lands within a few microseconds of where it should.
fn tick_until(frontend: &mut SuperShuckieFrontend, until: Instant) {
    loop {
        let now = Instant::now();
        if now >= until {
            return;
        }
        if let Err(e) = frontend.tick() {
            println!("tick error: {e}");
        }
        let left = until - now;
        if left > Duration::from_micros(1500) {
            std::thread::sleep(Duration::from_millis(1));
        }
        else {
            while Instant::now() < until {
                std::hint::spin_loop();
            }
            return;
        }
    }
}

struct Outcome {
    /// refreshes-on-screen -> how many frames were shown for that long
    histogram: BTreeMap<u64, u64>,
    frames: u64,
    refreshes: u64,
    /// Longest run of consecutive frames whose on-screen time was not the expected one.
    longest_bad_run: u64,
}

fn simulate(frontend: &mut SuperShuckieFrontend, presented: &AtomicU64, hz: f64, seconds: f64, sync: bool) -> Outcome {
    frontend.set_present_on_demand(sync);
    let period = Duration::from_secs_f64(1.0 / hz);
    let start = Instant::now();
    let mut next = start + period;
    let mut refreshes = 0u64;
    let mut last_count = presented.load(Ordering::Relaxed);
    let mut since_present = 0u64;
    let mut histogram: BTreeMap<u64, u64> = BTreeMap::new();
    let mut frames = 0u64;
    let mut run = 0u64;
    let mut longest_bad_run = 0u64;
    // Settle for a second before counting (the pacer measures its ratio over the first second).
    let settle = start + Duration::from_secs(1);
    let mut expected: Option<u64> = None;

    while start.elapsed().as_secs_f64() < seconds {
        tick_until(frontend, next);
        next += period;
        refreshes += 1;
        if sync {
            frontend.present_latest_frame();
        }
        since_present += 1;
        let count = presented.load(Ordering::Relaxed);
        if count != last_count {
            // A new frame is on screen from this refresh on; the previous one showed for
            // `since_present` refreshes.
            last_count = count;
            if Instant::now() >= settle {
                if expected.is_none() {
                    expected = Some((hz / 60.0).round().max(1.0) as u64);
                }
                *histogram.entry(since_present).or_default() += 1;
                frames += 1;
                if Some(since_present) != expected {
                    run += 1;
                    longest_bad_run = longest_bad_run.max(run);
                }
                else {
                    run = 0;
                }
            }
            since_present = 0;
        }
    }
    frontend.set_present_on_demand(false);
    Outcome { histogram, frames, refreshes, longest_bad_run }
}

fn main() {
    request_fine_timer_resolution();
    let mut args = std::env::args().skip(1);
    let rom = std::path::absolute(args.next().expect("usage: present_pacing_smoke <rom> [--hz n] [--seconds n] [--speed n] [--mode tick|sync|both]")).unwrap();
    let mut hz = 119.98f64;
    let mut seconds = 60.0f64;
    let mut speed = 1.0f64;
    let mut mode = "both".to_owned();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--hz" => hz = args.next().unwrap().parse().unwrap(),
            "--seconds" => seconds = args.next().unwrap().parse().unwrap(),
            "--speed" => speed = args.next().unwrap().parse().unwrap(),
            "--mode" => mode = args.next().unwrap(),
            other => panic!("unexpected {other}"),
        }
    }

    let dir = std::env::temp_dir().join("supershuckie-present-pacing-smoke");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let user = dir.join("UserData");

    let presented = Arc::new(AtomicU64::new(0));
    let mut frontend = SuperShuckieFrontend::new(user.clone(), user.clone(), Box::new(CountingScreen(presented.clone())));
    frontend.set_speed_settings(speed, 2.0);
    frontend.load_rom(&rom).expect("load rom");
    frontend.set_paused(false);
    println!("{} at {speed}x, simulated display {hz} Hz, {seconds} s per mode", rom.display());
    tick_until(&mut frontend, Instant::now() + Duration::from_secs(5));

    let modes: Vec<bool> = match mode.as_str() {
        "tick" => vec![false],
        "sync" => vec![true],
        _ => vec![false, true],
    };
    let mut failed = false;
    for sync in modes {
        let name = if sync { "sync" } else { "tick" };
        let o = simulate(&mut frontend, &presented, hz, seconds, sync);
        let expected = (hz / 60.0).round().max(1.0) as u64;
        let good = o.histogram.get(&expected).copied().unwrap_or(0);
        println!(
            "{name:<5} {} refreshes, {} frames counted; refreshes-per-frame histogram {:?}; {:.2}% at the expected {expected}; longest run of off-cadence frames {}",
            o.refreshes, o.frames, o.histogram, good as f64 * 100.0 / o.frames.max(1) as f64, o.longest_bad_run
        );
        if sync && expected >= 2 && o.longest_bad_run > 2 {
            println!("FAIL: sync mode still shows runs of off-cadence frames");
            failed = true;
        }
    }
    println!("RESULT: {}", if failed { "FAIL" } else { "OK" });
    std::process::exit(if failed { 1 } else { 0 });
}
