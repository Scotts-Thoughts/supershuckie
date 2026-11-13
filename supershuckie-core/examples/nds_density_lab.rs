//! What keyframes denser than the recorded ones would cost on a Nintendo DS replay, and what
//! taking them costs. Research tool (2026-11-11).
//!
//! Plays a stretch of a replay from a keyframe with drawing skipped, captures a save state every
//! `--step` frames, and reports for each spacing (step x 1, 2, 4, ...) the compressed size of
//! the masked delta chain, plus the time a capture, a diff and a hidden frame take.
//!
//! ```text
//! nds_density_lab <rom.nds> <file.replay> [--start F] [--frames N] [--step 15]
//! ```
//!
//! Link it like `nds_bench` (see that file's header).

use std::time::Instant;

use supershuckie_core::emulator::NintendoDS;
use supershuckie_core::{std_timestamp_provider, SuperShuckieCore};
use supershuckie_replay_recorder::keyframe_masks::transient_ranges;
use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::replay_file::ReplayConsoleType;
use supershuckie_replay_recorder::{compress_data, region_diff};

fn main() {
    let mut args = std::env::args().skip(1);
    let rom_path = args.next().expect("usage: nds_density_lab <rom.nds> <file.replay> [--start F] [--frames N] [--step 15]");
    let replay_path = args.next().expect("replay");
    let (mut start, mut frames, mut step) = (0u64, 7200u64, 15u64);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--start" => start = args.next().unwrap().parse().unwrap(),
            "--frames" => frames = args.next().unwrap().parse().unwrap(),
            "--step" => step = args.next().unwrap().parse().unwrap(),
            other => panic!("unknown argument {other}"),
        }
    }

    let rom = std::fs::read(&rom_path).expect("read rom");
    let bytes = std::fs::read(&replay_path).expect("read replay");
    let mut player = ReplayFilePlayer::new(&bytes, false).expect("parse replay");
    // A format-v11 recording's keyframes may copy bytes from the ROM.
    if player.wants_rom() {
        player.set_rom(std::sync::Arc::new(rom.to_vec()));
    }
    player.enable_threading();
    let nds = NintendoDS::new_from_rom(&rom, None, std_timestamp_provider(), false).expect("load ROM");
    let mut core = SuperShuckieCore::new(Box::new(nds), std_timestamp_provider());
    core.attach_replay_player(player, true).expect("attach");
    core.go_to_replay_keyframe(start).expect("seek");
    let first = core.total_frames();

    let strides: Vec<u64> = (0..).map(|i| 1u64 << i).take_while(|s| s * step <= 480).collect();
    let mut last: Vec<Option<Vec<u8>>> = vec![None; strides.len()];
    let mut streams: Vec<Vec<u8>> = vec![Vec::new(); strides.len()];
    let mut kept = vec![0usize; strides.len()];
    let mut raw = vec![0usize; strides.len()];
    let mut transient: Option<(std::ops::Range<usize>, Vec<u8>)> = None;
    let (mut run_s, mut save_s, mut diff_s) = (0.0f64, 0.0f64, 0.0f64);
    let (mut saves, mut diffs) = (0usize, 0usize);
    let mut state = Vec::new();

    let mut captures = 0u64;
    while core.total_frames() < first + frames && !core.is_replay_stalled() {
        let t = Instant::now();
        for _ in 0..step {
            core.run_unlocked_hidden();
        }
        run_s += t.elapsed().as_secs_f64();

        let t = Instant::now();
        core.get_core().create_save_state_into(&mut state);
        save_s += t.elapsed().as_secs_f64();
        saves += 1;

        match &transient {
            None => {
                if let Some(range) = transient_ranges(ReplayConsoleType::NintendoDS, &state).into_iter().next() {
                    transient = Some((range.clone(), state[range].to_vec()));
                }
            }
            Some((range, bytes)) => state[range.clone()].copy_from_slice(bytes),
        }

        for (i, stride) in strides.iter().enumerate() {
            if captures % stride != 0 {
                continue;
            }
            if let Some(prev) = &last[i] {
                if prev.len() == state.len() {
                    let t = Instant::now();
                    let diff = region_diff(prev, &state).expect("same length");
                    if i == 0 {
                        diff_s += t.elapsed().as_secs_f64();
                        diffs += 1;
                    }
                    streams[i].extend_from_slice(&diff.control);
                    streams[i].extend_from_slice(&diff.data);
                    raw[i] += diff.data.len();
                    kept[i] += 1;
                }
            }
            last[i] = Some(state.clone());
        }
        captures += 1;
    }

    let done = core.total_frames() - first;
    let hours = done as f64 / 215_374.0;
    println!(
        "{replay_path}: frames {first}..{} ({:.1} min); hidden frame {:.2} ms ({:.0} fps); save state {:.2} ms ({} bytes); region_diff {:.2} ms",
        first + done,
        done as f64 / 3590.0,
        run_s * 1000.0 / done as f64,
        done as f64 / run_s,
        save_s * 1000.0 / saves as f64,
        state.len(),
        diff_s * 1000.0 / diffs.max(1) as f64
    );
    println!("  spacing   deltas   changed bytes/delta   compressed/delta   MiB/hour   raw MiB/hour");
    for (i, stride) in strides.iter().enumerate() {
        if kept[i] == 0 {
            continue;
        }
        let c = compress_data(&streams[i], 3).unwrap().len();
        println!(
            "  {:>4} fr  {:>7}  {:>18.0}  {:>17.0}  {:>9.1}  {:>12.1}",
            stride * step,
            kept[i],
            raw[i] as f64 / kept[i] as f64,
            c as f64 / kept[i] as f64,
            c as f64 / 1048576.0 / hours,
            streams[i].len() as f64 / 1048576.0 / hours
        );
    }
}
