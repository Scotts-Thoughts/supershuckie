//! Simulates a display refreshing at a fixed rate against the real frontend and core thread, with
//! no window, and reports how many refreshes each drawn frame would have been on screen for.
//!
//! In `tick` mode (the default presenting path) frames reach the UI as they arrive and the
//! simulated display picks up whatever is newest at each refresh; in `sync` mode the frontend's
//! on-demand presenting is used and `present_latest_frame` is called once per simulated refresh.
//! A 60 fps game on a 120 Hz display should show every frame for 2 refreshes; stretches of 1 and 3
//! are the judder the "Sync display to monitor refresh" option exists to remove.
//!
//! It also reports how many emulated frames the picture moved on by at each refresh (4 at 4x on a
//! 60 Hz display), which is what sped-up play looks like: runs of 0 then 8 are the fast-forward
//! stutter that the core thread's present-phase steering removes (see
//! `SuperShuckieCore::shift_present_phase`). The simulated display is also handed to the core as
//! its display clock, as the system's compositor clock is in the app. Fails when more than 1% of
//! refreshes repeat a picture or jump twice as far as they should.
//!
//! ```text
//! present_pacing_smoke <rom> [--hz 119.98] [--seconds 60] [--speed 1] [--mode tick|sync|both] [--draw-fewer]
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
    tick_until_tracking(frontend, until, None);
}

/// Where the picture handed to the UI last came from: `(presented count seen, emulated frame)`.
type ShownFrame<'a> = Option<(&'a AtomicU64, &'a mut (u64, u64))>;

