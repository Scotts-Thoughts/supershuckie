//! Where the compressed bytes of a format-v9 Nintendo 3DS replay's keyframes come from: decode
//! every stored keyframe in file order, and for each consecutive pair attribute the change (raw
//! changed bytes, literal bytes after ROM copies, and the *marginal* compressed cost) to memory
//! regions, plus how often each 4 KB page changes. Research tool.
//!
//! ```text
//! n3ds_attrib_lab <file.replay> --rom <game file> [--sample N] [--max-pairs N] [--heavy] [--hot PCT]
//! ```
//! `--sample N`: recompress every Nth pair for the marginal attribution (default 10).
//! `--hot PCT`: pages changing in at least PCT % of pairs count as "hot" (default 90).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::{compress_data, compress_data_with_state_prefix, decompress_data_with_prefix, region_diff_resizing, Packet};

const HEADER: usize = 32;
const VRAM: usize = 6 << 20;
const DSP: usize = 512 << 10;
const N3DS_EXTRA: usize = 4 << 20;
const PAGE: usize = 4096;

#[derive(Clone)]
struct Layout {
    fcram: usize,
    n3ds: bool,
    /// (name, start, end) coarse regions.
    regions: Vec<(String, usize, usize)>,
}

impl Layout {
    fn for_state(len: usize) -> Layout {
        let n3ds = len > 200 << 20;
        let fcram = if n3ds { 256 << 20 } else { 128 << 20 };
        let mut regions = Vec::new();
        let mut at = HEADER;
        regions.push(("header".to_owned(), 0, HEADER));
        let chunk = 8 << 20;
        for i in 0..fcram / chunk {
            regions.push((format!("fcram {:3}-{:3} MB", i * 8, i * 8 + 8), at + i * chunk, at + (i + 1) * chunk));
        }
        at += fcram;
        regions.push(("vram".to_owned(), at, at + VRAM));
        at += VRAM;
        if n3ds {
            regions.push(("n3ds extra".to_owned(), at, at + N3DS_EXTRA));
            at += N3DS_EXTRA;
        }
        regions.push(("dsp".to_owned(), at, at + DSP));
        at += DSP;
        regions.push(("archive".to_owned(), at, usize::MAX));
        Layout { fcram, n3ds, regions }
    }
    fn region_of(&self, off: usize) -> usize {
        // Regions are sorted and contiguous.
        match self.regions.binary_search_by(|(_, s, e)| if off < *s { std::cmp::Ordering::Greater } else if off >= *e { std::cmp::Ordering::Less } else { std::cmp::Ordering::Equal }) {
            Ok(i) => i,
            Err(_) => self.regions.len() - 1,
        }
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

fn unzigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

/// Runs (state byte offset, byte len) of a region diff's control stream.
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

/// ROM copies (data offset, rom offset, len) of a v9 refs stream.
fn copies(refs: &[u8]) -> Vec<(usize, u64, usize)> {
    let mut r = refs;
    let mut out = Vec::new();
    let (mut data_pos, mut rom_pos) = (0usize, 0u64);
    while !r.is_empty() {
        let gap = read_leb(&mut r) as usize;
        let delta = read_leb(&mut r);
        let len = read_leb(&mut r) as usize;
        data_pos += gap;
        let start = rom_pos.wrapping_add(unzigzag(delta) as u64);
        out.push((data_pos, start, len));
        data_pos += len;
        rom_pos = start + len as u64;
    }
    out
}

struct Stats {
    pairs: usize,
    raw: Vec<u64>,
    literal: Vec<u64>,
    copied: Vec<u64>,
    marginal: Vec<f64>,
    marginal_pairs: usize,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: n3ds_attrib_lab <file.replay> --rom <game> [--sample N] [--max-pairs N] [--heavy] [--hot PCT]");
    let (mut rom_path, mut sample, mut max_pairs, mut heavy, mut hot_pct) = (None::<String>, 10usize, usize::MAX, false, 90u32);
    let mut dump_dir: Option<String> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--rom" => rom_path = args.next(),
            "--sample" => sample = args.next().unwrap().parse().unwrap(),
            "--max-pairs" => max_pairs = args.next().unwrap().parse().unwrap(),
            "--heavy" => heavy = true,
            "--hot" => hot_pct = args.next().unwrap().parse().unwrap(),
            "--dump-dir" => dump_dir = args.next(),
            o => panic!("unknown argument {o}")
        }
    }
    let file = std::fs::File::open(&path).expect("open");
    let map = unsafe { memmap2::Mmap::map(&file) }.expect("map");
    let bytes: Arc<memmap2::Mmap> = Arc::new(map);
    let source: Arc<dyn AsRef<[u8]> + Send + Sync> = bytes.clone();
    let mut player = ReplayFilePlayer::new_shared(source, false).expect("parse");
    let rom: Option<Arc<memmap2::Mmap>> = rom_path.map(|p| {
        let f = std::fs::File::open(&p).expect("open ROM");
        Arc::new(unsafe { memmap2::Mmap::map(&f) }.expect("map ROM"))
    });
    if let Some(r) = &rom {
        let r2: Arc<dyn AsRef<[u8]> + Send + Sync> = r.clone();
        player.set_rom(r2);
    }
    player.set_keyframe_states_wanted(false);
    player.set_materialise_every_keyframe(true);

