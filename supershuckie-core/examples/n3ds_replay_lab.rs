//! Replay a real Nintendo 3DS recording through the core and measure what the replay format
//! could do with it: keyframe size at other cadences (states captured every `--step` frames and
//! diffed per cadence), whether playback reproduces the recorded keyframes byte for byte, and
//! where seek time goes. Research tool for `replay-3ds-format-research.md`.
//!
//! ```text
//! n3ds_replay_lab <game.3ds> <file.replay> [--user-dir dir] [--start F] [--frames N]
//!                 [--step 120] [--cadences 120,240,480,960] [--seeks N] [--csv out.csv]
//! ```
//! `SUPERSHUCKIE_3DS_DRAW_LEAD=<n>` in the environment changes how many frames before the
//! target a seek draws (the core draws all of them by default).

use std::collections::HashMap;
use std::ffi::c_void;
use std::io::Write;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Instant;

use supershuckie_core::emulator::{Nintendo3DS, Nintendo3DSSettings};
use supershuckie_core::{std_timestamp_provider, SuperShuckieCore};
use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::{apply_region_diff_in_place, decompress_data, decompress_data_with_prefix, region_diff_resizing, Packet};
use zstd_sys::*;

fn zstd_len(data: &[u8], prefix: &[u8], level: i32, window_log: i32) -> usize {
    unsafe {
        let cctx = ZSTD_createCCtx();
        ZSTD_CCtx_setParameter(cctx, ZSTD_cParameter::ZSTD_c_compressionLevel, level);
        ZSTD_CCtx_setParameter(cctx, ZSTD_cParameter::ZSTD_c_windowLog, window_log);
        ZSTD_CCtx_setParameter(cctx, ZSTD_cParameter::ZSTD_c_enableLongDistanceMatching, 1);
        if !prefix.is_empty() {
            ZSTD_CCtx_refPrefix(cctx, prefix.as_ptr() as *const c_void, prefix.len());
        }
        let bound = ZSTD_compressBound(data.len());
        let mut out: Vec<u8> = Vec::with_capacity(bound);
        let n = ZSTD_compress2(cctx, out.as_mut_ptr() as *mut c_void, bound, data.as_ptr() as *const c_void, data.len());
        assert_eq!(ZSTD_isError(n), 0);
        ZSTD_freeCCtx(cctx);
        n
    }
}

/// FNV of each screen's pixels (top, bottom).
fn screen_hashes(core: &SuperShuckieCore) -> (u64, u64) {
    let h = |px: &[u32]| px.iter().fold(0xcbf29ce484222325u64, |h, p| (h ^ u64::from(*p)).wrapping_mul(0x100000001b3));
    let s = core.get_core().get_screens();
    (h(&s[0].pixels), h(&s[1].pixels))
}