/// [`tick_until`], also noting the emulated frame of each picture the UI is handed (read right
/// after the tick that handed it over, so a frame or two late on cores that draw only some
/// frames; the caller rounds that off).
fn tick_until_tracking(frontend: &mut SuperShuckieFrontend, until: Instant, mut shown: ShownFrame) {
    loop {
        let now = Instant::now();
        if now >= until {
            return;
        }
        if let Err(e) = frontend.tick() {
            println!("tick error: {e}");
        }
        if let Some((presented, (seen, frame))) = shown.as_mut() {
            let count = presented.load(Ordering::Relaxed);
            if count != *seen {
                *seen = count;
                *frame = frontend.get_screen_frame() as u64;
            }
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
    /// emulated frames advanced between two refreshes -> how many refreshes; at 4x on a 60 Hz
    /// display every refresh should move the game on by exactly 4.
    advance: BTreeMap<u64, u64>,
    /// Seconds into the run of each refresh that moved the game on by other than the ideal.
    uneven_at: Vec<f64>,
}

fn simulate(frontend: &mut SuperShuckieFrontend, presented: &AtomicU64, hz: f64, seconds: f64, sync: bool, speed: f64) -> Outcome {
    frontend.set_present_on_demand(sync);
    let period = Duration::from_secs_f64(1.0 / hz);
    let start = Instant::now();
    let mut next = start + period;
    // The emulator steers its drawn frames by the display's refresh cycle; make that the
    // simulated display (refreshing at `start + n * period`) rather than the real one.
    frontend.set_display_clock_override(Some((start, period)));
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
    let mut shown = (presented.load(Ordering::Relaxed), frontend.get_screen_frame() as u64);
    let mut last_shown_frame: Option<u64> = None;
    let mut advance: BTreeMap<u64, u64> = BTreeMap::new();
    let mut uneven_at = Vec::new();
    let ideal_advance = speed * 60.0 / hz;

    while start.elapsed().as_secs_f64() < seconds {
        tick_until_tracking(frontend, next, Some((presented, &mut shown)));
        next += period;
        refreshes += 1;
        if sync {
            frontend.present_latest_frame(1);
            let count = presented.load(Ordering::Relaxed);
            if count != shown.0 {
                shown = (count, frontend.get_screen_frame() as u64);
            }
        }
        let frame_on_screen = shown.1;
        if Instant::now() >= settle && let Some(last) = last_shown_frame {
            let moved = frame_on_screen.saturating_sub(last);
            *advance.entry(moved).or_default() += 1;
            if (moved as f64 - ideal_advance).abs() >= 1.0 {
                uneven_at.push(start.elapsed().as_secs_f64());
            }
        }
        last_shown_frame = Some(frame_on_screen);
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
    Outcome { histogram, frames, refreshes, longest_bad_run, advance, uneven_at }
}

fn main() {
    request_fine_timer_resolution();
    let mut args = std::env::args().skip(1);
    let rom = std::path::absolute(args.next().expect("usage: present_pacing_smoke <rom> [--hz n] [--seconds n] [--speed n] [--mode tick|sync|both]")).unwrap();
    let mut hz = 119.98f64;
    let mut seconds = 60.0f64;
    let mut speed = 1.0f64;
    let mut mode = "both".to_owned();
    let mut draw_fewer = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--hz" => hz = args.next().unwrap().parse().unwrap(),
            "--seconds" => seconds = args.next().unwrap().parse().unwrap(),
            "--speed" => speed = args.next().unwrap().parse().unwrap(),
            "--mode" => mode = args.next().unwrap(),
            "--draw-fewer" => draw_fewer = true,
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
    frontend.set_nds_draw_fewer_frames_when_sped_up(draw_fewer);
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
        let o = simulate(&mut frontend, &presented, hz, seconds, sync, speed);
        let expected = (hz / 60.0).round().max(1.0) as u64;
        let good = o.histogram.get(&expected).copied().unwrap_or(0);
        println!(
            "{name:<5} {} refreshes, {} frames counted; refreshes-per-frame histogram {:?}; {:.2}% at the expected {expected}; longest run of off-cadence frames {}",
            o.refreshes, o.frames, o.histogram, good as f64 * 100.0 / o.frames.max(1) as f64, o.longest_bad_run
        );
        let ideal = speed * 60.0 / hz;
        let total: u64 = o.advance.values().sum();
        let even = o.advance.iter().filter(|(a, _)| (**a as f64 - ideal).abs() < 1.0).map(|(_, n)| n).sum::<u64>();
        println!(
            "{name:<5} game frames advanced per refresh (ideal {ideal:.2}) {:?}; {:.2}% within one frame of ideal",
            o.advance, even as f64 * 100.0 / total.max(1) as f64
        );
        // Group uneven refreshes less than half a second apart into bursts.
        let mut bursts: Vec<(f64, f64, usize)> = Vec::new();
        for &t in &o.uneven_at {
            match bursts.last_mut() {
                Some((_, end, n)) if t - *end < 0.5 => { *end = t; *n += 1; }
                _ => bursts.push((t, t, 1)),
            }
        }
        let shown: Vec<String> = bursts.iter().map(|(a, b, n)| format!("{a:.1}-{b:.1}s x{n}")).collect();
        println!("{name:<5} uneven bursts: {}", shown.join(", "));
        // A hitch: a refresh that shows the same picture again, or one that jumps at least twice
        // the usual distance (what follows a repeat). Moving on by one frame more or less than
        // the ideal (a display showing 4 of 240 frames in one refresh, 3 or 5 in another) is
        // not one. Only meaningful when every refresh should show a new picture.
        if ideal >= 1.0 {
            let hitches: u64 = o.advance.iter().filter(|(a, _)| **a == 0 || **a as f64 >= ideal * 2.0).map(|(_, n)| n).sum();
            let share = hitches as f64 * 100.0 / total.max(1) as f64;
            println!("{name:<5} hitches (repeat or double step): {hitches} ({share:.2}% of refreshes)");
            if share > 1.0 {
                println!("FAIL: {name} mode hitches on more than 1% of refreshes");
                failed = true;
            }
        }
    }
    println!("RESULT: {}", if failed { "FAIL" } else { "OK" });
    std::process::exit(if failed { 1 } else { 0 });
}