    // Stored keyframes in file order.
    let kfs: Vec<(u64, u8, usize, usize, usize, usize)> = player.all_uncompressed_packets().iter().filter_map(|p| match p {
        Packet::StoredKeyframe { metadata, level, state_len, uncompressed_len, frame_len, frame_offset } =>
            Some((metadata.elapsed_frames, *level, *state_len as usize, *uncompressed_len as usize, *frame_len as usize, *frame_offset as usize)),
        _ => None
    }).collect();
    println!("{}: v{}, {} frames, {} stored keyframes", path, player.get_replay_version(), player.get_total_frames(), kfs.len());

    let mut layout: Option<Layout> = None;
    let mut stats: Option<Stats> = None; // region index nreg = "vram hot2" pseudo-region
    let mut churn: HashMap<usize, u32> = HashMap::new(); // page -> pairs it changed in (RAM only)
    let mut fine_vram: HashMap<usize, u64> = HashMap::new();
    let mut fine_arch: HashMap<usize, u64> = HashMap::new();
    let mut vram_range = (0usize, 0usize);
    let mut prev_vram_pages: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut prev: Option<Vec<u8>> = None;
    let mut prev_l01: Option<Vec<u8>> = None;
    let mut k = 0usize;
    let mut pairs_l2 = 0usize;
    let (mut ctl_total, mut refs_total, mut frame_total, mut lit_total, mut copied_total, mut copies_total) = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    let (mut l1_frame_total, mut l1_count) = (0u64, 0usize);
    let mut heavy_rows: Vec<(usize, usize, usize, usize, usize, f64, f64)> = Vec::new();
    let t0 = Instant::now();
    loop {
        let packet = match player.next_packet() { Ok(Some(p)) => p, Ok(None) => break, Err(e) => panic!("{e:?}") };
        if !matches!(packet, Packet::Keyframe { .. }) { continue; }
        let state = player.current_keyframe_state().to_vec();
        let (frame, level, state_len, uncompressed_len, frame_len, frame_offset) = kfs[k];
        assert_eq!(state.len(), state_len, "keyframe {k} at frame {frame}");
        if let Some(dir) = dump_dir.as_ref() && matches!(k, 0 | 1 | 2 | 300 | 301 | 302) {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(format!("{dir}/state-{k:04}-L{level}-f{frame}.raw"), &state).unwrap();
        }
        let layout = layout.get_or_insert_with(|| {
            let l = Layout::for_state(state_len);
            println!("state {} MB, {}, FCRAM {} MB, archive {:.2} MB", state_len >> 20, if l.n3ds { "New 3DS" } else { "Old 3DS" }, l.fcram >> 20,
                     (state_len - l.regions.last().unwrap().1) as f64 / 1e6);
            l
        });
        let nreg = layout.regions.len();
        if vram_range.1 == 0 { let v = layout.regions.iter().find(|r| r.0 == "vram").unwrap(); vram_range = (v.1, v.2); }
        let stats = stats.get_or_insert_with(|| Stats { pairs: 0, raw: vec![0; nreg + 2], literal: vec![0; nreg + 2], copied: vec![0; nreg + 2], marginal: vec![0.0; nreg + 2], marginal_pairs: 0 });

        if level == 1 { l1_frame_total += frame_len as u64; l1_count += 1; }
        if level == 2 && pairs_l2 < max_pairs {
            if let Some(prev) = prev.as_ref() {
                // The stored frame, decoded with the reference state as prefix: the actual v9 payload.
                let frame_bytes = &bytes[frame_offset..frame_offset + frame_len];
                let payload = decompress_data_with_prefix(frame_bytes, uncompressed_len, prev).expect("stored delta decodes");
                let control_len = u64::from_le_bytes(payload[..8].try_into().unwrap()) as usize;
                let refs_len = u64::from_le_bytes(payload[8..16].try_into().unwrap()) as usize;
                let control = &payload[16..16 + control_len];
                let refs = &payload[16 + control_len..16 + control_len + refs_len];
                let literal = &payload[16 + control_len + refs_len..];
                let rs = runs(control);
                let cps = copies(refs);
                ctl_total += control_len as u64; refs_total += refs_len as u64; frame_total += frame_len as u64; lit_total += literal.len() as u64;
                let copied: usize = cps.iter().map(|c| c.2).sum();
                copied_total += copied as u64; copies_total += cps.len() as u64;

                // Check against a fresh diff (sanity) and attribute.
                let diff = region_diff_resizing(prev, &state);
                assert_eq!(diff.control, control, "control differs from a fresh diff at keyframe {k}");
                let data = &diff.data;
                assert_eq!(data.len(), literal.len() + copied);
                // Per data byte: state offset and copy membership.
                let mut in_copy = vec![false; data.len()];
                for &(d, _, len) in &cps { for b in &mut in_copy[d..d + len] { *b = true; } }
                let hot_threshold = hot_pct; // percent
                let _ = hot_threshold;
                let mut region_of_data = vec![0u8; data.len()];
                let mut dpos = 0usize;
                let mut pages_this_pair: std::collections::HashSet<usize> = std::collections::HashSet::new();
                let mut vram_pages_now: std::collections::HashSet<usize> = std::collections::HashSet::new();
                for &(off, len) in &rs {
                    let mut r = layout.region_of(off);
                    if off >= vram_range.0 && off < vram_range.1 {
                        vram_pages_now.insert(off / PAGE);
                        if prev_vram_pages.contains(&(off / PAGE)) { r = nreg; }
                    }
                    for b in &mut region_of_data[dpos..dpos + len] { *b = r as u8; }
                    stats.raw[r] += len as u64;
                    // Page churn (RAM regions only: the archive shifts).
                    if r != nreg - 1 && r != 0 && r != nreg {
                        let mut p = off / PAGE;
                        while p * PAGE < off + len { pages_this_pair.insert(p); p += 1; }
                    }
                    // Fine histograms: VRAM in 64 KB buckets (changed bytes), archive in 64 KB buckets.
                    let r = if r == nreg { layout.region_of(off) } else { r };
                    let _ = r;
                    let (vram_start, vram_end) = vram_range;
                    let arch_start = layout.regions.last().unwrap().1;
                    if off >= vram_start && off < vram_end { *fine_vram.entry((off - vram_start) >> 16).or_default() += len as u64; }
                    if off >= arch_start { *fine_arch.entry((off - arch_start) >> 16).or_default() += len as u64; }
                    dpos += len;
                }
                for p in pages_this_pair { *churn.entry(p).or_default() += 1; }
                prev_vram_pages = vram_pages_now;
                for i in 0..data.len() {
                    let r = region_of_data[i] as usize;
                    if in_copy[i] { stats.copied[r] += 1; } else { stats.literal[r] += 1; }
                }
                stats.pairs += 1;

                if pairs_l2 % sample == 0 {
                    // Marginal compressed cost of each region: compress the literal without it.
                    let lit_all: Vec<u8> = (0..data.len()).filter(|&i| !in_copy[i]).map(|i| data[i]).collect();
                    let t = Instant::now();
                    let base = compress_data_with_state_prefix(&lit_all, 3, prev).unwrap().len();
                    let base_ms = t.elapsed().as_secs_f64() * 1000.0;
                    for r in 0..=nreg {
                        if stats.literal[r] == 0 && stats.raw[r] == 0 { continue; }
                        let lit: Vec<u8> = (0..data.len()).filter(|&i| !in_copy[i] && region_of_data[i] as usize != r).map(|i| data[i]).collect();
                        if lit.len() == lit_all.len() { continue; }
                        let c = compress_data_with_state_prefix(&lit, 3, prev).unwrap().len();
                        stats.marginal[r] += (base as f64 - c as f64).max(0.0);
                    }
                    stats.marginal_pairs += 1;
                    if heavy {
                        let t = Instant::now();
                        let l9 = compress_data_with_state_prefix(&lit_all, 9, prev).unwrap().len();
                        let l9_ms = t.elapsed().as_secs_f64() * 1000.0;
                        let plain = compress_data(&lit_all, 3).unwrap().len();
                        let plain19 = compress_data(&lit_all, 19).unwrap().len();
                        heavy_rows.push((k, frame_len, base, l9, plain, base_ms, l9_ms));
                        println!("  pair {k}: stored {frame_len}, literal {} -> zstd3+prefix {base} ({base_ms:.0} ms), zstd9+prefix {l9} ({l9_ms:.0} ms), plain zstd3 {plain}, plain zstd19 {plain19}, control {control_len}, refs {refs_len} ({} copies, {copied} bytes)", lit_all.len(), cps.len());
                    } else {
                        println!("  pair {k}: stored {frame_len}, literal {} -> zstd3+prefix {base} ({base_ms:.0} ms), control {control_len}, refs {refs_len} ({} copies, {copied} bytes)", lit_all.len(), cps.len());
                    }
                }
                pairs_l2 += 1;
            }
        }
        if level <= 1 { prev_l01 = Some(state.clone()); }
        prev = Some(state);
        k += 1;
    }
    let _ = prev_l01;
    let layout = layout.expect("no keyframes");
    let stats = stats.unwrap();
    println!("\n{} L2 pairs in {:.0} s; L1 frames: {} avg {:.2} MB", stats.pairs, t0.elapsed().as_secs_f64(), l1_count, l1_frame_total as f64 / l1_count.max(1) as f64 / 1e6);
    println!("L2 stored frames total {:.2} MB, avg {:.1} KB; per pair avg: control {:.1} KB, refs {:.1} KB, literal {:.1} KB, ROM-copied {:.1} KB in {:.0} copies",
             frame_total as f64 / 1e6, frame_total as f64 / stats.pairs as f64 / 1e3,
             ctl_total as f64 / stats.pairs as f64 / 1e3, refs_total as f64 / stats.pairs as f64 / 1e3,
             lit_total as f64 / stats.pairs as f64 / 1e3, copied_total as f64 / stats.pairs as f64 / 1e3, copies_total as f64 / stats.pairs as f64);
    println!("\n{:18} {:>12} {:>12} {:>12} {:>14} {:>7}", "region", "raw KB/pair", "lit KB/pair", "rom KB/pair", "marginal KB/pr", "share");
    let marg_sum: f64 = stats.marginal.iter().sum();
    let mut rows: Vec<(String, usize)> = layout.regions.iter().enumerate().map(|(r, (n, _, _))| (n.clone(), r)).collect();
    rows.push(("vram hot2 pages".to_owned(), layout.regions.len()));
    for (name, r) in rows {
        if stats.raw[r] == 0 { continue; }
        let m = stats.marginal[r] / stats.marginal_pairs.max(1) as f64 / 1e3;
        println!("{:18} {:12.1} {:12.1} {:12.1} {:14.1} {:6.1}%", name,
                 stats.raw[r] as f64 / stats.pairs as f64 / 1e3, stats.literal[r] as f64 / stats.pairs as f64 / 1e3,
                 stats.copied[r] as f64 / stats.pairs as f64 / 1e3, m, 100.0 * stats.marginal[r] / marg_sum.max(1.0));
    }
    println!("(marginals over {} sampled pairs; sum of marginals {:.1} KB/pair)", stats.marginal_pairs, marg_sum / stats.marginal_pairs.max(1) as f64 / 1e3);

