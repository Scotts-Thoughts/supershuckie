//! Lab for importing replays of the original Super Shuckie (SuperShuckie64, "GameREC>" files):
//! re-plays the old input stream on today's Game Boy core from the old file's first save state,
//! records a new replay, and at every old keyframe compares the emulated memory with the old
//! state, loading the old state when they differ.
//!
//! ```text
//! gamerec_import_lab <decompressed-body> <rom> <out.replay> [--frames n] [--no-resync]
//! ```
//!
//! `decompressed-body` is the packet stream after the 512-byte header, zstd-decompressed.

use std::fs::File;
use std::io::BufWriter;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use supershuckie_core::emulator::{EmulatorCore, GameBoyColor, Input, Model, PartialReplayRecordMetadata};
use supershuckie_core::{MonotonicTimestampProvider, SuperShuckieCore, TimestampMicros};
use supershuckie_replay_recorder::replay_file::record::ReplayFileRecorderSettings;
use supershuckie_replay_recorder::replay_file::ReplayPatchFormat;
use supershuckie_replay_recorder::ByteVec;

const CGB_BOOT: &[u8] = include_bytes!("../../bootrom/cgb/cgb_boot/cgb_boot_fast.bin");

struct FakeClock(Arc<AtomicU64>);
impl MonotonicTimestampProvider for FakeClock {
    fn get_timestamp_microseconds(&mut self) -> TimestampMicros {
        self.0.load(Ordering::Relaxed)
    }
}

enum Old {
    AddState(u32, Vec<u8>),
    LoadState(u32),
    Input(u8),
    Write(u32, u8),
    Reset,
    Other,
}

fn parse(body: &[u8]) -> Vec<(u64, Old)> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    let mut frame = 0u64;
    let be = |b: &[u8]| b.iter().fold(0u64, |a, &x| a << 8 | x as u64);
    while pos + 2 <= body.len() {
        frame += body[pos] as u64;
        let t = body[pos + 1];
        pos += 2;
        let p = match t {
            255 | 18 => if t == 18 { Old::Reset } else { Old::Other },
            1 => { let n = be(&body[pos..pos + 8]) as usize; pos += 8 + n; Old::Other }
            0 => { pos += 32; let n = be(&body[pos..pos + 8]) as usize; pos += 8 + n; Old::Other }
            2 => { pos += 2; Old::Other }
            3 => { pos += 32; Old::Other }
            4 => { pos += 1; Old::Input(body[pos - 1]) }
            5..=7 => { pos += 1 << (t - 4); Old::Other }
            8..=11 => { let w = 1 << (t - 8); let n = be(&body[pos..pos + w]) as usize; pos += w + n; Old::Other }
            12 => {
                let n = be(&body[pos..pos + 8]) as usize;
                let data = body[pos + 8..pos + 8 + n].to_vec();
                pos += 8 + n;
                let idx = be(&body[pos..pos + 4]) as u32;
                pos += 4;
                Old::AddState(idx, data)
            }
            13 => { let idx = be(&body[pos..pos + 4]) as u32; pos += 4; Old::LoadState(idx) }
            14 => { let a = be(&body[pos..pos + 4]) as u32; let b = body[pos + 4]; pos += 5; Old::Write(a, b) }
            16 => { pos += 5; Old::Other }
            15 | 17 => { pos += 9; Old::Other }
            _ => panic!("unknown packet {t} at {pos}"),
        };
        out.push((frame, p));
    }
    out
}

fn input_from_mask(m: u8) -> Input {
    // GB_key_mask_t: right, left, up, down, a, b, select, start
    Input {
        d_right: m & 1 != 0,
        d_left: m & 2 != 0,
        d_up: m & 4 != 0,
        d_down: m & 8 != 0,
        a: m & 16 != 0,
        b: m & 32 != 0,
        select: m & 64 != 0,
        start: m & 128 != 0,
        ..Input::default()
    }
}

fn region_diff(a: &dyn EmulatorCore, b: &dyn EmulatorCore) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    for (i, r) in a.memory_regions().iter().enumerate() {
        let (Some(x), Some(y)) = (a.memory_region_data(i), b.memory_region_data(i)) else { continue };
        let n = x.iter().zip(y).filter(|(p, q)| p != q).count();
        if n > 0 && std::env::var_os("LAB_VERBOSE").is_some() {
            let bytes: Vec<String> = x.iter().zip(y).enumerate().filter(|(_, (p, q))| p != q).take(40)
                .map(|(k, (p, q))| format!("{:04X}:{p:02X}/{q:02X}", r.base_address as usize + k)).collect();
            println!("  {}: {}", r.name, bytes.join(" "));
        }
        if n > 0 {
            out.push((r.name.to_string(), n));
        }
    }
    out
}

