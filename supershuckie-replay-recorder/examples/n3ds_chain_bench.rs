//! How long a Nintendo 3DS keyframe chain step costs when applied in place on one working
//! buffer (decompress + apply) versus cloning the reference state first, as the player does
//! today. Research tool for `replay-3ds-format-research.md`.
//!
//! ```text
//! n3ds_chain_bench <file.replay> [--from N] [--count N]
//! ```

use std::time::Instant;

use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::{apply_region_diff_in_place, decompress_data, decompress_data_with_prefix, Packet};

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage");
    let (mut from, mut count) = (0usize, 120usize);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--from" => from = args.next().unwrap().parse().unwrap(),
            "--count" => count = args.next().unwrap().parse().unwrap(),
            o => panic!("unknown {o}")
        }
    }
    let bytes = std::fs::read(&path).expect("read");
    let player = ReplayFilePlayer::new(&bytes[..], false).expect("parse");
    let kfs: Vec<(u8, usize, usize, usize, usize)> = player.all_uncompressed_packets().iter().filter_map(|p| match p {
        Packet::StoredKeyframe { level, state_len, uncompressed_len, frame_len, frame_offset, .. } =>
            Some((*level, *state_len as usize, *uncompressed_len as usize, *frame_len as usize, *frame_offset as usize)),
        _ => None
    }).collect();
    drop(player);

    // Walk every keyframe as if it were one chain of consecutive deltas is not possible (the file
    // has levels), so time each file delta against its own reference, both ways.
    let mut states: [Option<Vec<u8>>; 3] = [None, None, None];
    let mut payloads: [Vec<u8>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    let (mut dec, mut clone, mut apply, mut n) = (0f64, 0f64, 0f64, 0usize);
    for (k, &(level, state_len, ulen, flen, off)) in kfs.iter().enumerate() {
        if k >= from + count { break; }
        let fb = &bytes[off..off + flen];
        let l = usize::from(level);
        if level == 0 {
            let s = decompress_data(fb, ulen).unwrap();
            for m in 0..3 { states[m] = Some(s.clone()); payloads[m].clear(); }
            continue;
        }
        let t = Instant::now();
        let payload = decompress_data_with_prefix(fb, ulen, &payloads[l]).unwrap();
        let d = t.elapsed().as_secs_f64() * 1000.0;
        let t = Instant::now();
        let src = states[l].as_ref().unwrap();
        // A working buffer with slack, so a state a few KB longer does not reallocate.
        let mut s: Vec<u8> = Vec::with_capacity(src.len() + (1 << 20));
        s.extend_from_slice(src);
        let c = t.elapsed().as_secs_f64() * 1000.0;
        let t = Instant::now();
        s.resize(state_len, 0);
        let cl = u64::from_le_bytes(payload[..8].try_into().unwrap()) as usize;
        assert!(apply_region_diff_in_place(&mut s, &payload[8..8 + cl], &payload[8 + cl..]));
        let a = t.elapsed().as_secs_f64() * 1000.0;
        if k >= from { dec += d; clone += c; apply += a; n += 1; }
        for m in l..3 { payloads[m] = payload.clone(); }
        // Keep the level states distinct allocations, as the player does.
        for m in l..3 { states[m] = Some(if m == l { s.clone() } else { s.clone() }); }
    }
    let n = n.max(1) as f64;
    println!("{} deltas: decompress {:.2} ms, clone of the reference state {:.2} ms, apply in place {:.2} ms (per step)", n, dec / n, clone / n, apply / n);
}