    // Page churn: how many pages change in what share of pairs, and their raw bytes.
    let n = stats.pairs as f64;
    let classes = [(95.0, 101.0, ">=95%"), (50.0, 95.0, "50-95%"), (10.0, 50.0, "10-50%"), (1.0, 10.0, "1-10%"), (0.0, 1.0, "<1%")];
    println!("\npage churn (4 KB RAM pages; {} pages ever changed):", churn.len());
    let mut by_region_hot: Vec<(u64, u64)> = vec![(0, 0); layout.regions.len()];
    for (lo, hi, name) in classes {
        let (mut pages, mut changes) = (0u64, 0u64);
        for (&p, &c) in &churn {
            let pct = 100.0 * c as f64 / n;
            if pct >= lo && pct < hi {
                pages += 1; changes += c as u64;
                if lo >= f64::from(hot_pct) { let r = layout.region_of(p * PAGE); by_region_hot[r].0 += 1; by_region_hot[r].1 += c as u64; }
            }
        }
        println!("  {:7} {:8} pages, {:8.1} MB of RAM, {:8.1} KB changed per pair on average (upper bound: whole pages)", name, pages, pages as f64 * PAGE as f64 / 1e6, changes as f64 * PAGE as f64 / n / 1e3);
    }
    println!("hot pages (>= {hot_pct}% of pairs) by region:");
    for (r, (name, _, _)) in layout.regions.iter().enumerate() {
        if by_region_hot[r].0 > 0 { println!("  {:18} {:6} pages ({:.1} MB)", name, by_region_hot[r].0, by_region_hot[r].0 as f64 * PAGE as f64 / 1e6); }
    }
    // Finer: 1 MB buckets of raw churn for the top 24 buckets.
    let mut bucket: HashMap<usize, u64> = HashMap::new();
    for (&p, &c) in &churn { *bucket.entry(p * PAGE >> 20).or_default() += c as u64 * PAGE as u64; }
    let mut top: Vec<(usize, u64)> = bucket.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1));
    println!("busiest 1 MB buckets (whole changed pages per pair, upper bound):");
    for (b, c) in top.iter().take(24) {
        let off = *b << 20;
        let (name, s, _) = &layout.regions[layout.region_of(off)];
        println!("  state offset {:4} MB ({} +{:.1} MB): {:8.1} KB/pair", b, name.trim(), (off - s) as f64 / 1e6, *c as f64 / n / 1e3);
    }
    let n = stats.pairs as f64;
    println!("
VRAM by 64 KB bucket (changed KB per pair; only buckets > 1 KB):");
    let mut v: Vec<(usize, u64)> = fine_vram.into_iter().collect(); v.sort();
    for (b, c) in &v { if *c as f64 / n > 1000.0 { println!("  vram +{:5} KB: {:7.1} KB/pair", b * 64, *c as f64 / n / 1e3); } }
    println!("archive by 64 KB bucket (changed KB per pair; only buckets > 1 KB):");
    let mut v: Vec<(usize, u64)> = fine_arch.into_iter().collect(); v.sort();
    for (b, c) in &v { if *c as f64 / n > 1000.0 { println!("  archive +{:5} KB: {:7.1} KB/pair", b * 64, *c as f64 / n / 1e3); } }
    if heavy && !heavy_rows.is_empty() {
        let n = heavy_rows.len() as f64;
        let s = |f: &dyn Fn(&(usize, usize, usize, usize, usize, f64, f64)) -> f64| heavy_rows.iter().map(f).sum::<f64>() / n;
        println!("\nheavy (avg over {} pairs): stored {:.1} KB, zstd3+prefix {:.1} KB ({:.0} ms), zstd9+prefix {:.1} KB ({:.0} ms), plain zstd3 {:.1} KB",
                 heavy_rows.len(), s(&|r| r.1 as f64) / 1e3, s(&|r| r.2 as f64) / 1e3, s(&|r| r.5), s(&|r| r.3 as f64) / 1e3, s(&|r| r.6), s(&|r| r.4 as f64) / 1e3);
    }
}