fn fingerprint(core: &dyn EmulatorCore) -> [u8; 32] {
    let parts: Vec<&[u8]> = (0..core.memory_regions().len()).filter_map(|i| core.memory_region_data(i)).collect();
    supershuckie_replay_recorder::blake3_hash_slices(parts)
}

const FINGERPRINT_EVERY: u64 = 5000;

fn main() {
    let mut args = std::env::args().skip(1);
    let body_path = args.next().expect("body");
    let rom_path = args.next().expect("rom");
    let out_path = args.next().expect("out");
    let mut max_frames = u64::MAX;
    let mut resync = true;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--frames" => max_frames = args.next().unwrap().parse().unwrap(),
            "--no-resync" => resync = false,
            o => panic!("unexpected {o}"),
        }
    }

    let body = std::fs::read(&body_path).expect("body");
    let rom = std::fs::read(&rom_path).expect("rom");
    let mut packets = parse(&body);
    // LAB_SHIFT=n: apply inputs n frames later (negative: earlier).
    let shift: i64 = std::env::var("LAB_SHIFT").ok().map(|v| v.parse().unwrap()).unwrap_or(0);
    if shift != 0 {
        for p in packets.iter_mut() {
            if matches!(p.1, Old::Input(_)) && p.0 > 0 {
                p.0 = (p.0 as i64 + shift).max(1) as u64;
            }
        }
        packets.sort_by_key(|p| p.0);
    }
    drop(body);
    let total = packets.last().map(|p| p.0).unwrap_or(0);
    println!("{} packets, {} frames", packets.len(), total);

    let model = if std::env::var_os("LAB_CGB_B").is_some() { Model::CgbB } else { Model::Cgb0 };
    if let Ok(w) = std::env::var("LAB_WINDOW") {
        window_search(&packets, &rom, w.parse().unwrap(), model);
        return;
    }
    let clock = Arc::new(AtomicU64::new(0));
    let gb = GameBoyColor::new_from_rom(&rom, CGB_BOOT, None, model);
    let mut core = SuperShuckieCore::new(Box::new(gb), Box::new(FakeClock(clock.clone())));
    let mut probe = GameBoyColor::new_from_rom(&rom, CGB_BOOT, None, model);

    let mut states: std::collections::HashMap<u32, Vec<u8>> = Default::default();
    let mut i = 0;
    // Frame-0 setup: states and the initial load, before recording.
    while i < packets.len() && packets[i].0 == 0 {
        match &packets[i].1 {
            Old::AddState(idx, d) => { states.insert(*idx, d.clone()); }
            Old::LoadState(idx) => core.load_save_state(&states[idx]),
            Old::Input(m) => core.enqueue_input(input_from_mask(*m)),
            _ => {}
        }
        i += 1;
    }

    let final_file = BufWriter::new(File::create(&out_path).unwrap());
    let temp_file = BufWriter::new(File::create(format!("{out_path}.temp")).unwrap());
    core.start_recording_replay(PartialReplayRecordMetadata {
        rom_name: "Y25-Mew-Race.gbc".into(),
        rom_filename: "Y25-Mew-Race.gbc".into(),
        settings: ReplayFileRecorderSettings::default(),
        patch_format: ReplayPatchFormat::Unpatched,
        patch_target_checksum: Default::default(),
        patch_data: ByteVec::new(),
        frames_per_keyframe: NonZeroU64::new(600).unwrap(),
        final_file,
        temp_file,
    }).expect("record");

    let started = std::time::Instant::now();
    let (mut checks, mut mismatches, mut first_mismatch) = (0u64, 0u64, None);
    let mut frame = 0u64;
    let mut fingerprints = Vec::new();
    let us_per_frame = 1_000_000.0 * 70224.0 / 4_194_304.0;
    while frame < total.min(max_frames) {
        while i < packets.len() && packets[i].0 == frame {
            match &packets[i].1 {
                Old::AddState(idx, d) => {
                    checks += 1;
                    probe.load_save_state(d).expect("old state loads");
                    let diff = region_diff(core.get_core(), &probe);
                    if !diff.is_empty() {
                        mismatches += 1;
                        if first_mismatch.is_none() {
                            first_mismatch = Some(frame);
                            println!("first mismatch at frame {frame}: {diff:?}");
                        }
                        if resync {
                            core.load_save_state(d);
                        }
                    }
                    states.insert(*idx, d.clone());
                }
                Old::LoadState(idx) => core.load_save_state(&states[idx]),
                Old::Input(m) => core.enqueue_input(input_from_mask(*m)),
                Old::Write(a, b) => { core.enqueue_write(a & 0xFFFF, ByteVec::from(&[*b][..])); }
                Old::Reset => core.hard_reset(),
                Old::Other => {}
            }
            i += 1;
        }
        let before = core.total_frames();
        loop {
            core.run_unlocked_hidden();
            if core.total_frames() != before && !core.is_mid_frame() {
                break;
            }
        }
        assert_eq!(core.total_frames(), before + 1, "one frame per step");
        frame += 1;
        if frame % FINGERPRINT_EVERY == 0 {
            fingerprints.push((frame, fingerprint(core.get_core())));
        }
        clock.store((frame as f64 * us_per_frame) as u64, Ordering::Relaxed);
        if frame % 100_000 == 0 {
            println!("frame {frame}: {checks} checks, {mismatches} mismatches, {:.1}s", started.elapsed().as_secs_f64());
        }
    }
    println!("done: {frame} frames, {checks} keyframe checks, {mismatches} mismatches (first at {first_mismatch:?}), {:.1}s", started.elapsed().as_secs_f64());
    println!("closed: {:?}", core.stop_recording_replay());
    let _ = std::fs::remove_file(format!("{out_path}.temp"));
    println!("output {} bytes", std::fs::metadata(&out_path).unwrap().len());

    // Play the new replay back on a fresh core, as the app would, and compare fingerprints.
    let bytes = std::fs::read(&out_path).unwrap();
    let player = supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer::new(&bytes, false).expect("parse output");
    println!("output: {} frames, {} keyframes", player.get_total_frames(), player.all_keyframes().len());
    let gb = GameBoyColor::new_from_rom(&rom, CGB_BOOT, None, model);
    let mut play = SuperShuckieCore::new(Box::new(gb), supershuckie_core::std_timestamp_provider());
    play.attach_replay_player(player, false).expect("attach");
    let started = std::time::Instant::now();
    let (mut next, mut matched, mut differed) = (0usize, 0u64, 0u64);
    while !play.is_replay_stalled() && next < fingerprints.len() {
        play.run_unlocked_hidden();
        if play.is_mid_frame() {
            continue
        }
        let at = play.total_frames();
        if at == fingerprints[next].0 {
            if fingerprint(play.get_core()) == fingerprints[next].1 { matched += 1 } else {
                differed += 1;
                if differed <= 5 { println!("playback differs at frame {at}"); }
            }
            next += 1;
        }
    }
    println!("playback: {matched} fingerprints match, {differed} differ, {} not reached, {:.1}s", fingerprints.len() - next, started.elapsed().as_secs_f64());
}

