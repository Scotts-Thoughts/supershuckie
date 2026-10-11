//! Seeks around a Nintendo DS replay through `SuperShuckieCore` the ways the app's timeline does
//! (exact clicks, coarse drag steps, stepping, playing on between seeks) and reports every seek
//! that fails. A "cannot load the save state at frame N" hunting aid.
//!
//! ```text
//! nds_seek_repro <rom.nds> <file.replay> [--near frame] [--random n] [--play frames] [--seed n]
//! ```
//!
//! `--near` adds seeks to frames around that one, from before and after it; `--random` adds n
//! random seeks (alternating exact and coarse, each followed by `--play` frames of playback,
//! default 30).

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
    let usage = "usage: nds_seek_repro <rom.nds> <file.replay> [--near frame] [--random n] [--play frames] [--seed n]";
    let rom_path = args.next().expect(usage);
    let replay_path = args.next().expect(usage);
    let (mut near, mut random, mut play, mut seed) = (None, 0usize, 30u64, 0x9E37_79B9_7F4A_7C15u64);
    while let Some(a) = args.next() {
        let mut value = || args.next().unwrap_or_else(|| panic!("{a} needs a value"));
        match a.as_str() {
            "--near" => near = Some(value().parse::<u64>().expect("--near")),
            "--random" => random = value().parse().expect("--random"),
            "--play" => play = value().parse().expect("--play"),
            "--seed" => seed = value().parse().expect("--seed"),
            other => panic!("unknown option {other}\n{usage}"),
        }
    }

    let rom = std::fs::read(&rom_path).expect("read rom");
    let bytes = std::fs::read(&replay_path).expect("read replay");
    let nds = NintendoDS::new_from_rom(&rom, None, std_timestamp_provider(), false).expect("load rom");
    let mut core = SuperShuckieCore::new(Box::new(nds), std_timestamp_provider());
    let player = ReplayFilePlayer::new(&bytes, false).expect("parse replay");
    let total = player.get_total_frames();
    core.attach_replay_player(player, true).expect("attach");
    println!("{replay_path}: {total} frames");

    if std::env::var_os("SEEK_TRACE").is_some() {
        // Step by step: what the first re-seek to frame 0 does.
        println!("after attach: frame {}, resyncs {}", core.total_frames(), core.replay_hidden_state_resyncs());
        let k = core.go_to_replay_keyframe(0).expect("keyframe 0");
        println!("loaded keyframe {k}: frame {}, resyncs {}", core.total_frames(), core.replay_hidden_state_resyncs());
        for _ in 0..4 {
            core.run_unlocked_hidden();
            println!("ran hidden: frame {}, resyncs {}", core.total_frames(), core.replay_hidden_state_resyncs());
        }
        for _ in 0..4 {
            core.run_unlocked();
            println!("ran drawn: frame {}, resyncs {}", core.total_frames(), core.replay_hidden_state_resyncs());
        }
    }

    let mut failures = 0u64;
    let mut seeks = 0u64;
    let mut seek = |core: &mut SuperShuckieCore, frame: u64, coarse: bool, what: &str| {
        seeks += 1;
        let before = core.total_frames();
        if std::env::var_os("SEEK_VERBOSE").is_some() {
            println!("{what} {} seek {before} -> {frame}", if coarse { "coarse" } else { "exact" });
        }
        let result = if coarse { core.go_to_replay_frame_coarse(frame) } else { core.go_to_replay_frame(frame) };
        if let Err(e) = result {
            failures += 1;
            println!("{what} {} seek {before} -> {frame} FAILED: {e}", if coarse { "coarse" } else { "exact" });
            // The app keeps going after an error; a later seek unstalls playback.
        }
    };
    let run = |core: &mut SuperShuckieCore, frames: u64| {
        for _ in 0..frames {
            if core.is_replay_stalled() {
                break;
            }
            core.run_unlocked_hidden();
        }
    };

    if let Some(n) = near {
        // Around the frame: exact clicks from far before and far after, coarse drag steps across
        // it, stepping over it, playing through it.
        let offsets: [i64; 12] = [-240, -121, -120, -60, -1, 0, 1, 4, 5, 30, 119, 240];
        for &o in &offsets {
            let f = (n as i64 + o).max(0) as u64;
            seek(&mut core, 0, false, "reset");
            seek(&mut core, f, false, "from start");
            seek(&mut core, total.saturating_sub(10), false, "to end");
            seek(&mut core, f, false, "from end");
        }
        for f in (n.saturating_sub(600)..n + 600).step_by(37) {
            seek(&mut core, f, true, "drag");
        }
        seek(&mut core, n + 10, false, "drag end");
        seek(&mut core, n.saturating_sub(300), false, "before");
        run(&mut core, 700);
        println!("played {} -> {}", n.saturating_sub(300), core.total_frames());
        for d in [-1i64, -1, -1, -5, 1, 1, 60, -60, -120] {
            let target = (core.total_frames() as i64 + d).max(0) as u64;
            seek(&mut core, target, false, "step");
        }
    }

    let mut rng = Rng(seed);
    for i in 0..random {
        let frame = rng.next() % total;
        seek(&mut core, frame, i % 2 == 1, "random");
        run(&mut core, play);
    }

    println!("{seeks} seeks, {failures} failed");
}
