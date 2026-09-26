//! Keyframe topology experiments on a real Nintendo 3DS replay (format v8): what each keyframe
//! would cost as a delta against a fixed anchor (every W keyframes) instead of a chain, where the
//! bytes of a chain delta come from (RAM regions vs Azahar's archive), and what a full keyframe
//! costs at other zstd levels. Research tool for `replay-3ds-format-research.md`.
//!
//! ```text
//! n3ds_topology_lab <file.replay> [--anchors 8,15,30] [--from N] [--count N] [--l0-levels 3,9,15]
//! ```

use std::ffi::c_void;
use std::time::Instant;

use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::{apply_region_diff_in_place, decompress_data, decompress_data_with_prefix, region_diff_resizing, Packet};
use zstd_sys::*;

const RAM_END: usize = 32 + (128 << 20) + (6 << 20) + (512 << 10);

fn zstd_len(data: &[u8], prefix: &[u8], level: i32, window_log: i32, workers: i32) -> (usize, f64) {
    let t = Instant::now();
    unsafe {
        let cctx = ZSTD_createCCtx();
        ZSTD_CCtx_setParameter(cctx, ZSTD_cParameter::ZSTD_c_compressionLevel, level);
        ZSTD_CCtx_setParameter(cctx, ZSTD_cParameter::ZSTD_c_windowLog, window_log);
        ZSTD_CCtx_setParameter(cctx, ZSTD_cParameter::ZSTD_c_enableLongDistanceMatching, 1);
        if workers > 0 { ZSTD_CCtx_setParameter(cctx, ZSTD_cParameter::ZSTD_c_nbWorkers, workers); }
        if !prefix.is_empty() {
            ZSTD_CCtx_refPrefix(cctx, prefix.as_ptr() as *const c_void, prefix.len());
        }
        let bound = ZSTD_compressBound(data.len());
        let mut out: Vec<u8> = Vec::with_capacity(bound);
        let n = ZSTD_compress2(cctx, out.as_mut_ptr() as *mut c_void, bound, data.as_ptr() as *const c_void, data.len());
        assert_eq!(ZSTD_isError(n), 0);
        ZSTD_freeCCtx(cctx);
        (n, t.elapsed().as_secs_f64() * 1000.0)
    }
}

