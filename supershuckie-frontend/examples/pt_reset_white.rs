//! Repro for "in Play Together, resetting turns my screen white": a host and a joiner follow each
//! other, then one of them resets in several ways, and both the resetter's own screen and the
//! other player's view of it are sampled for how white they are, and whether they still change.
//!
//! ```text
//! pt_reset_white <rom> [--speed 1] [--seconds 8]
//! ```

use std::num::NonZeroU8;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use supershuckie_core::emulator::ScreenData;
use supershuckie_frontend::play_together::PeerId;
use supershuckie_frontend::{ScreenInfo, SuperShuckieFrontend, SuperShuckieFrontendCallbacks};

#[derive(Default, Clone, Copy, Debug)]
struct Pic {
    hash: u64,
    white: f64,
    deliveries: u64,
}

fn pic_of(screens: &[ScreenData], prev: &Pic) -> Pic {
    let mut hash: u64 = 0xcbf29ce484222325;
    let mut white = 0usize;
    let mut total = 0usize;
    for s in screens {
        for &p in &s.pixels {
            hash = (hash ^ u64::from(p)).wrapping_mul(0x100000001b3);
            let (r, g, b) = ((p >> 16) & 0xFF, (p >> 8) & 0xFF, p & 0xFF);
            if r >= 0xE0 && g >= 0xE0 && b >= 0xE0 {
                white += 1;
            }
            total += 1;
        }
    }
    Pic { hash, white: white as f64 / total.max(1) as f64, deliveries: prev.deliveries + 1 }
}

#[derive(Default)]
struct Cb {
    own: Arc<Mutex<Pic>>,
    peer: Arc<Mutex<Pic>>,
}

impl SuperShuckieFrontendCallbacks for Cb {
    fn refresh_screens(&mut self, screens: &[ScreenData]) {
        let mut own = self.own.lock().unwrap();
        *own = pic_of(screens, &own);
    }
    fn change_video_mode(&mut self, _: &[ScreenInfo], _: NonZeroU8) {}
    fn peer_refresh_screens(&mut self, _: PeerId, screens: &[ScreenData]) {
        let mut peer = self.peer.lock().unwrap();
        *peer = pic_of(screens, &peer);
    }
}

struct Player {
    name: &'static str,
    fe: SuperShuckieFrontend,
    own: Arc<Mutex<Pic>>,
    peer: Arc<Mutex<Pic>>,
}

fn make(dir: &Path, name: &'static str, rom: &Path, speed: f64) -> Player {
    let user = dir.join(name);
    let _ = std::fs::remove_dir_all(&user);
    std::fs::create_dir_all(&user).unwrap();
    std::fs::write(
        user.join("settings.json"),
        r#"{"pokeabyte": {"enabled": false}, "external_commands": {"enabled": false}, "play_together": {"display_name": "NAME", "bind_address": "127.0.0.1"}}"#.replace("NAME", name)
    ).unwrap();
    let cb = Cb::default();
    let (own, peer) = (cb.own.clone(), cb.peer.clone());
    let mut fe = SuperShuckieFrontend::new(user.clone(), user.clone(), Box::new(cb));
    fe.set_speed_settings(speed, 2.0);
    fe.set_auto_pause_on_record_setting(false);
    fe.load_rom(rom).expect("load rom");
    fe.set_paused(false);
    Player { name, fe, own, peer }
}

fn tick_for(ps: &mut [Player], d: Duration) {
    let until = Instant::now() + d;
    while Instant::now() < until {
        for p in ps.iter_mut() {
            if let Err(e) = p.fe.tick() {
                println!("[{}] tick error: {e}", p.name);
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Sample for `seconds`: the resetter's own picture and the other player's view of it.
fn watch(ps: &mut [Player], resetter: usize, seconds: f64) {
    let other = 1 - resetter;
    let start = Instant::now();
    let mut next = 0.0;
    while start.elapsed().as_secs_f64() < seconds {
        tick_for(ps, Duration::from_millis(20));
        let t = start.elapsed().as_secs_f64();
        if t >= next {
            next += 0.5;
            let own = *ps[resetter].own.lock().unwrap();
            let view = *ps[other].peer.lock().unwrap();
            let st = ps[other].fe.play_together_state();
            let status = st.participants.first().map(|q| format!("{}:{}", q.status, q.status_text)).unwrap_or_default();
            println!(
                "  t={t:4.1}s own: white {:5.1}% hash {:016x} n={:5} paused={} | {}'s view: white {:5.1}% hash {:016x} n={:5} {status}",
                own.white * 100.0, own.hash, own.deliveries, ps[resetter].fe.is_paused(),
                ps[other].name, view.white * 100.0, view.hash, view.deliveries
            );
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let rom: PathBuf = std::path::absolute(args.next().expect("rom")).unwrap();
    let mut speed = 1.0;
    let mut seconds = 8.0;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--speed" => speed = args.next().unwrap().parse().unwrap(),
            "--seconds" => seconds = args.next().unwrap().parse().unwrap(),
            o => panic!("unexpected {o}"),
        }
    }
    #[cfg(windows)]
    {
        #[link(name = "winmm")]
        unsafe extern "system" { fn timeBeginPeriod(p: u32) -> u32; }
        unsafe { timeBeginPeriod(1); }
    }
    let dir = std::env::temp_dir().join("supershuckie-pt-reset-white");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut ps = vec![make(&dir, "Host", &rom, speed), make(&dir, "Ash", &rom, speed)];
    tick_for(&mut ps, Duration::from_secs(3));
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let code = ps[0].fe.play_together_host(port, "Host", 0).expect("host").to_string();
    let port = code.rsplit(':').next().unwrap().to_owned();
    tick_for(&mut ps, Duration::from_millis(200));
    ps[1].fe.play_together_join(&format!("127.0.0.1:{port}"), "Ash", 0).expect("join");
    let t = Instant::now();
    while !ps.iter().all(|p| p.fe.play_together_state().participants.iter().any(|q| q.status == "following" || q.status == "waiting")) {
        tick_for(&mut ps, Duration::from_millis(10));
        assert!(t.elapsed() < Duration::from_secs(30), "never following");
    }
    tick_for(&mut ps, Duration::from_secs(2));

    for resetter in [1usize, 0] {
        let name = ps[resetter].name;
        println!("== {name} resets while running");
        watch(&mut ps, resetter, 1.0);
        ps[resetter].fe.hard_reset_console();
        watch(&mut ps, resetter, seconds);

        println!("== {name} pauses, resets, unpauses (sync pause off)");
        ps[resetter].fe.set_paused(true);
        tick_for(&mut ps, Duration::from_millis(300));
        ps[resetter].fe.hard_reset_console();
        watch(&mut ps, resetter, 2.0);
        ps[resetter].fe.set_paused(false);
        watch(&mut ps, resetter, seconds);
    }

    println!("== sync pause on, Ash pauses, resets, unpauses");
    ps[0].fe.set_play_together_sync_pause(true).expect("sync pause");
    tick_for(&mut ps, Duration::from_millis(300));
    ps[1].fe.set_paused(true);
    tick_for(&mut ps, Duration::from_millis(300));
    ps[1].fe.hard_reset_console();
    watch(&mut ps, 1, 2.0);
    ps[1].fe.set_paused(false);
    watch(&mut ps, 1, seconds);

    println!("== race start (reset all)");
    ps[0].fe.play_together_reset_all(1).expect("reset all");
    watch(&mut ps, 1, seconds + 1.0);

    for p in ps.iter_mut() {
        p.fe.play_together_leave();
    }
    tick_for(&mut ps, Duration::from_millis(300));
}
