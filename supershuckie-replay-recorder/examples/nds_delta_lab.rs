//! What Nintendo DS keyframe deltas cost and what they are made of. Research tool (2026-11-11).
//!
//! Reads a stretch of a DS replay's keyframes (no emulator) and reports:
//! 1. the compressed size of the delta chain when only every n-th recorded keyframe is kept
//!    (how file size falls as keyframes get sparser);
//! 2. which melonDS savestate sections the changed bytes are in, and what each compresses to;
//! 3. with `--rom`, how much of the changed main RAM is a verbatim copy of ROM bytes.
//!
//! ```text
//! nds_delta_lab <file.replay> [--rom <rom.nds>] [--from <frame>] [--count <keyframes>] [--level <zstd>]
//! ```

use std::collections::{BTreeMap, HashSet};
use std::time::Instant;

use supershuckie_replay_recorder::keyframe_masks::transient_ranges;
use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::replay_file::ReplayConsoleType;
use supershuckie_replay_recorder::{compress_data, region_diff, Packet};

const STRIDES: [usize; 7] = [1, 2, 4, 8, 15, 30, 60];
const BLOCK: usize = 32;
const BASE: u64 = 0x100000001b3;

fn read_leb(input: &mut &[u8]) -> u64 {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let b = input[0];
        *input = &input[1..];
        v |= u64::from(b & 0x7F) << shift;
        if b & 0x80 == 0 {
            return v;
        }
        shift += 7;
    }
}

/// (name, start, end) of each melonDS section, with the first (NDSG) split into its main RAM
/// and the rest.
fn sections(state: &[u8]) -> Vec<(String, usize, usize)> {
    let mut out = Vec::new();
    let mut offset = 16usize;
    while offset + 16 <= state.len() {
        let len = u32::from_le_bytes(state[offset + 4..offset + 8].try_into().unwrap()) as usize;
        if len < 16 {
            break;
        }
        let name = String::from_utf8_lossy(&state[offset..offset + 4]).into_owned();
        let end = (offset + len).min(state.len());
        if name == "NDSG" && len > 0x1000000 + 0x20 {
            // 16-byte header, a config word, then 16 MiB of main RAM (see nds_replay_survey).
            let ram = offset + 0x10 + 0x10;
            out.push(("NDSG head".into(), offset, ram));
            for mib in 0..16 {
                out.push((format!("main RAM {mib:2} MiB"), ram + (mib << 20), ram + ((mib + 1) << 20)));
            }
            out.push(("NDSG rest (WRAM, I/O)".into(), ram + 0x1000000, end));
        } else {
            out.push((name, offset, end));
        }
        offset += len;
    }
    out
}