fn payload_of(prev: &[u8], cur: &[u8]) -> Vec<u8> {
    let diff = region_diff_resizing(prev, cur);
    let mut payload = Vec::with_capacity(8 + diff.control.len() + diff.data.len());
    payload.extend_from_slice(&(diff.control.len() as u64).to_le_bytes());
    payload.extend_from_slice(&diff.control);
    payload.extend_from_slice(&diff.data);
    payload
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage");
    let (mut anchors, mut from, mut count, mut l0_levels) = (vec![8usize, 15, 30], 0usize, usize::MAX, vec![3i32, 9, 15]);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--anchors" => anchors = args.next().unwrap().split(',').map(|s| s.parse().unwrap()).collect(),
            "--from" => from = args.next().unwrap().parse().unwrap(),
            "--count" => count = args.next().unwrap().parse().unwrap(),
            "--l0-levels" => l0_levels = args.next().unwrap().split(',').map(|s| s.parse().unwrap()).collect(),
            o => panic!("unknown {o}")
        }
    }
    let bytes = std::fs::read(&path).expect("read");
    let player = ReplayFilePlayer::new(&bytes[..], false).expect("parse");
    let kfs: Vec<(u64, u8, usize, usize, usize, usize)> = player.all_uncompressed_packets().iter().filter_map(|p| match p {
        Packet::StoredKeyframe { metadata, level, state_len, uncompressed_len, frame_len, frame_offset } =>
            Some((metadata.elapsed_frames, *level, *state_len as usize, *uncompressed_len as usize, *frame_len as usize, *frame_offset as usize)),
        _ => None
    }).collect();

    let mut states: [Option<Vec<u8>>; 3] = [None, None, None];
    let mut payloads: [Vec<u8>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    let mut prev: Option<Vec<u8>> = None;
    let mut anchor_states: Vec<Option<Vec<u8>>> = vec![None; anchors.len()];
    let mut star_tot = vec![(0usize, 0f64); anchors.len()];
    let (mut chain_tot, mut chain_ram, mut chain_arch, mut samples) = (0usize, 0usize, 0usize, 0usize);
    let mut file_tot = 0usize;
    let mut l0_done = false;
    for (k, &(frame, level, state_len, ulen, flen, off)) in kfs.iter().enumerate() {
        let fb = &bytes[off..off + flen];
        let l = usize::from(level);
        let state = if level == 0 { decompress_data(fb, ulen).unwrap() } else {
            let payload = decompress_data_with_prefix(fb, ulen, &payloads[l]).unwrap();
            let mut s = states[l].clone().unwrap();
            s.resize(state_len, 0);
            let cl = u64::from_le_bytes(payload[..8].try_into().unwrap()) as usize;
            assert!(apply_region_diff_in_place(&mut s, &payload[8..8 + cl], &payload[8 + cl..]));
            for m in l..3 { payloads[m] = payload.clone(); }
            s
        };
        if level == 0 { for m in 0..3 { payloads[m].clear(); } }
        for m in l..3 { states[m] = Some(state.clone()); }

        if level == 0 && !l0_done {
            l0_done = true;
            for &lv in &l0_levels {
                let (n, ms) = zstd_len(&state, &[], lv, 27, 0);
                let (nm, msm) = zstd_len(&state, &[], lv, 27, 8);
                println!("L0 full state {} bytes: zstd{lv} {n} ({ms:.0} ms single thread, {nm} / {msm:.0} ms with 8 workers)", state.len());
            }
            let ram = &state[32..RAM_END.min(state.len())];
            let zero_pages = ram.chunks_exact(4096).filter(|p| p.iter().all(|&b| b == 0)).count();
            println!("L0 RAM {} pages, {zero_pages} all zero; archive {} bytes", ram.len() / 4096, state.len() - RAM_END);
        }

        let in_range = k >= from && k < from.saturating_add(count);
        if let (Some(p), true) = (prev.as_ref(), in_range) {
            // Chain delta, split: RAM part vs archive part, each against its own reference.
            let ram_payload = payload_of(&p[..RAM_END], &state[..RAM_END]);
            let arch_payload = payload_of(&p[RAM_END..], &state[RAM_END..]);
            let (ram_c, _) = zstd_len(&ram_payload, &p[..RAM_END], 3, 28, 0);
            let (arch_c, _) = zstd_len(&arch_payload, &p[RAM_END..], 3, 28, 0);
            let (arch_alone, _) = zstd_len(&state[RAM_END..], &p[RAM_END..], 3, 28, 0);
            let chain = payload_of(p, &state);
            let (chain_c, _) = zstd_len(&chain, p, 3, 28, 0);
            chain_tot += chain_c; chain_ram += ram_c; chain_arch += arch_c; samples += 1;
            file_tot += flen;
            let mut line = format!("k={k} f={frame} L{level} file={flen} chain={chain_c} ram={ram_c} archive={arch_c} (whole archive vs prev {arch_alone}, archive {} bytes)", state.len() - RAM_END);
            for (i, &w) in anchors.iter().enumerate() {
                if k % w == 0 || anchor_states[i].is_none() {
                    anchor_states[i] = Some(state.clone());
                    // The anchor itself would be a chain delta against the previous anchor.
                    star_tot[i].0 += chain_c;
                    line += &format!(" star{w}=anchor");
                    continue;
                }
                let a = anchor_states[i].as_ref().unwrap();
                let sp = payload_of(a, &state);
                let (n, ms) = zstd_len(&sp, a, 3, 28, 0);
                star_tot[i].0 += n;
                star_tot[i].1 += ms;
                line += &format!(" star{w}={n}({ms:.0}ms)");
            }
            println!("{line}");
        }
        prev = Some(state);
    }
    println!("samples {samples}: file {:.1} MB, chain(full prefix) {:.1} MB = RAM {:.1} + archive {:.1}", file_tot as f64 / 1e6, chain_tot as f64 / 1e6, chain_ram as f64 / 1e6, chain_arch as f64 / 1e6);
    for (i, &w) in anchors.iter().enumerate() {
        println!("star anchor every {w}: {:.1} MB (anchors counted as chain deltas; {:.0} ms/kf compress)", star_tot[i].0 as f64 / 1e6, star_tot[i].1 / samples.max(1) as f64);
    }
}
