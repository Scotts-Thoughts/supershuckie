//! Breaks a cold seek into a compressed blob down into its parts, per blob: zstd decompression,
//! packet parsing, and folding the whole delta chain. Also measures what splitting each blob's
//! compression into smaller independent zstd frames (at keyframe boundaries) would cost in size.
//!
//! ```text
//! cargo run --release -p supershuckie-replay-recorder --example blob_bench -- <file.replay> [max_blobs]
//! ```

use std::ffi::c_void;
use std::time::Instant;

use supershuckie_replay_recorder::replay_file::{ReplayHeaderBytes, ReplayHeaderRaw};
use supershuckie_replay_recorder::{apply_region_diff_in_place, Packet, PacketIO};
use zstd_sys::{ZSTD_CCtx_setParameter, ZSTD_cParameter, ZSTD_compress2, ZSTD_createCCtx, ZSTD_decompress, ZSTD_freeCCtx};
use zstd_sys::{ZSTD_createDCtx, ZSTD_decompressStream, ZSTD_freeDCtx, ZSTD_inBuffer, ZSTD_outBuffer};

/// Streaming decompression that stops once `want` output bytes exist (what a seek that only
/// needs the blob up to a keyframe's offset would pay). Returns the bytes produced.
fn decompress_prefix(data: &[u8], size: usize, want: usize) -> usize {
    let mut out = vec![0u8; size];
    unsafe {
        let dctx = ZSTD_createDCtx();
        let mut input = ZSTD_inBuffer { src: data.as_ptr() as *const c_void, size: data.len(), pos: 0 };
        let mut output = ZSTD_outBuffer { dst: out.as_mut_ptr() as *mut c_void, size: want.min(size), pos: 0 };
        while output.pos < want && input.pos < input.size {
            let r = ZSTD_decompressStream(dctx, &mut output, &mut input);
            assert_eq!(zstd_sys::ZSTD_isError(r), 0, "stream decompress failed");
            if r == 0 { break; }
        }
        ZSTD_freeDCtx(dctx);
        output.pos
    }
}

const LARGE_WINDOW_THRESHOLD: usize = 8 * 1024 * 1024;
const MAX_WINDOW_LOG: u32 = 27;

fn compress(data: &[u8], level: i32) -> Vec<u8> {
    unsafe {
        let bound = zstd_sys::ZSTD_compressBound(data.len());
        let mut v = vec![0u8; bound];
        let cctx = ZSTD_createCCtx();
        ZSTD_CCtx_setParameter(cctx, ZSTD_cParameter::ZSTD_c_compressionLevel, level);
        if data.len() > LARGE_WINDOW_THRESHOLD {
            let window_log = (usize::BITS - (data.len() - 1).leading_zeros()).min(MAX_WINDOW_LOG);
            ZSTD_CCtx_setParameter(cctx, ZSTD_cParameter::ZSTD_c_windowLog, window_log as i32);
            ZSTD_CCtx_setParameter(cctx, ZSTD_cParameter::ZSTD_c_enableLongDistanceMatching, 1);
        }
        let n = ZSTD_compress2(cctx, v.as_mut_ptr() as *mut c_void, bound, data.as_ptr() as *const c_void, data.len());
        ZSTD_freeCCtx(cctx);
        assert_eq!(zstd_sys::ZSTD_isError(n), 0, "compress failed");
        v.truncate(n);
        v
    }
}

fn decompress(data: &[u8], size: usize) -> Vec<u8> {
    let mut out = vec![0u8; size];
    let n = unsafe { ZSTD_decompress(out.as_mut_ptr() as *mut c_void, size, data.as_ptr() as *const c_void, data.len()) };
    assert_eq!(n, size, "decompress failed");
    out
}

