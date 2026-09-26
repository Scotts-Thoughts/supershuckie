//! History-prefix experiments on a real Nintendo 3DS replay (format v8): each consecutive keyframe
//! delta compressed with the reference state plus the bytes earlier deltas overwrote, with the
//! history capped or reset every W keyframes (what a decoder walking a segment can rebuild). Research tool for `replay-3ds-format-research.md`.
//!
//! ```text
//! n3ds_keyframe_lab <file.replay> [--from N] [--count N] [--stride N] [--heavy] [--csv out.csv]
//! ```
//! `--heavy` adds the slow experiments (zstd 19, whole previous state as prefix).

use std::collections::HashSet;
use std::ffi::c_void;
use std::io::Write;
use std::time::Instant;

use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::{apply_region_diff_in_place, decompress_data, decompress_data_with_prefix, region_diff_resizing, Packet};
use zstd_sys::*;

/// Raw layout of an Old 3DS Azahar state (patch 0002/0004): header, FCRAM, VRAM, DSP RAM, archive.
const HEADER: usize = 32;
const FCRAM: usize = 128 << 20;
const VRAM: usize = 6 << 20;
const DSP: usize = 512 << 10;

fn region_of(offset: usize) -> usize {
    if offset < HEADER { 0 } else if offset < HEADER + FCRAM { 1 } else if offset < HEADER + FCRAM + VRAM { 2 } else if offset < HEADER + FCRAM + VRAM + DSP { 3 } else { 4 }
}
const REGION_NAMES: [&str; 5] = ["header", "fcram", "vram", "dsp", "archive"];

#[derive(Clone, Copy)]
struct Z { level: i32, window_log: i32, ldm: bool, threads: i32 }

fn zstd_frame(data: &[u8], prefix: &[u8], z: &Z) -> Vec<u8> {
    unsafe {
        let cctx = ZSTD_createCCtx();
        ZSTD_CCtx_setParameter(cctx, ZSTD_cParameter::ZSTD_c_compressionLevel, z.level);
        ZSTD_CCtx_setParameter(cctx, ZSTD_cParameter::ZSTD_c_windowLog, z.window_log);
        ZSTD_CCtx_setParameter(cctx, ZSTD_cParameter::ZSTD_c_enableLongDistanceMatching, z.ldm as i32);
        if z.threads > 0 {
            ZSTD_CCtx_setParameter(cctx, ZSTD_cParameter::ZSTD_c_nbWorkers, z.threads);
        }
        if !prefix.is_empty() {
            ZSTD_CCtx_refPrefix(cctx, prefix.as_ptr() as *const c_void, prefix.len());
        }
        let bound = ZSTD_compressBound(data.len());
        let mut out: Vec<u8> = Vec::with_capacity(bound);
        let n = ZSTD_compress2(cctx, out.as_mut_ptr() as *mut c_void, bound, data.as_ptr() as *const c_void, data.len());
        assert_eq!(ZSTD_isError(n), 0, "zstd error");
        out.set_len(n);
        ZSTD_freeCCtx(cctx);
        out
    }
}

/// Compressed size, compression ms, and decompression ms (checked round trip).
fn timed(data: &[u8], prefix: &[u8], z: &Z) -> (usize, f64, f64) {
    let t = Instant::now();
    let frame = zstd_frame(data, prefix, z);
    let c = t.elapsed().as_secs_f64() * 1000.0;
    let t = Instant::now();
    let out = unzstd(&frame, data.len(), prefix);
    let d = t.elapsed().as_secs_f64() * 1000.0;
    assert!(out == data, "round trip");
    (frame.len(), c, d)
}

