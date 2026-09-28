//! Time keyframe materialisation (the part of a seek that happens in the replay player) on a
//! Nintendo 3DS replay, without an emulator.
//!
//! ```text
//! n3ds_seek_bench <file.replay> [--rom <game file>] [--random N] [--seed S]
//! ```
//!
//! Three passes: `--random N` seeks to random keyframes; then every keyframe of the file from
//! the last to the first (backward seeks, the worst case for delta chains); then every keyframe
//! from the first (a forward walk, the best case). Each prints the average and worst time per
//! seek and the deltas applied.

use std::sync::Arc;
use std::time::Instant;

use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::Packet;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: n3ds_seek_bench <file.replay> [--rom <file>] [--random N] [--seed S]");
    let (mut rom, mut random, mut seed) = (None::<String>, 200usize, 1u64);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--rom" => rom = args.next(),
            "--random" => random = args.next().unwrap().parse().unwrap(),
            "--seed" => seed = args.next().unwrap().parse().unwrap(),
            o => panic!("unknown argument {o}")
        }
    }

    let file = std::fs::File::open(&path).expect("open");
    // SAFETY: read-only mapping of a file nobody writes while this runs.
    let map = unsafe { memmap2::Mmap::map(&file) }.expect("map");
    let source: Arc<dyn AsRef<[u8]> + Send + Sync> = Arc::new(map);
    let t = Instant::now();
    let mut player = ReplayFilePlayer::new_shared(source, false).expect("parse");
    println!("parsed in {:.0} ms: version {}, {} frames, {} keyframes", t.elapsed().as_secs_f64() * 1000.0,
             player.get_replay_version(), player.get_total_frames(), player.all_keyframes().len());
    if let Some(rom) = rom {
        let file = std::fs::File::open(&rom).expect("open ROM");
        // SAFETY: as above.
        let map = unsafe { memmap2::Mmap::map(&file) }.expect("map ROM");
        player.set_rom(Arc::new(map));
    }
    player.set_keyframe_states_wanted(false);

    let frames: Vec<u64> = player.all_keyframes().keys().copied().collect();
    let mut rng = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut next = || { rng ^= rng << 13; rng ^= rng >> 7; rng ^= rng << 17; rng };
    let random_order: Vec<u64> = (0..random).map(|_| frames[(next() % frames.len() as u64) as usize]).collect();
    let backward: Vec<u64> = frames.iter().rev().copied().collect();

    for (name, order) in [("random", &random_order), ("backward", &backward), ("forward", &frames)] {
        let (mut total, mut worst, mut worst_frame, mut folds) = (0f64, 0f64, 0u64, 0u64);
        for &frame in order {
            let before = player.chain_folds();
            let t = Instant::now();
            player.go_to_keyframe(frame).expect("seek");
            match player.next_packet() {
                Ok(Some(Packet::Keyframe { metadata, .. })) => assert_eq!(metadata.elapsed_frames, frame),
                other => panic!("seek to {frame}: {other:?}")
            }
            assert!(!player.current_keyframe_state().is_empty());
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            total += ms;
            if ms > worst {
                worst = ms;
                worst_frame = frame;
            }
            folds += player.chain_folds() - before;
        }
        let n = order.len().max(1) as f64;
        println!("{name:>8}: {} seeks, {:.1} ms average, worst {:.1} ms (frame {worst_frame}), {:.1} keyframes decoded per seek",
                 order.len(), total / n, worst, folds as f64 / n);
    }
}
