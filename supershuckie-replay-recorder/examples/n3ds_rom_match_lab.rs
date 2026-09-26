//! How much of what changes between Nintendo 3DS keyframes is a verbatim copy of bytes in the
//! game's ROM (which a player always has, so a delta could point into it instead of storing the
//! bytes)? Hashes every 64-byte-aligned block of new data in the keyframe deltas, scans the ROM
//! with a rolling hash at every byte offset, then recompresses the deltas without the blocks
//! found. Research tool for `replay-3ds-format-research.md`.
//!
//! ```text
//! n3ds_rom_match_lab <file.replay> <rom> [--from N] [--count N]
//! ```

use std::ffi::c_void;
use std::io::{Read, Seek, SeekFrom};
use std::time::Instant;

use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::{apply_region_diff_in_place, decompress_data, decompress_data_with_prefix, region_diff_resizing, Packet};
use zstd_sys::*;

const BLOCK: usize = 64;
const BASE: u64 = 0x100000001b3;

fn block_hash(b: &[u8]) -> u64 {
    let mut h = 0u64;
    for &x in b { h = h.wrapping_mul(BASE).wrapping_add(u64::from(x) + 1); }
    h
}

fn zstd_len(data: &[u8], prefix: &[u8]) -> usize {
    unsafe {
        let cctx = ZSTD_createCCtx();
        ZSTD_CCtx_setParameter(cctx, ZSTD_cParameter::ZSTD_c_compressionLevel, 3);
        ZSTD_CCtx_setParameter(cctx, ZSTD_cParameter::ZSTD_c_windowLog, 28);
        ZSTD_CCtx_setParameter(cctx, ZSTD_cParameter::ZSTD_c_enableLongDistanceMatching, 1);
        if !prefix.is_empty() { ZSTD_CCtx_refPrefix(cctx, prefix.as_ptr() as *const c_void, prefix.len()); }
        let bound = ZSTD_compressBound(data.len());
        let mut out: Vec<u8> = Vec::with_capacity(bound);
        let n = ZSTD_compress2(cctx, out.as_mut_ptr() as *mut c_void, bound, data.as_ptr() as *const c_void, data.len());
        assert_eq!(ZSTD_isError(n), 0);
        ZSTD_freeCCtx(cctx);
        n
    }
}

fn read_leb(input: &mut &[u8]) -> u64 {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let b = input[0];
        *input = &input[1..];
        v |= u64::from(b & 0x7F) << shift;
        if b & 0x80 == 0 { return v; }
        shift += 7;
    }
}