fn block_hash(b: &[u8]) -> u64 {
    let mut h = 0u64;
    for &x in b {
        h = h.wrapping_mul(BASE).wrapping_add(u64::from(x) + 1);
    }
    h
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: nds_delta_lab <file.replay> [--rom rom] [--from frame] [--count n] [--level n]");
    let (mut rom, mut from, mut count, mut level) = (None, 0u64, 900usize, 3i32);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--rom" => rom = args.next(),
            "--from" => from = args.next().unwrap().parse().unwrap(),
            "--count" => count = args.next().unwrap().parse().unwrap(),
            "--level" => level = args.next().unwrap().parse().unwrap(),
            other => panic!("unknown argument {other}"),
        }
    }

    let bytes = std::fs::read(&path).expect("read replay");
    let mut player = ReplayFilePlayer::new(&bytes, false).expect("parse replay");
    player.set_keyframe_states_wanted(false);
    let total = player.get_total_frames();
    let keyframe_frames: Vec<u64> = player.all_keyframes().keys().copied().collect();
    let start = keyframe_frames.iter().copied().filter(|k| *k <= from).next_back().unwrap_or(keyframe_frames[0]);
    println!("{path}: {total} frames, {} keyframes, {} bytes ({:.1} MiB/h)", keyframe_frames.len(), bytes.len(), bytes.len() as f64 / 1048576.0 / (total as f64 / 215_374.0));
    player.go_to_keyframe(start).expect("seek");

    // Per stride: the previous kept state and the concatenated (control, data) of its deltas.
    let mut last: Vec<Option<Vec<u8>>> = vec![None; STRIDES.len()];
    let mut streams: Vec<Vec<u8>> = vec![Vec::new(); STRIDES.len()];
    let mut raw_data: Vec<usize> = vec![0; STRIDES.len()];
    let mut kept: Vec<usize> = vec![0; STRIDES.len()];
    // Attribution at strides 1 and 8 (index 0 and 3).
    let mut by_section: [BTreeMap<String, (usize, Vec<u8>)>; 2] = [BTreeMap::new(), BTreeMap::new()];
    let mut section_order: Vec<String> = Vec::new();
    let mut ram_blocks: HashSet<u64> = HashSet::new();
    let mut ram_block_count = 0usize;
    let mut ram_changed_bytes = 0usize;
    let mut ram_block_list: Vec<u64> = Vec::new();
    // (offset in streams[0], hash) of every hashed block, for the ROM-reference re-encode.
    let mut ram_block_at: Vec<(usize, u64)> = Vec::new();
    let mut full_blocks: Vec<u64> = Vec::new();

    let mut first_state: Option<Vec<u8>> = None;
    let mut transient: Option<(std::ops::Range<usize>, Vec<u8>)> = None;
    let mut seen = 0usize;
    let mut first_frame = 0;
    let mut last_frame = 0;
    let mut diff_seconds = 0.0;
    let mut diffs = 0usize;

    while seen < count {
        let Some(packet) = player.next_packet().expect("read replay") else { break };
        let frame = match packet {
            Packet::Keyframe { metadata, .. } => metadata.elapsed_frames,
            _ => continue,
        };
        let mut state = player.current_keyframe_state().to_vec();
        // What the recorder's masks do along a chain: the transient ranges stay the chain start's.
        match &transient {
            None => {
                if let Some(range) = transient_ranges(ReplayConsoleType::NintendoDS, &state).into_iter().next() {
                    transient = Some((range.clone(), state[range].to_vec()));
                }
            }
            Some((range, bytes)) => {
                if transient_ranges(ReplayConsoleType::NintendoDS, &state).into_iter().next().as_ref() == Some(range) {
                    state[range.clone()].copy_from_slice(bytes);
                }
            }
        }
        if first_state.is_none() {
            first_state = Some(state.clone());
            first_frame = frame;
            for (name, a, b) in sections(&state) {
                if name.starts_with("main RAM") {
                    for block in state[a..b].chunks_exact(BLOCK) {
                        if !block.iter().all(|x| *x == block[0]) {
                            let h = block_hash(block);
                            ram_blocks.insert(h);
                            full_blocks.push(h);
                        }
                    }
                }
            }
            for (name, ..) in sections(&state) {
                section_order.push(name);
            }
        }
        last_frame = frame;

        for (i, stride) in STRIDES.iter().enumerate() {
            if seen % stride != 0 {
                continue;
            }
            if let Some(prev) = &last[i] {
                if prev.len() != state.len() {
                    last[i] = Some(state.clone());
                    continue;
                }
                let t = Instant::now();
                let diff = region_diff(prev, &state).expect("same length");
                diff_seconds += t.elapsed().as_secs_f64();
                diffs += 1;
                streams[i].extend_from_slice(&diff.control);
                let data_base = streams[i].len();
                streams[i].extend_from_slice(&diff.data);
                raw_data[i] += diff.data.len();
                kept[i] += 1;

                let slot = match i { 0 => Some(0), 3 => Some(1), _ => None };
                if let Some(slot) = slot {
                    let secs = sections(&state);
                    let mut control = diff.control.as_slice();
                    let mut word = 0usize;
                    let mut data_at = 0usize;
                    let mut sec = 0usize;
                    while !control.is_empty() {
                        word += read_leb(&mut control) as usize;
                        let len = read_leb(&mut control) as usize;
                        let (mut at, end) = (word * 4, (word + len) * 4);
                        while at < end {
                            while sec + 1 < secs.len() && at >= secs[sec].2 {
                                sec += 1;
                            }
                            let stop = end.min(secs[sec].2.max(at + 1));
                            let piece_start = data_at + (at - word * 4);
                            let piece = &diff.data[piece_start..data_at + (stop - word * 4).min(len * 4)];
                            let entry = by_section[slot].entry(secs[sec].0.clone()).or_default();
                            entry.0 += piece.len();
                            entry.1.extend_from_slice(piece);
                            if slot == 0 && secs[sec].0.starts_with("main RAM") {
                                ram_changed_bytes += piece.len();
                                for (n, block) in piece.chunks_exact(BLOCK).enumerate() {
                                    if block.iter().all(|b| *b == block[0]) {
                                        continue;
                                    }
                                    let h = block_hash(block);
                                    ram_block_at.push((data_base + piece_start + n * BLOCK, h));
                                    ram_blocks.insert(h);
                                    ram_block_list.push(h);
                                    ram_block_count += 1;
                                }
                            }
                            at = stop;
                        }
                        data_at += len * 4;
                        word += len;
                    }
                }
            }
            last[i] = Some(state.clone());
        }
        seen += 1;
    }

    let span = last_frame - first_frame;
    let hours = span as f64 / 215_374.0;
    let first = first_state.expect("at least one keyframe");
    println!("\n{seen} keyframes read, frames {first_frame}..{last_frame} ({:.1} min); state {} bytes; region_diff {:.1} ms each", span as f64 / 3590.0, first.len(), diff_seconds * 1000.0 / diffs.max(1) as f64);
    println!("full keyframe at chain start: {} bytes compressed", compress_data(&first, level).unwrap().len());

    println!("\n[1] delta chain, every n-th recorded keyframe kept (zstd {level}, one frame per chain)");
    println!("  stride  interval   deltas   changed bytes/delta   compressed   MiB/hour   per delta");
    for (i, stride) in STRIDES.iter().enumerate() {
        if kept[i] == 0 {
            continue;
        }
        let c = compress_data(&streams[i], level).unwrap().len();
        println!(
            "  {:>5}  {:>6.0} s  {:>7}  {:>18.0}  {:>11}  {:>9.1}  {:>9.0}",
            stride,
            *stride as f64 * span as f64 / (seen.max(2) - 1) as f64 / 59.8261,
            kept[i],
            raw_data[i] as f64 / kept[i] as f64,
            c,
            c as f64 / 1048576.0 / hours,
            c as f64 / kept[i] as f64
        );
    }

    for (slot, label) in [(0usize, "every keyframe"), (1, "every 8th keyframe")] {
        println!("\n[2] where the changed bytes are ({label}); each section's bytes compressed on their own");
        let total_raw: usize = by_section[slot].values().map(|v| v.0).sum();
        let mut total_c = 0usize;
        let mut rows = Vec::new();
        for name in &section_order {
            if let Some((raw, data)) = by_section[slot].get(name) {
                let c = compress_data(data, level).unwrap().len();
                total_c += c;
                rows.push((name.clone(), *raw, c));
            }
        }
        for (name, raw, c) in rows {
            if raw * 500 < total_raw && c * 500 < total_c {
                continue;
            }
            println!("  {:<24} {:>12} raw ({:>5.1}%)  {:>11} compressed ({:>5.1}%)", name, raw, raw as f64 * 100.0 / total_raw as f64, c, c as f64 * 100.0 / total_c as f64);
        }
        println!("  {:<24} {:>12} raw           {:>11} compressed", "total", total_raw, total_c);
    }

    if let Some(rom) = rom {
        let rom = std::fs::read(&rom).expect("read rom");
        let t = Instant::now();
        let mut found: HashSet<u64> = HashSet::new();
        if rom.len() > BLOCK {
            let mut top = 1u64;
            for _ in 0..BLOCK - 1 {
                top = top.wrapping_mul(BASE);
            }
            let mut h = block_hash(&rom[..BLOCK]);
            if ram_blocks.contains(&h) {
                found.insert(h);
            }
            for i in BLOCK..rom.len() {
                h = h.wrapping_sub((u64::from(rom[i - BLOCK]) + 1).wrapping_mul(top));
                h = h.wrapping_mul(BASE).wrapping_add(u64::from(rom[i]) + 1);
                if ram_blocks.contains(&h) {
                    found.insert(h);
                }
            }
        }
        let matched = ram_block_list.iter().filter(|h| found.contains(h)).count();
        // Re-encode the stride-1 stream with the matched blocks taken out; each run of adjacent
        // matched blocks costs a 6-byte reference instead.
        let mut without: Vec<u8> = Vec::with_capacity(streams[0].len());
        let mut refs: Vec<u8> = Vec::new();
        let mut at = 0usize;
        let mut previous_end = usize::MAX;
        let mut runs = 0usize;
        for (offset, h) in &ram_block_at {
            if !found.contains(h) {
                continue;
            }
            without.extend_from_slice(&streams[0][at..*offset]);
            at = offset + BLOCK;
            if *offset != previous_end {
                runs += 1;
                refs.extend_from_slice(&(*offset as u32).to_le_bytes());
                refs.extend_from_slice(&(*h as u16).to_le_bytes());
            }
            previous_end = offset + BLOCK;
        }
        without.extend_from_slice(&streams[0][at..]);
        let before = compress_data(&streams[0], level).unwrap().len();
        let after = compress_data(&without, level).unwrap().len() + compress_data(&refs, level).unwrap().len();
        println!("    stride-1 chain: {} -> {} compressed with ROM references ({:+.1}%), {} reference runs", before, after, (after as f64 / before as f64 - 1.0) * 100.0, runs);
        let full_matched = full_blocks.iter().filter(|h| found.contains(h)).count();
        println!("    full keyframe: {} non-constant main RAM blocks ({} bytes), {:.1}% verbatim ROM", full_blocks.len(), full_blocks.len() * BLOCK, full_matched as f64 * 100.0 / full_blocks.len().max(1) as f64);
        println!(
            "\n[3] ROM copies: {} changed main RAM bytes, {} non-constant {BLOCK}-byte blocks, {} ({:.1}% of blocks, {:.1}% of changed RAM bytes) found verbatim in the {} MiB ROM (scan {:.1} s)",
            ram_changed_bytes,
            ram_block_count,
            matched,
            matched as f64 * 100.0 / ram_block_count.max(1) as f64,
            (matched * BLOCK) as f64 * 100.0 / ram_changed_bytes.max(1) as f64,
            rom.len() >> 20,
            t.elapsed().as_secs_f64()
        );
    }
}