fn mib(b: usize) -> f64 { b as f64 / (1024.0 * 1024.0) }

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: blob_bench <file.replay> [max_blobs]");
    let max_blobs: usize = args.next().map(|s| s.parse().expect("max_blobs")).unwrap_or(usize::MAX);
    let level: i32 = std::env::var("BLOB_BENCH_LEVEL").ok().and_then(|s| s.parse().ok()).unwrap_or(9);
    let splits: Vec<usize> = std::env::var("BLOB_BENCH_SPLITS")
        .ok()
        .map(|s| s.split(',').map(|x| x.parse().expect("split")).collect())
        .unwrap_or_else(|| vec![3, 5, 15]);

    let bytes = std::fs::read(&path).expect("read");
    let header_bytes: &ReplayHeaderBytes = bytes[..2048].try_into().unwrap();
    let header = ReplayHeaderRaw::from_bytes(header_bytes);
    let version = header.replay_version;
    let end = header.packet_stream_end().map(|e| e as usize).unwrap_or(bytes.len());
    let mut stream = &bytes[2048 + header.patch_data_length as usize..end];

    let mut top = Vec::new();
    while !stream.is_empty() {
        top.push(Packet::read_all(&mut stream, version).expect("top-level packet"));
    }
    let blobs: Vec<&Packet> = top.iter().filter(|p| matches!(p, Packet::CompressedBlob { .. })).collect();
    println!("{path}: v{version}, {} top-level packets, {} blobs, level {level}, splits {splits:?}", top.len(), blobs.len());
    println!();
    println!("blob | frames | keyframes full/delta | compressed MiB | uncompressed MiB | packets | zstd ms | parse ms | fold-all ms | ms/fold | full-kf MiB (alone)");

    let mut totals = (0usize, 0usize, 0f64, 0f64, 0f64, 0usize, 0usize);
    let mut split_totals: Vec<usize> = vec![0; splits.len()];
    let mut per_kf_total = 0usize;

    for (blob_no, packet) in blobs.iter().enumerate().take(max_blobs) {
        let Packet::CompressedBlob { compressed_data, uncompressed_size, elapsed_frames_start, elapsed_frames_end, keyframes, .. } = packet else { unreachable!() };
        let usize_size = *uncompressed_size as usize;

        let t = Instant::now();
        let raw = decompress(compressed_data.as_slice(), usize_size);
        let zstd_ms = t.elapsed().as_secs_f64() * 1000.0;

        let t = Instant::now();
        let mut b = raw.as_slice();
        let mut packets = Vec::new();
        let mut keyframe_offsets = Vec::new(); // byte offset of each keyframe-class packet
        while !b.is_empty() {
            let offset = raw.len() - b.len();
            let p = Packet::read_all(&mut b, version).expect("blob packet");
            if matches!(p, Packet::Keyframe { .. } | Packet::RegionDeltaKeyframe { .. } | Packet::DeltaKeyframe { .. }) {
                keyframe_offsets.push(offset);
            }
            packets.push(p);
        }
        let parse_ms = t.elapsed().as_secs_f64() * 1000.0;

        let full = packets.iter().filter(|p| matches!(p, Packet::Keyframe { .. })).count();
        let delta = keyframe_offsets.len() - full;

        // Fold the whole chain.
        let t = Instant::now();
        let mut state: Vec<u8> = Vec::new();
        let mut folds = 0usize;
        for p in &packets {
            match p {
                Packet::Keyframe { state: s, .. } => { state.clear(); state.extend_from_slice(s.as_slice()); },
                Packet::RegionDeltaKeyframe { control, data, .. } => {
                    assert!(apply_region_diff_in_place(state.as_mut_slice(), control.as_slice(), data.as_slice()));
                    folds += 1;
                },
                _ => {}
            }
        }
        let fold_ms = t.elapsed().as_secs_f64() * 1000.0;

        // Full keyframe compressed alone (what an extra blob start costs).
        let first_end = keyframe_offsets.get(1).copied().unwrap_or(raw.len());
        let full_alone = compress(&raw[..first_end], level).len();

        println!(
            "{blob_no:>4} | {:>6} | {full:>4}/{delta:<5} | {:>8.2} | {:>8.2} | {:>7} | {zstd_ms:>7.1} | {parse_ms:>7.1} | {fold_ms:>7.1} | {:>5.2} | {:.2}",
            elapsed_frames_end - elapsed_frames_start,
            mib(compressed_data.as_slice().len()),
            mib(usize_size),
            packets.len(),
            if folds > 0 { fold_ms / folds as f64 } else { 0.0 },
            mib(full_alone)
        );

        totals.0 += compressed_data.as_slice().len();
        totals.1 += usize_size;
        totals.2 += zstd_ms;
        totals.3 += parse_ms;
        totals.4 += fold_ms;
        totals.5 += packets.len();
        totals.6 += keyframes.len();

        // Partial (streaming) decompression up to a fraction of the blob, and partial parsing.
        if std::env::var("BLOB_BENCH_PREFIX").is_ok() {
            for &(num, den) in &[(1usize, 4usize), (1, 2), (3, 4), (1, 1)] {
                let want = raw.len() * num / den;
                let t = Instant::now();
                let got = decompress_prefix(compressed_data.as_slice(), raw.len(), want);
                let d_ms = t.elapsed().as_secs_f64() * 1000.0;
                let t = Instant::now();
                let mut b = &raw[..got.min(raw.len())];
                let mut n = 0usize;
                while !b.is_empty() {
                    if Packet::read_all(&mut b, version).is_err() { break; }
                    n += 1;
                }
                let p_ms = t.elapsed().as_secs_f64() * 1000.0;
                println!("       prefix {num}/{den}: zstd stream {d_ms:>6.1} ms ({:.1} MiB), parse {p_ms:>6.1} ms ({n} packets)", mib(got));
            }
            println!("       size_of::<Packet>() = {} bytes; parsed blob footprint ~{:.1} MiB", std::mem::size_of::<Packet>(), mib(packets.len() * std::mem::size_of::<Packet>() + usize_size));
        }
        // zstd level sweep: size, compression time and decompression time of this blob.
        if let Ok(levels) = std::env::var("BLOB_BENCH_LEVELS") {
            for lv in levels.split(',').map(|x| x.parse::<i32>().expect("level")) {
                let t = Instant::now();
                let c = compress(&raw, lv);
                let c_ms = t.elapsed().as_secs_f64() * 1000.0;
                let t = Instant::now();
                let d = decompress(&c, raw.len());
                let d_ms = t.elapsed().as_secs_f64() * 1000.0;
                assert_eq!(d.len(), raw.len());
                println!("       level {lv:>2}: {:>8.2} MiB ({:+.1}% vs file), compress {c_ms:>7.0} ms, decompress {d_ms:>6.1} ms", mib(c.len()), (c.len() as f64 / compressed_data.as_slice().len() as f64 - 1.0) * 100.0);
            }
        }
        // Splitting compression into independent frames at keyframe boundaries.
        if std::env::var("BLOB_BENCH_NO_SPLITS").is_ok() { continue; }
        for (si, &k) in splits.iter().enumerate() {
            let per = (keyframe_offsets.len() + k - 1) / k;
            let mut total = 0usize;
            let mut i = 0;
            while i < keyframe_offsets.len() {
                let start = keyframe_offsets[i];
                let stop = keyframe_offsets.get(i + per).copied().unwrap_or(raw.len());
                total += compress(&raw[start..stop], level).len();
                i += per;
            }
            split_totals[si] += total;
        }
        // Every keyframe its own frame.
        {
            let mut total = 0usize;
            for (i, &start) in keyframe_offsets.iter().enumerate() {
                let stop = keyframe_offsets.get(i + 1).copied().unwrap_or(raw.len());
                total += compress(&raw[start..stop], level).len();
            }
            per_kf_total += total;
        }
    }

    println!();
    println!(
        "totals: compressed {:.2} MiB, uncompressed {:.2} MiB, {} packets, {} keyframes; zstd {:.0} ms, parse {:.0} ms, fold {:.0} ms",
        mib(totals.0), mib(totals.1), totals.5, totals.6, totals.2, totals.3, totals.4
    );
    println!("re-compressed as one frame per blob at level {level}: (baseline = file's blobs, {:.2} MiB)", mib(totals.0));
    for (si, &k) in splits.iter().enumerate() {
        println!("  split into {k:>3} frames per blob: {:.2} MiB ({:+.1}%)", mib(split_totals[si]), (split_totals[si] as f64 / totals.0 as f64 - 1.0) * 100.0);
    }
    println!("  one frame per keyframe:        {:.2} MiB ({:+.1}%)", mib(per_kf_total), (per_kf_total as f64 / totals.0 as f64 - 1.0) * 100.0);
}