fn heap_hash(core: &SuperShuckieCore) -> u64 {
    let heap = core.get_core().memory_region_data(0).unwrap_or(&[]);
    let mut h = 0xcbf29ce484222325u64;
    for c in heap.chunks(8) {
        let mut w = [0u8; 8];
        w[..c.len()].copy_from_slice(c);
        h ^= u64::from_le_bytes(w);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// The recorded keyframe states of a v8 file, decoded in order on demand.
struct FileKeyframes<'a> {
    bytes: &'a [u8],
    list: Vec<(u64, u8, usize, usize, usize, usize)>,
    next: usize,
    states: [Option<Vec<u8>>; 3],
    payloads: [Vec<u8>; 3],
}

impl<'a> FileKeyframes<'a> {
    fn new(bytes: &'a [u8], player: &ReplayFilePlayer) -> Self {
        let list = player.all_uncompressed_packets().iter().filter_map(|p| match p {
            Packet::StoredKeyframe { metadata, level, state_len, uncompressed_len, frame_len, frame_offset } =>
                Some((metadata.elapsed_frames, *level, *state_len as usize, *uncompressed_len as usize, *frame_len as usize, *frame_offset as usize)),
            _ => None
        }).collect();
        Self { bytes, list, next: 0, states: [None, None, None], payloads: [Vec::new(), Vec::new(), Vec::new()] }
    }

    /// The recorded state at `frame` if a keyframe sits there (decoding every keyframe before it).
    fn state_at(&mut self, frame: u64) -> Option<&Vec<u8>> {
        let mut found = None;
        while self.next < self.list.len() && self.list[self.next].0 <= frame {
            let (f, level, state_len, ulen, flen, off) = self.list[self.next];
            self.next += 1;
            let fb = &self.bytes[off..off + flen];
            let l = usize::from(level);
            let state = if level == 0 {
                decompress_data(fb, ulen).unwrap()
            } else {
                let payload = decompress_data_with_prefix(fb, ulen, &self.payloads[l]).unwrap();
                let mut s = self.states[l].clone().unwrap();
                s.resize(state_len, 0);
                let cl = u64::from_le_bytes(payload[..8].try_into().unwrap()) as usize;
                assert!(apply_region_diff_in_place(&mut s, &payload[8..8 + cl], &payload[8 + cl..]));
                for m in l..3 { self.payloads[m] = payload.clone(); }
                s
            };
            if level == 0 { for m in 0..3 { self.payloads[m].clear(); } }
            for m in l..3 { self.states[m] = Some(state.clone()); }
            if f == frame { found = Some(2); }
        }
        found.and(self.states[2].as_ref())
    }
}


fn main() {
    let mut args = std::env::args().skip(1);
    let rom_path = args.next().expect("usage: n3ds_replay_lab <game> <replay> ...");
    let replay_path = args.next().expect("replay");
    let mut user_dir = std::env::temp_dir().join("supershuckie-n3ds-replay-lab").join("user");
    let (mut start, mut frames, mut step, mut seeks, mut csv_path) = (0u64, 0u64, 120u64, 0usize, None::<String>);
    let mut cadences: Vec<u64> = vec![120, 240, 480, 960];
    while let Some(a) = args.next() {
        match a.as_str() {
            "--user-dir" => user_dir = args.next().unwrap().into(),
            "--start" => start = args.next().unwrap().parse().unwrap(),
            "--frames" => frames = args.next().unwrap().parse().unwrap(),
            "--step" => step = args.next().unwrap().parse().unwrap(),
            "--cadences" => cadences = args.next().unwrap().split(',').map(|s| s.parse().unwrap()).collect(),
            "--seeks" => seeks = args.next().unwrap().parse().unwrap(),
            "--csv" => csv_path = args.next(),
            o => panic!("unknown option {o}")
        }
    }
    std::fs::create_dir_all(&user_dir).unwrap();

    let t = Instant::now();
    let n3ds = Nintendo3DS::new_from_path(&rom_path, &[], user_dir.to_str().unwrap(), std_timestamp_provider(), &Nintendo3DSSettings::default()).expect("load game");
    let mut core = SuperShuckieCore::new(Box::new(n3ds), std_timestamp_provider());
    println!("core made in {:.0} ms", t.elapsed().as_secs_f64() * 1000.0);

    let bytes = std::fs::read(&replay_path).expect("read replay");
    let player = ReplayFilePlayer::new(&bytes[..], false).expect("parse");
    let total = player.get_total_frames();
    let mut file_kf = FileKeyframes::new(&bytes, &player);
    let player2 = ReplayFilePlayer::new(&bytes[..], false).expect("parse");
    let mut player2 = player2;
    player2.enable_threading();
    core.attach_replay_player(player2, true).expect("attach");
    drop(player);
    if frames == 0 { frames = total.saturating_sub(start); }
    let end = (start + frames).min(total.saturating_sub(1));

    if start > 0 {
        let t = Instant::now();
        core.go_to_replay_frame(start).expect("seek to start");
        println!("seek to start {start}: {:.0} ms", t.elapsed().as_secs_f64() * 1000.0);
    }

    // One worker per cadence (in order, so each keeps its previous payload for the base3 prefix).
    let (res_tx, res_rx) = mpsc::channel::<(u64, u64, usize, usize, usize, usize, f64)>();
    let mut senders: HashMap<u64, mpsc::SyncSender<(u64, Arc<Vec<u8>>, Arc<Vec<u8>>)>> = HashMap::new();
    let mut workers = Vec::new();
    for &c in &cadences {
        let (tx, rx) = mpsc::sync_channel::<(u64, Arc<Vec<u8>>, Arc<Vec<u8>>)>(3);
        let res = res_tx.clone();
        senders.insert(c, tx);
        workers.push(std::thread::spawn(move || {
            let mut prev_payload: Vec<u8> = Vec::new();
            while let Ok((frame, prev, cur)) = rx.recv() {
                let t = Instant::now();
                let diff = region_diff_resizing(&prev, &cur);
                let mut payload = Vec::with_capacity(8 + diff.control.len() + diff.data.len());
                payload.extend_from_slice(&(diff.control.len() as u64).to_le_bytes());
                payload.extend_from_slice(&diff.control);
                payload.extend_from_slice(&diff.data);
                let base3 = zstd_len(&payload, &prev_payload, 3, 27);
                let full3 = zstd_len(&payload, &prev, 3, 28);
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                let _ = res.send((c, frame, diff.data.len(), payload.len(), base3, full3, ms));
                prev_payload = payload;
            }
        }));
    }
    drop(res_tx);

    let mut prev: HashMap<u64, Arc<Vec<u8>>> = HashMap::new();
    let mut matches = (0usize, 0usize);
    let mut heap_hashes: Vec<(u64, u64, (u64, u64))> = Vec::new();
    let mut save_ms_sum = 0f64;
    let mut saves = 0usize;
    let t_play = Instant::now();
    let mut frames_run = 0u64;
    // Frame times: the frame right after a capture versus the rest.
    let (mut after_ms, mut after_n, mut other_ms, mut other_n, mut just_captured) = (0f64, 0usize, 0f64, 0usize, false);
    while core.total_frames() < end && !core.is_replay_stalled() {
        let tf = Instant::now();
        core.run_unlocked();
        let fms = tf.elapsed().as_secs_f64() * 1000.0;
        if just_captured { after_ms += fms; after_n += 1; } else { other_ms += fms; other_n += 1; }
        just_captured = false;
        frames_run += 1;
        let f = core.total_frames();
        if f % step != 1 {
            continue;
        }
        // Keyframes in the file sit at frames 1 + 480k: capture on the same phase.
        let mut buf = Vec::new();
        let t = Instant::now();
        core.get_core().create_save_state_into(&mut buf);
        save_ms_sum += t.elapsed().as_secs_f64() * 1000.0;
        saves += 1;
        let cur = Arc::new(buf);
        just_captured = true;
        if let Some(rec) = file_kf.state_at(f) {
            // Raw layout: 32-byte header, RAM regions (Old 3DS: FCRAM 128 MiB, VRAM 6 MiB, DSP
            // 512 KiB), then the archive of everything else.
            const RAM_END: usize = 32 + (128 << 20) + (6 << 20) + (512 << 10);
            let ram_same = rec.len() > RAM_END && cur.len() > RAM_END && rec[32..RAM_END] == cur[32..RAM_END];
            let archive_same = rec[RAM_END..] == cur[RAM_END..];
            if ram_same && archive_same { matches.0 += 1 } else {
                matches.1 += 1;
                let ram_diff = rec[32..RAM_END].iter().zip(cur[32..RAM_END].iter()).filter(|(a, b)| a != b).count();
                println!("frame {f}: playback differs from the recorded keyframe: RAM {ram_diff} bytes differ, archive {} vs {} bytes{}",
                         cur.len() - RAM_END, rec.len() - RAM_END, if archive_same { " (same)" } else { "" });
                if let Some(dir) = std::env::var_os("N3DS_LAB_DUMP") {
                    let dir = std::path::PathBuf::from(dir);
                    let _ = std::fs::create_dir_all(&dir);
                    let _ = std::fs::write(dir.join(format!("{f}-recorded.archive")), &rec[RAM_END..]);
                    let _ = std::fs::write(dir.join(format!("{f}-playback.archive")), &cur[RAM_END..]);
                }
            }
        }
        heap_hashes.push((f, heap_hash(&core), screen_hashes(&core)));
        for &c in &cadences {
            if (f - 1) % c != 0 { continue; }
            if let Some(p) = prev.get(&c) {
                senders[&c].send((f, p.clone(), cur.clone())).unwrap();
            }
            prev.insert(c, cur.clone());
        }
        if f % 3600 == 1 {
            println!("frame {f} / {end}: {:.0} fps, keyframe match {}/{}", frames_run as f64 / t_play.elapsed().as_secs_f64(), matches.0, matches.0 + matches.1);
        }
    }
    drop(senders);
    drop(prev);
    for w in workers { w.join().unwrap(); }
    println!("frame after a capture {:.2} ms, other frames {:.2} ms", after_ms / after_n.max(1) as f64, other_ms / other_n.max(1) as f64);
    let mut csv = csv_path.map(|p| std::io::BufWriter::new(std::fs::File::create(p).unwrap()));
    if let Some(c) = csv.as_mut() { writeln!(c, "cadence,frame,changed,payload,base3,full3,ms").unwrap(); }
    let mut sums: HashMap<u64, (usize, usize, usize, usize)> = HashMap::new();
    for (cadence, frame, changed, plen, base3, full3, ms) in res_rx.iter() {
        let e = sums.entry(cadence).or_default();
        e.0 += 1; e.1 += changed; e.2 += base3; e.3 += full3;
        if let Some(c) = csv.as_mut() { writeln!(c, "{cadence},{frame},{changed},{plen},{base3},{full3},{ms:.0}").unwrap(); }
    }
    let secs = t_play.elapsed().as_secs_f64();
    println!("played {frames_run} frames in {secs:.1} s ({:.0} fps incl. captures); {saves} captures, {:.1} ms each; recorded keyframes reproduced {}/{}",
             frames_run as f64 / secs, save_ms_sum / saves.max(1) as f64, matches.0, matches.0 + matches.1);
    let mut cs: Vec<_> = sums.iter().collect();
    cs.sort();
    let hours = frames_run as f64 / 60.0 / 3600.0;
    for (c, (n, changed, base3, full3)) in cs {
        println!("cadence {c:5} frames: {n:5} keyframes, avg changed {:.2} MB, base3 {:.3} MB/kf ({:.0} MB/h), full3 {:.3} MB/kf ({:.0} MB/h)",
                 *changed as f64 / *n as f64 / 1e6, *base3 as f64 / *n as f64 / 1e6, *base3 as f64 / 1e6 / hours, *full3 as f64 / *n as f64 / 1e6, *full3 as f64 / 1e6 / hours);
    }

    // Seeks: random targets inside the played range, checked against the heap hash playback saw.
    if seeks > 0 && heap_hashes.len() > 2 {
        let mut rng = 0x2545F4914F6CDD1Du64;
        let mut times = Vec::new();
        let mut ok = 0;
        let mut screens_ok = (0usize, 0usize);
        for _ in 0..seeks {
            rng ^= rng << 13; rng ^= rng >> 7; rng ^= rng << 17;
            let (target, want, want_screens) = heap_hashes[(rng % heap_hashes.len() as u64) as usize];
            // go_to_replay_frame(target) shows `target`, having emulated up to target - 1: seek
            // to target + 1 so that total_frames() == target + 1 ... keep it simple: compare at
            // the frame the capture was taken after (total_frames() == target).
            let t = Instant::now();
            let kf = core.go_to_replay_keyframe(target.saturating_sub(1)).expect("keyframe");
            let load_ms = t.elapsed().as_secs_f64() * 1000.0;
            let t2 = Instant::now();
            let lead: u64 = std::env::var("SUPERSHUCKIE_3DS_DRAW_LEAD").ok().and_then(|v| v.parse().ok()).unwrap_or(u64::MAX);
            while core.total_frames() < target {
                if core.total_frames().saturating_add(lead) < target { core.run_unlocked_hidden(); } else { core.run_unlocked(); }
            }
            let run_ms = t2.elapsed().as_secs_f64() * 1000.0;
            let same = heap_hash(&core) == want;
            let screens = screen_hashes(&core);
            if same { ok += 1; }
            if screens.0 == want_screens.0 { screens_ok.0 += 1; }
            if screens.1 == want_screens.1 { screens_ok.1 += 1; }
            times.push((target, target - kf, load_ms, run_ms));
            println!("seek {target}: keyframe {kf} (+{} frames) load {load_ms:.0} ms, run {run_ms:.0} ms ({:.0} fps), heap {}, top {}, bottom {}",
                     target - kf, (target - kf) as f64 * 1000.0 / run_ms.max(0.001), if same { "ok" } else { "DIFFERS" },
                     if screens.0 == want_screens.0 { "ok" } else { "DIFFERS" }, if screens.1 == want_screens.1 { "ok" } else { "DIFFERS" });
        }
        let n = times.len() as f64;
        println!("seeks: top screen matches playback {}/{}, bottom {}/{}", screens_ok.0, times.len(), screens_ok.1, times.len());
        println!("seeks: {ok}/{} reproduce playback; avg load {:.0} ms, avg run {:.0} ms, avg frames {:.0}",
                 times.len(), times.iter().map(|t| t.2).sum::<f64>() / n, times.iter().map(|t| t.3).sum::<f64>() / n, times.iter().map(|t| t.1 as f64).sum::<f64>() / n);
    }
    println!("done");
}
