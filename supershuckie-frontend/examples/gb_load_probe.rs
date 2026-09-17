//! Probe: load a ROM through the frontend the way the app does and report whether frames and
//! screen refreshes advance under a given base speed and audio setting.
//!
//! ```text
//! gb_load_probe <rom> [base_speed] [audio:0|1]
//! ```

use std::num::NonZeroU8;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use supershuckie_core::emulator::ScreenData;
use supershuckie_frontend::{ScreenInfo, SuperShuckieFrontend, SuperShuckieFrontendCallbacks};

struct Counter(Arc<AtomicU64>, Arc<AtomicU64>);

impl SuperShuckieFrontendCallbacks for Counter {
    fn refresh_screens(&mut self, screens: &[ScreenData]) {
        self.0.fetch_add(1, Ordering::Relaxed);
        if let Some(s) = screens.first() {
            let sum: u64 = s.pixels.iter().map(|p| *p as u64).sum();
            self.1.store(sum, Ordering::Relaxed);
        }
    }
    fn change_video_mode(&mut self, _: &[ScreenInfo], _: NonZeroU8) {}
}

fn main() {
    let mut args = std::env::args().skip(1);
    let rom = std::path::absolute(args.next().expect("usage: gb_load_probe <rom> [speed] [audio]")).unwrap();
    let speed: f64 = args.next().map(|s| s.parse().unwrap()).unwrap_or(1.0);
    let audio = args.next().map(|s| s == "1").unwrap_or(false);
    let seed: Option<std::path::PathBuf> = args.next().map(Into::into);

    let dir = std::env::temp_dir().join("supershuckie-gb-load-probe");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    if let Some(seed) = seed.as_ref() {
        // Mirror an existing UserData folder (settings + per-ROM data) so the run sees what the app sees.
        fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
            std::fs::create_dir_all(to).unwrap();
            for entry in std::fs::read_dir(from).unwrap() {
                let entry = entry.unwrap();
                let target = to.join(entry.file_name());
                if entry.file_type().unwrap().is_dir() { copy_dir(&entry.path(), &target); }
                else { std::fs::copy(entry.path(), target).unwrap(); }
            }
        }
        copy_dir(seed, &dir.join("data"));
        copy_dir(seed, &dir.join("config"));
    }

    let refreshes = Arc::new(AtomicU64::new(0));
    let checksum = Arc::new(AtomicU64::new(0));
    let mut frontend = SuperShuckieFrontend::new(dir.join("data"), dir.join("config"), Box::new(Counter(refreshes.clone(), checksum.clone())));
    if seed.is_none() {
        frontend.set_speed_settings(speed, 2.0);
        frontend.set_audio_enabled(audio);
    }
    frontend.load_rom(&rom).expect("load rom");
    frontend.set_paused(false);

    let start = Instant::now();
    let mut last_report = Instant::now();
    while start.elapsed() < Duration::from_secs(6) {
        frontend.tick().expect("tick");
        std::thread::sleep(Duration::from_millis(1));
        if last_report.elapsed() >= Duration::from_secs(1) {
            last_report = Instant::now();
            println!(
                "t={:.1}s frames={} refreshes={} checksum={} paused={}",
                start.elapsed().as_secs_f64(),
                frontend.get_elapsed_frames(),
                refreshes.load(Ordering::Relaxed),
                checksum.load(Ordering::Relaxed),
                frontend.is_paused()
            );
        }
    }
}
