//! Play a real Nintendo 3DS recording through the core and measure the format-v9 recorder and
//! the seek path on it:
//!
//! * keyframes at other cadences: every `--step` frames the state is captured and handed, with
//!   the ROM reads Azahar logged since the last capture, to one real `ReplayFileRecorder` per
//!   cadence (levels: a level-1 keyframe every 2 minutes, a full one every 60), which reports
//!   bytes per hour and encode time per keyframe;
//! * seeks: random targets among the captures, each checked against the heap and both screens
//!   that straight playback had there, with the time it took and whether the seek had to redo
//!   its walk drawn (see `SuperShuckieCore::replay_seek_redraws`).
//!
//! ```text
//! n3ds_v9_lab <game file> <file.replay> [--user-dir dir] [--start F] [--frames N] [--step 120]
//!             [--cadences 120,240,480|none] [--seeks N] [--seed S] [--profile-seeks N]
//!             [--out <file.replay>]
//! ```
//! `--out` writes the first cadence's recording to a file (a real format-v10 3DS replay, VRAM
//! pages the GPU rewrites left out of its keyframes; `N3DS_LAB_NO_MASK=1` keeps them), which a
//! second run of this tool can then seek through and check against straight playback.
//! `SUPERSHUCKIE_3DS_SEEK_TAIL=<n>` changes how many frames before its target a seek draws
//! (`18446744073709551615` draws them all, as before skipping).

use std::collections::HashMap;
use std::io::Write;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Instant;

use supershuckie_core::emulator::{Nintendo3DS, Nintendo3DSSettings};
use supershuckie_core::{std_timestamp_provider, SuperShuckieCore};
use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::replay_file::record::{ReplayFileRecorder, ReplayFileRecorderSettings, ReplayFileSink, ReplayFileWriteError, NullReplayFileSink, TransientPageAccess};
use supershuckie_replay_recorder::replay_file::{ReplayConsoleType, ReplayFileMetadata, ReplayHeaderBytes, RomBytes};
use supershuckie_replay_recorder::{ByteVec, Packet, Speed};

/// Counts what a recorder writes, and keeps it in a file when asked.
struct CountingSink(u64, Option<std::io::BufWriter<std::fs::File>>);

impl ReplayFileSink for CountingSink {
    fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), ReplayFileWriteError> {
        self.0 += bytes.len() as u64;
        if let Some(file) = self.1.as_mut() {
            file.write_bytes(bytes)?;
        }
        Ok(())
    }
    fn truncate(&mut self, size: u64) -> Result<(), ReplayFileWriteError> {
        self.0 = self.0.min(size);
        if let Some(file) = self.1.as_mut() {
            file.truncate(size)?;
        }
        Ok(())
    }
    fn overwrite_header(&mut self, header: &ReplayHeaderBytes) -> Result<(), ReplayFileWriteError> {
        if let Some(file) = self.1.as_mut() {
            file.overwrite_header(header)?;
        }
        Ok(())
    }
}

fn fnv(bytes: impl Iterator<Item = u64>) -> u64 {
    bytes.fold(0xcbf29ce484222325u64, |h, w| (h ^ w).wrapping_mul(0x100000001b3))
}

fn screen_hashes(core: &SuperShuckieCore) -> (u64, u64) {
    let s = core.get_core().get_screens();
    (fnv(s[0].pixels.iter().map(|&p| u64::from(p))), fnv(s[1].pixels.iter().map(|&p| u64::from(p))))
}

fn heap_hash(core: &SuperShuckieCore) -> u64 {
    let heap = core.get_core().memory_region_data(0).unwrap_or(&[]);
    fnv(heap.chunks(8).map(|c| {
        let mut w = [0u8; 8];
        w[..c.len()].copy_from_slice(c);
        u64::from_le_bytes(w)
    }))
}

fn map(path: &str) -> RomBytes {
    let file = std::fs::File::open(path).unwrap_or_else(|e| panic!("open {path}: {e}"));
    // SAFETY: read-only mapping of a file nobody writes while this runs.
    Arc::new(unsafe { memmap2::Mmap::map(&file) }.expect("map"))
}