/// Decode the file's keyframes in order, calling `f(k, prev, cur)` for consecutive pairs in range.
fn walk(bytes: &[u8], kfs: &[(u64, u8, usize, usize, usize, usize)], from: usize, count: usize, mut f: impl FnMut(usize, &[u8], &[u8])) {
    let mut states: [Option<Vec<u8>>; 3] = [None, None, None];
    let mut payloads: [Vec<u8>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    let mut prev: Option<Vec<u8>> = None;
    for (k, &(_, level, state_len, ulen, flen, off)) in kfs.iter().enumerate() {
        if k >= from.saturating_add(count) { break; }
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
        if k == 0 && std::env::var_os("N3DS_L0").is_some() {
            // Full-keyframe mode: the first state against an all-zero state of the same length.
            let zeros = vec![0u8; state.len()];
            f(0, &zeros, &state);
            return;
        }
        if let Some(p) = prev.as_ref() {
            if k >= from { f(k, p, &state); }
        }
        prev = Some(state);
    }
}

/// Aligned 64-byte blocks fully inside changed runs: (state offset, hash).
fn changed_blocks(prev: &[u8], cur: &[u8]) -> (Vec<u8>, Vec<(usize, usize)>, Vec<(usize, u64)>) {
    let diff = region_diff_resizing(prev, cur);
    let mut ctl = &diff.control[..];
    let mut pos = 0usize;
    let mut runs = Vec::new();
    let mut blocks = Vec::new();
    while !ctl.is_empty() {
        let gap = read_leb(&mut ctl) as usize;
        let len = read_leb(&mut ctl) as usize;
        pos += gap;
        let (s, e) = (pos * 4, ((pos + len) * 4).min(cur.len()));
        runs.push((s, e));
        let mut b = s.div_ceil(BLOCK) * BLOCK;
        while b + BLOCK <= e {
            let blk = &cur[b..b + BLOCK];
            // All-zero and constant blocks compress to nothing anyway; do not count them.
            if blk.iter().any(|&x| x != blk[0]) { blocks.push((b, block_hash(blk))); }
            b += BLOCK;
        }
        pos += len;
    }
    let mut payload = Vec::new();
    payload.extend_from_slice(&(diff.control.len() as u64).to_le_bytes());
    payload.extend_from_slice(&diff.control);
    payload.extend_from_slice(&diff.data);
    (payload, runs, blocks)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage");
    let rom_path = args.next().expect("rom");
    let (mut from, mut count) = (0usize, 150usize);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--from" => from = args.next().unwrap().parse().unwrap(),
            "--count" => count = args.next().unwrap().parse().unwrap(),
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
    drop(player);

    // Pass 1: hashes of the changed blocks.
    let t = Instant::now();
    let mut hashes: Vec<u64> = Vec::new();
    let mut changed_bytes = 0usize;
    walk(&bytes, &kfs, from, count, |_, p, c| {
        let (payload, _, blocks) = changed_blocks(p, c);
        changed_bytes += payload.len();
        hashes.extend(blocks.iter().map(|b| b.1));
    });
    let total_blocks = hashes.len();
    hashes.sort_unstable();
    hashes.dedup();
    println!("pass 1: {} changed payload bytes, {total_blocks} non-constant 64-byte blocks ({} distinct) in {:.0} s", changed_bytes, hashes.len(), t.elapsed().as_secs_f64());

    // Pass 2: scan the ROM (rolling hash at every byte offset) on 12 threads.
    let t = Instant::now();
    const FILTER_BITS: usize = 1 << 30;
    let mut filter = vec![0u64; FILTER_BITS / 64];
    for &h in &hashes { let i = (h >> 34) as usize; filter[i / 64] |= 1 << (i % 64); }
    let rom_len = std::fs::metadata(&rom_path).unwrap().len() as usize;
    let threads = 12;
    let per = rom_len.div_ceil(threads);
    let mut pow = 1u64;
    for _ in 0..BLOCK - 1 { pow = pow.wrapping_mul(BASE); }
    let found: Vec<Vec<u64>> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..threads).map(|i| {
            let (filter, hashes, rom_path) = (&filter, &hashes, &rom_path);
            sc.spawn(move || {
                let start = i * per;
                let end = ((i + 1) * per + BLOCK - 1).min(rom_len);
                let mut f = std::fs::File::open(rom_path).unwrap();
                f.seek(SeekFrom::Start(start as u64)).unwrap();
                let mut out = Vec::new();
                // Stream 16 MB chunks, carrying the last BLOCK-1 bytes over.
                let mut buf: Vec<u8> = Vec::with_capacity((16 << 20) + BLOCK);
                let mut pos = start;
                while pos < end {
                    let n = (16usize << 20).min(end - pos);
                    let keep = buf.len().min(BLOCK - 1);
                    let tail: Vec<u8> = buf[buf.len() - keep..].to_vec();
                    buf.clear();
                    buf.extend_from_slice(&tail);
                    let old = buf.len();
                    buf.resize(old + n, 0);
                    f.read_exact(&mut buf[old..]).unwrap();
                    pos += n;
                    if buf.len() < BLOCK { continue; }
                    let mut h = block_hash(&buf[..BLOCK]);
                    let mut j = 0usize;
                    loop {
                        let fi = (h >> 34) as usize;
                        if filter[fi / 64] & (1 << (fi % 64)) != 0 && hashes.binary_search(&h).is_ok() { out.push(h); }
                        if j + BLOCK >= buf.len() { break; }
                        h = h.wrapping_sub((u64::from(buf[j]) + 1).wrapping_mul(pow)).wrapping_mul(BASE).wrapping_add(u64::from(buf[j + BLOCK]) + 1);
                        j += 1;
                    }
                }
                out
            })
        }).collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut in_rom: Vec<u64> = found.into_iter().flatten().collect();
    in_rom.sort_unstable();
    in_rom.dedup();
    println!("pass 2: ROM {} bytes scanned in {:.0} s: {} of {} distinct blocks found in the ROM", rom_len, t.elapsed().as_secs_f64(), in_rom.len(), hashes.len());

    // Pass 3: recompress each delta with and without the ROM-found blocks (a ROM reference would
    // cost ~6 bytes per run of found blocks; counted).
    let t = Instant::now();
    let (mut full, mut without, mut found_bytes, mut block_bytes, mut refs) = (0usize, 0usize, 0usize, 0usize, 0usize);
    let (mut hist_full, mut hist_without) = (0usize, 0usize);
    let mut history: std::collections::VecDeque<Vec<u8>> = Default::default();
    walk(&bytes, &kfs, from, count, |k, p, c| {
        let (payload, runs, blocks) = changed_blocks(p, c);
        let a = zstd_len(&payload, p);
        // History prefix: bytes the previous deltas overwrote (64 MB), then the reference state.
        let mut hp: Vec<u8> = Vec::new();
        for u in history.iter() { hp.extend_from_slice(u); }
        hp.extend_from_slice(p);
        let mut undo = Vec::new();
        for &(s, e) in &runs { undo.extend_from_slice(&p[s.min(p.len())..e.min(p.len())]); }
        // Rebuild the data with found blocks cut out.
        let found: std::collections::HashSet<usize> = blocks.iter().filter(|b| in_rom.binary_search(&b.1).is_ok()).map(|b| b.0).collect();
        let mut rest = Vec::with_capacity(payload.len());
        let mut last_found = false;
        for &(s, e) in &runs {
            let mut i = s;
            while i < e {
                if i % BLOCK == 0 && found.contains(&i) {
                    if !last_found { refs += 1; }
                    last_found = true;
                    i += BLOCK;
                } else {
                    last_found = false;
                    rest.push(c[i]);
                    i += 1;
                }
            }
        }
        let b = zstd_len(&rest, p);
        if std::env::var_os("N3DS_L0").is_some() {
            let mut zeroed = c.to_vec();
            for &o in &found { zeroed[o..o + BLOCK].fill(0); }
            println!("full state: zstd3 {} bytes; with ROM-found blocks cut out {} bytes ({} blocks, {} bytes found)", zstd_len(c, &[]), zstd_len(&zeroed, &[]), found.len(), found.len() * BLOCK);
        }
        hist_full += zstd_len(&payload, &hp);
        hist_without += zstd_len(&rest, &hp);
        history.push_back(undo);
        while history.iter().map(|u| u.len()).sum::<usize>() > 64 << 20 { history.pop_front(); }
        full += a;
        without += b;
        found_bytes += found.len() * BLOCK;
        block_bytes += blocks.len() * BLOCK;
        if k % 20 == 0 { println!("k={k}: delta {a} bytes, without ROM blocks {b} ({} of {} block bytes found)", found.len() * BLOCK, blocks.len() * BLOCK); }
    });
    println!("with 64 MB history prefix: {:.1} MB -> {:.1} MB without ROM-found blocks", hist_full as f64 / 1e6, hist_without as f64 / 1e6);
    println!("pass 3 ({:.0} s): deltas {:.1} MB -> {:.1} MB without ROM-found blocks (+ {refs} references, ~{:.1} MB); {:.1}% of non-constant changed block bytes are in the ROM",
             t.elapsed().as_secs_f64(), full as f64 / 1e6, without as f64 / 1e6, refs as f64 * 6.0 / 1e6, found_bytes as f64 * 100.0 / block_bytes.max(1) as f64);
}
