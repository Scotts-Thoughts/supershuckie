//! Where the bytes of a Nintendo 3DS replay (format v8) go: serialised size of every top-level
//! packet kind, keyframe frames by level, and what the thumbnails and inputs would cost under
//! other encodings. Research tool for `replay-3ds-format-research.md`.
//!
//! ```text
//! cargo run --release -p supershuckie-replay-recorder --example n3ds_breakdown -- <file.replay> [--dump-thumbs <dir>]
//! ```

use std::collections::BTreeMap;

use supershuckie_replay_recorder::append_packet;
use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::{compress_data, compress_data_with_prefix, decompress_data};
use supershuckie_replay_recorder::Packet;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: n3ds_breakdown <file.replay> [--dump-thumbs dir]");
    let mut dump_thumbs: Option<std::path::PathBuf> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--dump-thumbs" => dump_thumbs = Some(args.next().unwrap().into()),
            other => panic!("unknown option {other}")
        }
    }
    let bytes = std::fs::read(&path).expect("read");
    let player = ReplayFilePlayer::new(&bytes, false).expect("parse");
    println!("{path}: {} bytes, v{}, {} frames", bytes.len(), player.get_replay_version(), player.get_total_frames());

    let mut sizes: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let mut add = |k: &str, n: u64| { let e = sizes.entry(k.to_owned()).or_default(); e.0 += 1; e.1 += n; };
    let mut buf = Vec::new();
    let mut inputs: Vec<Vec<u8>> = Vec::new();
    let mut input_stream: Vec<u8> = Vec::new();
    let mut thumbs_raw_top: Vec<Vec<u8>> = Vec::new();
    let mut thumbs_raw_bottom: Vec<Vec<u8>> = Vec::new();
    let mut thumb_dims = (0u64, 0u64, 0u64, 0u64);
    for p in player.all_uncompressed_packets() {
        buf.clear();
        append_packet(p, &mut buf);
        let n = buf.len() as u64;
        match p {
            Packet::StoredKeyframe { level, frame_len, .. } => {
                add(&format!("StoredKeyframe L{level} header"), n);
                add(&format!("StoredKeyframe L{level} frame"), *frame_len);
            }
            Packet::Thumbnail { top, bottom, top_width, top_height, bottom_width, bottom_height, .. } => {
                add("Thumbnail", n);
                add("Thumbnail top zstd", top.len() as u64);
                add("Thumbnail bottom zstd", bottom.len() as u64);
                thumb_dims = (*top_width, *top_height, *bottom_width, *bottom_height);
                thumbs_raw_top.push(decompress_data(top, (*top_width * *top_height * 2) as usize).expect("top"));
                thumbs_raw_bottom.push(decompress_data(bottom, (*bottom_width * *bottom_height * 2) as usize).expect("bottom"));
            }
            Packet::ChangeInput { data } => {
                add("ChangeInput", n);
                inputs.push(data.to_vec());
                input_stream.extend_from_slice(&buf);
            }
            Packet::NextFrame { .. } => {
                add("NextFrame", n);
                input_stream.extend_from_slice(&buf);
            }
            other => add(&format!("{:?}", std::mem::discriminant(other)), n)
        }
    }
    let mut total = 0u64;
    for (k, (c, n)) in &sizes {
        println!("  {k:32} count {c:8} bytes {:10.2} MB", *n as f64 / 1e6);
        if !k.contains(" zstd") && !k.contains("frame") || k.ends_with(" frame") { total += n; }
    }
    println!("  sum of packets+frames {:.2} MB (file {:.2} MB)", total as f64 / 1e6, bytes.len() as f64 / 1e6);

    // Inputs: how compressible is the NextFrame/ChangeInput stream?
    let distinct: std::collections::HashSet<&Vec<u8>> = inputs.iter().collect();
    let repeats = inputs.windows(2).filter(|w| w[0] == w[1]).count();
    println!("inputs: {} ChangeInput, {} distinct values, {} equal to the previous one, input len {}", inputs.len(), distinct.len(), repeats, inputs.first().map_or(0, |v| v.len()));
    println!("input+frame stream {:.2} MB raw, zstd19 {:.2} MB", input_stream.len() as f64 / 1e6, compress_data(&input_stream, 19).unwrap().len() as f64 / 1e6);

    // Thumbnails under other encodings.
    let (tw, th, bw, bh) = thumb_dims;
    println!("thumbnails: {} pairs, top {tw}x{th}, bottom {bw}x{bh}", thumbs_raw_top.len());
    if thumbs_raw_top.is_empty() { return; }
    let alone: usize = thumbs_raw_top.iter().chain(thumbs_raw_bottom.iter()).map(|t| compress_data(t, 3).unwrap().len()).sum();
    let alone19: usize = thumbs_raw_top.iter().chain(thumbs_raw_bottom.iter()).map(|t| compress_data(t, 19).unwrap().len()).sum();
    println!("  zstd3 each alone {:.2} MB, zstd19 each alone {:.2} MB", alone as f64 / 1e6, alone19 as f64 / 1e6);
    // Prefixed by the previous thumbnail of the same screen.
    let mut pref = 0usize;
    for list in [&thumbs_raw_top, &thumbs_raw_bottom] {
        for i in 0..list.len() {
            let prefix: &[u8] = if i == 0 { &[] } else { &list[i - 1] };
            pref += compress_data_with_prefix(&list[i], 3, prefix).unwrap().len();
        }
    }
    println!("  zstd3 prefixed by previous {:.2} MB", pref as f64 / 1e6);
    // Random access: groups of G pictures, the first alone, the rest prefixed by the one before;
    // a screen identical to the previous picture costs one flag byte. Decoding a picture needs
    // its group up to it.
    for g in [10usize, 30, 60, 120] {
        let (mut sum, mut worst_decode) = (0usize, 0f64);
        for list in [&thumbs_raw_top, &thumbs_raw_bottom] {
            let mut frames: Vec<Vec<u8>> = Vec::new();
            for i in 0..list.len() {
                let same = i % g != 0 && list[i] == list[i - 1];
                if same { sum += 1; frames.push(Vec::new()); continue; }
                let prefix: &[u8] = if i % g == 0 { &[] } else { &list[i - 1] };
                let f = compress_data_with_prefix(&list[i], 3, prefix).unwrap();
                sum += f.len() + 1;
                frames.push(f);
            }
            // Decode time of the last picture of the first full group.
            if list.len() >= g {
                let t = std::time::Instant::now();
                let mut prev: Vec<u8> = Vec::new();
                for i in 0..g {
                    if frames[i].is_empty() { continue; }
                    prev = supershuckie_replay_recorder::decompress_data_with_prefix(&frames[i], list[i].len(), if i == 0 { &[] } else { &prev }).unwrap();
                }
                worst_decode = worst_decode.max(t.elapsed().as_secs_f64() * 1000.0);
            }
        }
        println!("  groups of {g}: {:.2} MB, worst group decode {worst_decode:.2} ms", sum as f64 / 1e6);
    }
    // Half resolution (2x2 box of the stored picture) and 8-bit palette-less RGB332 as bounds.
    let half = |raw: &[u8], w: usize, h: usize| -> Vec<u8> {
        let px = |x: usize, y: usize| u16::from_le_bytes([raw[(y * w + x) * 2], raw[(y * w + x) * 2 + 1]]);
        let mut out = Vec::with_capacity(w * h / 2);
        for y in (0..h - 1).step_by(2) { for x in (0..w - 1).step_by(2) {
            let (mut r, mut g, mut b) = (0u32, 0u32, 0u32);
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] { let p = px(x + dx, y + dy) as u32; r += p >> 11; g += (p >> 5) & 63; b += p & 31; }
            let p = (((r / 4) << 11) | ((g / 4) << 5) | (b / 4)) as u16;
            out.extend_from_slice(&p.to_le_bytes());
        } }
        out
    };
    let mut half_sum = 0usize;
    let mut half_pref = 0usize;
    let mut prev: [Vec<u8>; 2] = [Vec::new(), Vec::new()];
    for i in 0..thumbs_raw_top.len() {
        for (s, (raw, w, h)) in [(&thumbs_raw_top[i], tw as usize, th as usize), (&thumbs_raw_bottom[i], bw as usize, bh as usize)].into_iter().enumerate() {
            let hh = half(raw, w, h);
            half_sum += compress_data(&hh, 3).unwrap().len();
            half_pref += compress_data_with_prefix(&hh, 3, &prev[s]).unwrap().len();
            prev[s] = hh;
        }
    }
    println!("  half resolution: zstd3 alone {:.2} MB, prefixed by previous {:.2} MB", half_sum as f64 / 1e6, half_pref as f64 / 1e6);
    // Change detection: pairs identical to the previous pair.
    let same = (1..thumbs_raw_top.len()).filter(|&i| thumbs_raw_top[i] == thumbs_raw_top[i - 1] && thumbs_raw_bottom[i] == thumbs_raw_bottom[i - 1]).count();
    let same_top = (1..thumbs_raw_top.len()).filter(|&i| thumbs_raw_top[i] == thumbs_raw_top[i - 1]).count();
    let same_bottom = (1..thumbs_raw_top.len()).filter(|&i| thumbs_raw_bottom[i] == thumbs_raw_bottom[i - 1]).count();
    println!("  identical to previous: pair {same}, top {same_top}, bottom {same_bottom}");
    if let Some(dir) = dump_thumbs {
        std::fs::create_dir_all(&dir).unwrap();
        for i in (0..thumbs_raw_top.len()).step_by(thumbs_raw_top.len() / 40 + 1) {
            std::fs::write(dir.join(format!("t{i:05}-top.rgb565")), &thumbs_raw_top[i]).unwrap();
            std::fs::write(dir.join(format!("t{i:05}-bottom.rgb565")), &thumbs_raw_bottom[i]).unwrap();
        }
    }
}