/// What a cadence's recorder thread is sent per frame: the source file's packets for the frame
/// (inputs, a timeline picture), the state when the frame is one of its keyframes, the ROM reads
/// of the frame, and what touched each VRAM page first in it.
#[derive(Clone)]
struct FrameMessage {
    frame: u64,
    current_input: Vec<u8>,
    inputs: Vec<Vec<u8>>,
    thumbnail: Option<[(u32, u32, Vec<u8>); 2]>,
    state: Option<Arc<Vec<u8>>>,
    reads: Vec<(u64, u64)>,
    access: Option<TransientPageAccess>,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let game_path = args.next().expect("usage: n3ds_v9_lab <game> <replay> ...");
    let replay_path = args.next().expect("replay");
    let mut user_dir = std::env::temp_dir().join("supershuckie-n3ds-v9-lab").join("user");
    let (mut start, mut frames, mut step, mut seeks, mut seed, mut profile) = (0u64, 0u64, 120u64, 0usize, 1u64, 0usize);
    let mut cadences: Vec<u64> = vec![120, 240, 480];
    let mut out: Option<String> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--user-dir" => user_dir = args.next().unwrap().into(),
            "--start" => start = args.next().unwrap().parse().unwrap(),
            "--frames" => frames = args.next().unwrap().parse().unwrap(),
            "--step" => step = args.next().unwrap().parse().unwrap(),
            "--cadences" => cadences = args.next().unwrap().split(',').filter(|s| !s.is_empty() && *s != "none").map(|s| s.parse().unwrap()).collect(),
            "--seeks" => seeks = args.next().unwrap().parse().unwrap(),
            "--seed" => seed = args.next().unwrap().parse().unwrap(),
            "--profile-seeks" => profile = args.next().unwrap().parse().unwrap(),
            "--out" => out = args.next(),
            o => panic!("unknown option {o}")
        }
    }
    assert!(cadences.iter().all(|c| c % step == 0), "every cadence must be a multiple of --step");
    std::fs::create_dir_all(&user_dir).unwrap();

    let rom = map(&game_path);
    let t = Instant::now();
    let n3ds = Nintendo3DS::new_from_path(&game_path, &[], user_dir.to_str().unwrap(), std_timestamp_provider(), &Nintendo3DSSettings::default()).expect("load game");
    let mut core = SuperShuckieCore::new(Box::new(n3ds), std_timestamp_provider());
    println!("core made in {:.0} ms", t.elapsed().as_secs_f64() * 1000.0);

    let source_bytes: Arc<dyn AsRef<[u8]> + Send + Sync> = map(&replay_path);
    let mut player = ReplayFilePlayer::new_shared(source_bytes.clone(), false).expect("parse");
    player.set_rom(rom.clone());
    let total = player.get_total_frames();
    println!("replay: version {}, {total} frames, {} keyframes", player.get_replay_version(), player.all_keyframes().len());
    core.attach_replay_player(player, true).expect("attach");
    if frames == 0 {
        frames = total.saturating_sub(start);
    }
    let end = (start + frames).min(total.saturating_sub(1));
    if start > 0 {
        // Load the keyframe and play drawn from there: a seek (which skips drawing) would put
        // whatever it leaves out of date into the ground truth.
        start = core.go_to_replay_keyframe(start).expect("keyframe at the start");
        println!("starting at keyframe {start}");
    }
    core.get_core_mut().set_rom_read_log(true);
    core.get_core_mut().set_transient_page_tracking(true);
    let mask = std::env::var_os("N3DS_LAB_NO_MASK").is_none();

    // One recorder thread per cadence. Every frame's packets of the source file (inputs, timeline
    // pictures) are forwarded to it, so the file it writes plays and seeks like a recording.
    let (result_tx, result_rx) = mpsc::channel::<(u64, u64, usize, f64, f64)>();
    let mut senders: HashMap<u64, mpsc::SyncSender<FrameMessage>> = HashMap::new();
    let mut workers = Vec::new();
    for (i, &cadence) in cadences.iter().enumerate() {
        let (tx, rx) = mpsc::sync_channel::<FrameMessage>(16);
        senders.insert(cadence, tx);
        let (result, rom) = (result_tx.clone(), rom.clone());
        let out_file = if i == 0 { out.as_ref().map(|p| std::io::BufWriter::new(std::fs::File::create(p).expect("create --out"))) } else { None };
        workers.push(std::thread::spawn(move || {
            // A level-1 keyframe every 2 minutes, a full one every 60.
            let level1_every = (7200 / cadence).max(1) as u32;
            let zstd = std::env::var("N3DS_LAB_ZSTD").ok().and_then(|v| v.parse().ok()).unwrap_or(3);
            let settings = ReplayFileRecorderSettings { stored_keyframe_levels: (30, level1_every), stored_keyframe_mask_transients: mask, stored_keyframe_compression_level: zstd, ..Default::default() };
            let mut recorder: Option<ReplayFileRecorder<CountingSink, NullReplayFileSink>> = None;
            let mut sink = Some(CountingSink(0, out_file));
            let (mut keyframes, mut encode_ms, mut worst_ms) = (0usize, 0f64, 0f64);
            while let Ok(message) = rx.recv() {
                let FrameMessage { frame, current_input, inputs, thumbnail, state, reads, access } = message;
                if let Some(recorder) = recorder.as_mut() {
                    for input in &inputs {
                        recorder.set_input(input.iter().copied().collect()).expect("input");
                    }
                    if let Some([top, bottom]) = thumbnail.as_ref() {
                        recorder.thumbnail((top.0, top.1, &top.2), (bottom.0, bottom.1, &bottom.2)).expect("thumbnail");
                    }
                    recorder.next_frame((frame * 16).into()).expect("frame");
                    recorder.rom_reads(&reads);
                    if let Some(access) = access.as_ref() {
                        recorder.transient_page_access(access);
                    }
                }
                let Some(state) = state else { continue };
                let t = Instant::now();
                match recorder.as_mut() {
                    None => {
                        let metadata = ReplayFileMetadata { console_type: ReplayConsoleType::Nintendo3DS, rom_name: "lab".into(), rom_filename: "lab".into(), ..Default::default() };
                        let mut r = ReplayFileRecorder::new_with_metadata(metadata, ByteVec::new(), settings.clone(), (frame * 16).into(), ByteVec::Heap(current_input.clone()), Speed::default(), ByteVec::Heap((*state).clone()), sink.take().expect("one recorder"), NullReplayFileSink).expect("recorder");
                        // N3DS_LAB_NO_ROM=1: the same recorder without ROM copies, for comparison.
                        if std::env::var_os("N3DS_LAB_NO_ROM").is_none() {
                            r.set_rom(rom.clone());
                        }
                        recorder = Some(r);
                    },
                    Some(r) => {
                        r.insert_keyframe(ByteVec::Heap((*state).clone()), (frame * 16).into()).expect("keyframe");
                        let ms = t.elapsed().as_secs_f64() * 1000.0;
                        encode_ms += ms;
                        worst_ms = worst_ms.max(ms);
                    }
                }
                keyframes += 1;
            }
            let bytes = recorder.map(|mut r| r.close().map_err(|e| e.2).expect("close").0 .0).unwrap_or(0);
            let _ = result.send((cadence, bytes, keyframes, encode_ms / keyframes.saturating_sub(1).max(1) as f64, worst_ms));
        }));
    }
    drop(result_tx);

    // A second reader of the source file, stepped frame by frame with the core, for the packets
    // to forward (the core consumes its own copy).
    let mut source: Option<ReplayFilePlayer> = (!cadences.is_empty()).then(|| {
        let mut p = ReplayFilePlayer::new_shared(source_bytes.clone(), false).expect("parse");
        p.set_rom(rom.clone());
        p.set_keyframe_states_wanted(false);
        p
    });
    let mut current_input: Vec<u8> = Vec::new();
    // The frame the second reader is at (the file has no getter for it).
    let mut source_frame = 0u64;
    if let Some(p) = source.as_mut() && start > 0 {
        // The same keyframe the core started from; its metadata carries the input in effect.
        p.go_to_keyframe(start).expect("source keyframe");
        if let Ok(Some(Packet::Keyframe { metadata, .. })) = p.next_packet() {
            current_input = metadata.input.to_vec();
        }
        source_frame = start;
    }

    // Straight playback, drawn; the captures are the seeks' ground truth.
    let mut truth: Vec<(u64, u64, (u64, u64))> = Vec::new();
    let t_play = Instant::now();
    let mut frames_run = 0u64;
    let mut reads = Vec::new();
    let mut access_buf = Vec::new();
    let (mut read_ranges, mut read_bytes) = (0usize, 0u64);
    while core.total_frames() < end && !core.is_replay_stalled() {
        // This frame's packets in the source: inputs (in effect for this frame) and pictures.
        let mut inputs = Vec::new();
        let mut thumbnail = None;
        if let Some(p) = source.as_mut() {
            loop {
                match p.next_packet() {
                    Ok(Some(Packet::ChangeInput { data })) => { current_input = data.to_vec(); inputs.push(current_input.clone()); }
                    Ok(Some(Packet::Thumbnail { .. })) => {
                        thumbnail = p.thumbnail_at_or_before(source_frame).map(|t| [t.top, t.bottom]);
                    }
                    Ok(Some(Packet::NextFrame { .. })) => { source_frame += 1; break; }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
        }
        core.run_unlocked();
        frames_run += 1;
        let f = core.total_frames();
        let capture = (f - start) % step == 0;
        if capture {
            truth.push((f, heap_hash(&core), screen_hashes(&core)));
            // N3DS_LAB_PRINT_HASHES=1: the heap and screen hashes of every capture, to compare
            // two playbacks frame by frame.
            if std::env::var_os("N3DS_LAB_PRINT_HASHES").is_some() {
                let (heap, (top, bottom)) = (heap_hash(&core), screen_hashes(&core));
                println!("hash {f} {heap:016x} {top:016x} {bottom:016x}");
            }
        }
        if cadences.is_empty() {
            continue;
        }
        let mut state = Vec::new();
        if capture {
            core.get_core().create_save_state_into(&mut state);
        }
        let state = (!state.is_empty()).then(|| Arc::new(state));
        reads.clear();
        core.get_core_mut().take_rom_reads(&mut reads);
        read_ranges += reads.len();
        read_bytes += reads.iter().map(|r| r.1).sum::<u64>();
        let access = core.get_core_mut().take_transient_page_access(&mut access_buf)
            .map(|(offset, page)| TransientPageAccess { state_offset: offset as u64, page_size: page as u32, first_access: access_buf.clone() });
        for &cadence in &cadences {
            let keyframe = capture && ((f - start) % cadence == 0 || f == start + step);
            senders[&cadence].send(FrameMessage {
                frame: f,
                current_input: current_input.clone(),
                inputs: inputs.clone(),
                thumbnail: thumbnail.clone(),
                state: if keyframe { state.clone() } else { None },
                reads: reads.clone(),
                access: access.clone(),
            }).expect("worker");
        }
        if (f - start) % 36_000 < 1 {
            println!("frame {f} / {end}: {:.0} fps", frames_run as f64 / t_play.elapsed().as_secs_f64());
        }
    }
    let secs = t_play.elapsed().as_secs_f64();
    println!("played {frames_run} frames in {secs:.1} s ({:.0} fps with captures); the game read {read_ranges} ROM ranges, {read_bytes} bytes", frames_run as f64 / secs);
    drop(senders);
    for w in workers {
        w.join().unwrap();
    }
    let hours = frames_run as f64 / 60.0 / 3600.0;
    let mut results: Vec<_> = result_rx.iter().collect();
    results.sort_by_key(|r| r.0);
    for (cadence, bytes, keyframes, avg_ms, worst_ms) in results {
        println!("cadence {cadence:4} frames ({:.0} s): {keyframes} keyframes, {:.1} MB ({:.0} MB/h, {:.2} GB per 3 h), encode {avg_ms:.0} ms avg / {worst_ms:.0} ms worst",
                 cadence as f64 / 60.0, bytes as f64 / 1e6, bytes as f64 / 1e6 / hours, bytes as f64 / 1e9 / hours * 3.0);
    }
    std::io::stdout().flush().ok();

    // Seeks.
    if seeks > 0 && truth.len() > 2 {
        let mut rng = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let (mut ok, mut top_ok, mut bottom_ok, mut drawn_ok) = (0usize, 0usize, 0usize, 0usize);
        let (mut total_ms, mut worst_ms) = (0f64, 0f64);
        let redraws_before = core.replay_seek_redraws();
        for _ in 0..seeks {
            rng ^= rng << 13; rng ^= rng >> 7; rng ^= rng << 17;
            let (target, heap, screens) = truth[(rng % truth.len() as u64) as usize];
            let redraws = core.replay_seek_redraws();
            let t = Instant::now();
            // Shows `target`: total_frames() is then the frame count straight playback had.
            core.go_to_replay_frame(target).expect("seek");
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            assert_eq!(core.total_frames(), target);
            let (same, got) = (heap_hash(&core) == heap, screen_hashes(&core));
            ok += usize::from(same);
            top_ok += usize::from(got.0 == screens.0);
            bottom_ok += usize::from(got.1 == screens.1);
            total_ms += ms;
            worst_ms = worst_ms.max(ms);
            let redrawn = core.replay_seek_redraws() > redraws;
            // The same walk from the same keyframe with every frame drawn: what the seek would
            // show without skipping (straight playback can differ from a loaded keyframe).
            let keyframe = core.replay_keyframe_loaded();
            core.go_to_replay_keyframe(keyframe).expect("keyframe");
            while core.total_frames() < target {
                core.run_unlocked();
            }
            let drawn = screen_hashes(&core);
            let matches_drawn = got == drawn;
            drawn_ok += usize::from(matches_drawn);
            println!("seek {target} (keyframe {keyframe}): {ms:.0} ms{}, heap {}, top {}, bottom {}; same picture as drawing every frame: {}",
                     if redrawn { " (redrawn)" } else { "" },
                     if same { "ok" } else { "DIFFERS" }, if got.0 == screens.0 { "ok" } else { "DIFFERS" }, if got.1 == screens.1 { "ok" } else { "DIFFERS" },
                     if matches_drawn { "yes" } else { "NO" });
        }
        println!("seeks: {seeks}, {:.0} ms average, {worst_ms:.0} ms worst; heap {ok}/{seeks}, top {top_ok}/{seeks}, bottom {bottom_ok}/{seeks} (against straight playback); same picture as drawing the walk {drawn_ok}/{seeks}; {} redone drawn",
                 total_ms / seeks as f64, core.replay_seek_redraws() - redraws_before);
    }

    // Where a seek's time goes: the keyframe load, the frames run hidden, the drawn tail, and
    // (again from the keyframe) the same walk with every frame drawn, in 10-frame chunks.
    if profile > 0 && truth.len() > 2 {
        let mut rng = seed.wrapping_mul(0x2545_F491_4F6C_DD1D) | 1;
        for _ in 0..profile {
            rng ^= rng << 13; rng ^= rng >> 7; rng ^= rng << 17;
            let target = truth[(rng % truth.len() as u64) as usize].0;
            let t = Instant::now();
            let keyframe = core.go_to_replay_keyframe(target.saturating_sub(3)).expect("keyframe");
            let load_ms = t.elapsed().as_secs_f64() * 1000.0;
            let (mut hidden, mut drawn) = (0u64, 0u64);
            let t = Instant::now();
            while core.total_frames() + 3 < target {
                core.run_unlocked_hidden();
                hidden += 1;
            }
            let hidden_ms = t.elapsed().as_secs_f64() * 1000.0;
            let t = Instant::now();
            while core.total_frames() < target {
                core.run_unlocked();
                drawn += 1;
            }
            let drawn_ms = t.elapsed().as_secs_f64() * 1000.0;
            let stale = core.get_core().skipped_draws_left_stale();
            let t = Instant::now();
            core.go_to_replay_keyframe(keyframe).expect("keyframe");
            let reload_ms = t.elapsed().as_secs_f64() * 1000.0;
            let mut chunks = Vec::new();
            let mut t = Instant::now();
            let mut n = 0;
            while core.total_frames() < target {
                core.run_unlocked();
                n += 1;
                if n % 10 == 0 || core.total_frames() == target {
                    chunks.push(format!("{:.1}", t.elapsed().as_secs_f64() * 1000.0 / if n % 10 == 0 { 10.0 } else { (n % 10) as f64 }));
                    t = Instant::now();
                }
            }
            println!("profile {target} (keyframe {keyframe}): load {load_ms:.0} ms, {hidden} hidden {hidden_ms:.0} ms ({:.2} ms/frame), {drawn} drawn {drawn_ms:.0} ms, stale {stale:?}; reload {reload_ms:.0} ms, all drawn ms/frame per 10: {}",
                     hidden_ms / hidden.max(1) as f64, chunks.join(" "));
        }
    }
    println!("done");
}
