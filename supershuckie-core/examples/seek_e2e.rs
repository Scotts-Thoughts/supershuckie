//! End-to-end replay seek latency as the timeline sees it: `SuperShuckieCore::go_to_replay_frame`
//! on a real NDS ROM, which reads the keyframe (decompressing its blob if needed), loads the
//! state and emulates up to the target frame with drawing skipped.
//!
//! ```text
//! seek_e2e <rom.nds> <file.replay> [seeks]
//! ```
//!
//! Three series, each `seeks` long:
//! 1. keyframe-exact seeks alternating cold random jumps and short warm forward hops (compare
//!    with `seek_bench`, which does the same on the player alone);
//! 2. warm seeks to keyframe + 3, +30, +60, +119 frames, i.e. the emulate-to-target cost;
//! 3. random arbitrary frames, as a timeline scrub produces them.
//!
//! Link it like `nds_bench` (see that file's header).

use std::time::Instant;

use supershuckie_core::emulator::NintendoDS;
use supershuckie_core::{std_timestamp_provider, SuperShuckieCore};
use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let rom_path = args.next().expect("usage: seek_e2e <rom.nds> <file.replay> [seeks]");
    let replay_path = args.next().expect("usage: seek_e2e <rom.nds> <file.replay> [seeks]");
    let seeks: usize = args.next().map(|s| s.parse().expect("seeks")).unwrap_or(30);

    let rom = std::fs::read(&rom_path).expect("read rom");
    let bytes = std::fs::read(&replay_path).expect("read replay");

    let t = Instant::now();
    let mut player = ReplayFilePlayer::new(&bytes, false).expect("parse replay");
    player.enable_threading();
    let keyframes: Vec<u64> = player.all_keyframes().keys().copied().collect();
    let total = player.get_total_frames();
    println!("{replay_path}: {} frames, {} keyframes, parsed in {:.0} ms", total, keyframes.len(), t.elapsed().as_secs_f64() * 1000.0);

    let nds = NintendoDS::new_from_rom(&rom, None, std_timestamp_provider(), false).expect("load ROM");
    let mut core = SuperShuckieCore::new(Box::new(nds), std_timestamp_provider());
    core.attach_replay_player(player, true).expect("attach");
    core.go_to_replay_frame(3).expect("initial seek");

    let post = SuperShuckieCore::POST_LOAD_FRAMES;
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let seek = |core: &mut SuperShuckieCore, frame: u64| -> f64 {
        let t = Instant::now();
        core.go_to_replay_frame(frame).expect("seek");
        assert_eq!(core.total_frames(), frame, "landed on the wrong frame");
        t.elapsed().as_secs_f64() * 1000.0
    };

    // 1. keyframe-exact: cold random / warm +600 hops.
    println!("\n[1] keyframe-exact seeks (keyframe + {post}), cold random jump then warm +600 hop");
    let (mut cold, mut warm) = (Vec::new(), Vec::new());
    let mut previous: Option<u64> = None;
    for i in 0..seeks {
        let k = match (i % 2, previous) {
            (1, Some(p)) => keyframes.iter().copied().find(|&f| f > p + 600).unwrap_or(keyframes[0]),
            _ => keyframes[(rng.next() as usize) % keyframes.len()],
        };
        let ms = seek(&mut core, k + post);
        if i % 2 == 0 { cold.push(ms) } else { warm.push(ms) }
        println!("  seek to keyframe {k:>8}+{post}: {ms:7.1} ms{}", if i % 2 == 0 { " (cold)" } else { " (warm)" });
        previous = Some(k);
    }
    let avg = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
    println!("  cold average {:.1} ms, warm average {:.1} ms", avg(&cold), avg(&warm));

    // 2. emulate-to-target cost: warm seeks to keyframe + offset.
    println!("\n[2] warm seeks to keyframe + offset (same blob), offset cost = emulation after the state load");
    for &offset in &[post, 30, 60, 119] {
        let mut v = Vec::new();
        for _ in 0..(seeks / 3).max(3) {
            // A keyframe well inside the current blob region: hop forward a little each time.
            let cur = core.total_frames();
            let k = keyframes.iter().copied().find(|&f| f > cur + 200).unwrap_or(keyframes[0]);
            v.push(seek(&mut core, k + offset));
        }
        println!("  keyframe + {offset:>3}: average {:6.1} ms, worst {:6.1} ms", avg(&v), v.iter().cloned().fold(0.0, f64::max));
    }

    // 3. random arbitrary frames (timeline scrub).
    println!("\n[3] random arbitrary frames (what a timeline drag produces)");
    let mut v = Vec::new();
    for _ in 0..seeks {
        let frame = post + (rng.next() % (total - post - 1));
        let ms = seek(&mut core, frame);
        v.push(ms);
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!("  average {:.1} ms, median {:.1} ms, p90 {:.1} ms, worst {:.1} ms", avg(&v), v[v.len() / 2], v[v.len() * 9 / 10], v[v.len() - 1]);
}