fn unzstd(frame: &[u8], len: usize, prefix: &[u8]) -> Vec<u8> {
    unsafe {
        let dctx = ZSTD_createDCtx();
        ZSTD_DCtx_setParameter(dctx, ZSTD_dParameter::ZSTD_d_windowLogMax, 31);
        if !prefix.is_empty() {
            ZSTD_DCtx_refPrefix(dctx, prefix.as_ptr() as *const c_void, prefix.len());
        }
        let mut out: Vec<u8> = Vec::with_capacity(len);
        let n = ZSTD_decompressDCtx(dctx, out.as_mut_ptr() as *mut c_void, len, frame.as_ptr() as *const c_void, frame.len());
        assert_eq!(ZSTD_isError(n), 0, "unzstd");
        out.set_len(n);
        ZSTD_freeDCtx(dctx);
        out
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

/// Runs (byte offset, byte len) of a region diff's control stream.
fn runs(control: &[u8]) -> Vec<(usize, usize)> {
    let mut ctl = control;
    let mut pos = 0usize;
    let mut out = Vec::new();
    while !ctl.is_empty() {
        let gap = read_leb(&mut ctl) as usize;
        let len = read_leb(&mut ctl) as usize;
        pos += gap;
        out.push((pos * 4, len * 4));
        pos += len;
    }
    out
}

fn page_hash(p: &[u8]) -> u64 {
    // FNV-1a over 8-byte words; collisions are irrelevant at this scale for a statistic.
    let mut h = 0xcbf29ce484222325u64;
    for c in p.chunks_exact(8) {
        h ^= u64::from_le_bytes(c.try_into().unwrap());
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage");
    let (mut from, mut count, mut stride, mut heavy, mut csv) = (0usize, usize::MAX, 1usize, false, None::<String>);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--from" => from = args.next().unwrap().parse().unwrap(),
            "--count" => count = args.next().unwrap().parse().unwrap(),
            "--stride" => stride = args.next().unwrap().parse().unwrap(),
            "--heavy" => heavy = true,
            "--csv" => csv = args.next(),
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
    println!("{} keyframes", kfs.len());
    let mut csv = csv.map(|p| std::io::BufWriter::new(std::fs::File::create(p).unwrap()));
    if let Some(c) = csv.as_mut() {
        write!(c, "k,frame,level,file_frame,state_len,runs,changed,pages,ch_header,ch_fcram,ch_vram,ch_dsp,ch_archive,pg_fcram,pg_vram,pg_dsp,pg_archive,pages_in_prev,pages_in_store,pagebytes_new,diff_ms,decode_ms").unwrap();
        for n in ["base3", "full3", "hist64", "seg15", "seg30", "seg60"] { write!(c, ",{n},{n}_cms,{n}_dms").unwrap(); }
        writeln!(c).unwrap();
    }

    // Decoder state: per level, the state after the most recent keyframe of level <= it and that
    // keyframe's payload.
    let mut states: [Option<Vec<u8>>; 3] = [None, None, None];
    let mut payloads: [Vec<u8>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    let mut prev_state: Option<Vec<u8>> = None;
    let mut prev_payload: Vec<u8> = Vec::new();
    // (name, reset every W keyframes (0 = never), cap in bytes, history)
    let mut hists: Vec<(&'static str, usize, usize, std::collections::VecDeque<Vec<u8>>)> = vec![
        ("hist64", 0, 64 << 20, Default::default()),
        ("seg15", 15, 128 << 20, Default::default()),
        ("seg30", 30, 128 << 20, Default::default()),
        ("seg60", 60, 128 << 20, Default::default()),
    ];
    let mut names: [&str; 16] = [""; 16];
    names[0] = "file";
    let (mut ctime, mut dtime, mut samples) = ([0f64; 16], [0f64; 16], 0usize);
    let mut store: HashSet<u64> = HashSet::new();
    let mut totals = [0f64; 16];

    let z3 = Z { level: 3, window_log: 27, ldm: true, threads: 0 };
    for (k, &(frame, level, state_len, uncompressed_len, frame_len, frame_offset)) in kfs.iter().enumerate() {
        let t = Instant::now();
        let frame_bytes = &bytes[frame_offset..frame_offset + frame_len];
        let state = if level == 0 {
            decompress_data(frame_bytes, uncompressed_len).expect("L0")
        } else {
            let l = usize::from(level);
            let payload = decompress_data_with_prefix(frame_bytes, uncompressed_len, &payloads[l]).expect("delta");
            let mut s = states[l].clone().expect("reference");
            s.resize(state_len, 0);
            let cl = u64::from_le_bytes(payload[..8].try_into().unwrap()) as usize;
            assert!(apply_region_diff_in_place(&mut s, &payload[8..8 + cl], &payload[8 + cl..]));
            for m in l..3 { payloads[m] = payload.clone(); }
            s
        };
        if level == 0 { for m in 0..3 { payloads[m].clear(); } }
        for m in usize::from(level)..3 { states[m] = Some(state.clone()); }
        let decode_ms = t.elapsed().as_secs_f64() * 1000.0;

        let in_range = k >= from && k < from.saturating_add(count) && (k - from) % stride == 0;
        if let (Some(prev), true) = (prev_state.as_ref(), in_range) {
            let t = Instant::now();
            let diff = region_diff_resizing(prev, &state);
            let diff_ms = t.elapsed().as_secs_f64() * 1000.0;
            let mut payload = Vec::with_capacity(8 + diff.control.len() + diff.data.len());
            payload.extend_from_slice(&(diff.control.len() as u64).to_le_bytes());
            payload.extend_from_slice(&diff.control);
            payload.extend_from_slice(&diff.data);

            // Per-region changed bytes and 4 KB pages, XOR and undo data.
            let rs = runs(&diff.control);
            let mut ch = [0usize; 5];
            let mut pages: HashSet<usize> = HashSet::new();
            let mut undo = Vec::with_capacity(diff.data.len());
            let mut split: [Vec<u8>; 5] = Default::default();
            for &(off, len) in &rs {
                // Counted per region at run granularity (runs rarely straddle regions).
                ch[region_of(off)] += len;
                let mut p = off / 4096;
                while p * 4096 < off + len { pages.insert(p); p += 1; }
                for i in off..(off + len).min(state.len()) {
                    let o = prev.get(i).copied().unwrap_or(0);
                    undo.push(o);
                    split[region_of(off)].push(state[i]);
                }
            }
            let mut pg = [0usize; 5];
            for &p in &pages { pg[region_of(p * 4096)] += 1; }

            // Page dedupe: changed pages whose new content is a page of the previous state, or a
            // page ever seen in any earlier keyframe state.
            let prev_pages: HashSet<u64> = prev.chunks_exact(4096).map(page_hash).collect();
            let (mut in_prev, mut in_store, mut new_bytes) = (0usize, 0usize, Vec::new());
            let mut sorted: Vec<usize> = pages.iter().copied().collect();
            sorted.sort();
            for &p in &sorted {
                if (p + 1) * 4096 > state.len() { continue; }
                let page = &state[p * 4096..(p + 1) * 4096];
                let h = page_hash(page);
                if prev_pages.contains(&h) { in_prev += 1; }
                if store.contains(&h) { in_store += 1; } else { new_bytes.extend_from_slice(page); }
            }

            // Whole changed pages (index list + contents): what a recorder could write straight
            // from the write-watch bitmap without comparing anything.
            let mut page_payload = Vec::with_capacity(sorted.len() * 4100);
            for &p in &sorted { page_payload.extend_from_slice(&(p as u32).to_le_bytes()); }
            for &p in &sorted { let e = ((p + 1) * 4096).min(state.len()); page_payload.extend_from_slice(&state[p * 4096..e]); }
            for h in hists.iter_mut() { if h.1 > 0 && k % h.1 == 0 { h.3.clear(); } }
            let prefixes: Vec<Vec<u8>> = hists.iter().map(|h| {
                let mut v: Vec<u8> = Vec::new();
                for u in h.3.iter() { v.extend_from_slice(u); }
                v.extend_from_slice(prev);
                v
            }).collect();
            let mut jobs: Vec<(&'static str, Box<dyn Fn() -> (usize, f64, f64) + Sync + '_>)> = vec![
                ("base3", Box::new(|| timed(&payload, &prev_payload, &Z { ..z3 }))),
                ("full3", Box::new(|| timed(&payload, prev, &Z { window_log: 28, ..z3 }))),
            ];
            let payload_ref = &payload;
            for (i, h) in hists.iter().enumerate() {
                let pre = &prefixes[i];
                let wl = if pre.len() > (512 << 20) { 30 } else { 29 };
                jobs.push((h.0, Box::new(move || timed(payload_ref, pre, &Z { window_log: wl, ..z3 }))));
            }
            let results: Vec<(&'static str, (usize, f64, f64))> = std::thread::scope(|sc| {
                let hs: Vec<_> = jobs.iter().map(|(n, f)| (*n, sc.spawn(move || f()))).collect();
                hs.into_iter().map(|(n, h)| (n, h.join().unwrap())).collect()
            });
            drop(jobs);
            let changed: usize = ch.iter().sum();
            let summary: Vec<String> = results.iter().map(|(n, (sz, cms, dms))| format!("{n}={sz}({cms:.0}/{dms:.0}ms)")).collect();
            println!("k={k} f={frame} L{level} file={frame_len} runs={} changed={} pages={} [{}] dedupe(prev {in_prev}, store {in_store}, new {}KB) diff={diff_ms:.0}ms dec={decode_ms:.0}ms {}",
                rs.len(), changed, pages.len(), (0..5).map(|r| format!("{}:{}KB/{}p", REGION_NAMES[r], ch[r] / 1024, pg[r])).collect::<Vec<_>>().join(" "), new_bytes.len() / 1024, summary.join(" "));
            if let Some(c) = csv.as_mut() {
                write!(c, "{k},{frame},{level},{frame_len},{state_len},{},{changed},{},{},{},{},{},{},{},{},{},{},{in_prev},{in_store},{},{diff_ms:.1},{decode_ms:.1}",
                    rs.len(), pages.len(), ch[0], ch[1], ch[2], ch[3], ch[4], pg[1], pg[2], pg[3], pg[4], new_bytes.len()).unwrap();
                for (_, (sz, cms, dms)) in &results { write!(c, ",{sz},{cms:.1},{dms:.1}").unwrap(); }
                writeln!(c).unwrap();
            }
            totals[0] += frame_len as f64;
            for (i, (n, (sz, cms, dms))) in results.iter().enumerate() {
                names[i + 1] = n;
                totals[i + 1] += *sz as f64;
                ctime[i + 1] += cms;
                dtime[i + 1] += dms;
            }
            samples += 1;
            for h in hists.iter_mut() {
                h.3.push_back(undo.clone());
                while h.3.iter().map(|u| u.len()).sum::<usize>() > h.2 { h.3.pop_front(); }
            }
            prev_payload = payload;
        }
        // Every page of every keyframe state goes into the global store.
        prev_state = Some(state);
    }
    for i in 0..16 {
        if names[i].is_empty() { continue; }
        println!("total {:10} {:9.1} MB  compress {:7.1} ms/kf  decompress {:6.1} ms/kf", names[i], totals[i] / 1e6, ctime[i] / samples.max(1) as f64, dtime[i] / samples.max(1) as f64);
    }
}