/// Starting from the old keyframe at `start`, re-run to the next keyframe with every input after
/// frame `t` shifted by `k` frames, for every `t` in the window and small `k`; print the shifts
/// that reproduce the next old keyframe.
fn window_search(packets: &[(u64, Old)], rom: &[u8], start: u64, model: Model) {
    let state_at = |f: u64| packets.iter().find_map(|(fr, p)| match p { Old::AddState(_, d) if *fr == f => Some(d.clone()), _ => None });
    let s0 = state_at(start).expect("start keyframe");
    let end = start + 240;
    let s1 = state_at(end).expect("end keyframe");
    let inputs: Vec<(u64, u8)> = packets.iter().filter_map(|(f, p)| match p { Old::Input(m) if *f >= start && *f < end + 4 => Some((*f, *m)), _ => None }).collect();
    let last_before = packets.iter().filter_map(|(f, p)| match p { Old::Input(m) if *f < start => Some(*m), _ => None }).last().unwrap_or(0);
    let mut probe = GameBoyColor::new_from_rom(rom, CGB_BOOT, None, model);
    probe.load_save_state(&s1).unwrap();
    let mut gb = GameBoyColor::new_from_rom(rom, CGB_BOOT, None, model);
    let mut run = |t: u64, k: i64| -> usize {
        gb.load_save_state(&s0).unwrap();
        let shifted: Vec<(u64, u8)> = inputs.iter().map(|&(f, m)| (if f > t { (f as i64 + k) as u64 } else { f }, m)).collect();
        let mut cur = last_before;
        for f in start..end {
            for &(fi, m) in &shifted { if fi == f { cur = m; } }
            gb.set_input_encoded(&[cur]);
            loop { gb.run_unlocked(); if !gb.is_mid_frame() { break; } }
        }
        region_diff(&gb, &probe).iter().map(|d| d.1).sum()
    };
    println!("baseline diff bytes: {}", run(end, 0));
    for k in [-2i64, -1, 1, 2] {
        let mut hits = Vec::new();
        for t in start..end {
            if run(t, k) == 0 { hits.push(t); }
        }
        println!("shift {k}: matching cut points {} (first {:?}, last {:?})", hits.len(), hits.first(), hits.last());
    }
}
