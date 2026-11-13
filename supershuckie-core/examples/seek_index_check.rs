//! Checks and times the seek index (`SuperShuckieCore::set_replay_seek_index_budget`) on a real
//! Nintendo DS replay: the same seeks are made by a core without the index and then by one with
//! it, and both must end on the same emulator state and the same picture every time.
//!
//! ```text
//! seek_index_check <rom.nds> <file.replay> [--seeks N] [--from F] [--span N] [--seed S]
//!                  [--prefill WORKERS] [--prefill-seconds S]
//! ```
//!
//! With `--index-dir DIR` the index is read from and written to its file in `DIR` (see
//! `ReplaySeekIndexFile`): run twice to check that the second run walks nothing.
//!
//! With `--prefill`, the core with the index also gets that many background emulators filling
//! it (`ReplaySeekPrefill`), which are given `--prefill-seconds` (default 10; 0 waits until the
//! whole replay is walked) before the seeks start, so the first pass finds snapshots too.
//!
//! Three series over a `--span`-frame stretch starting at `--from`:
//! 1. scrub: random frames in the stretch, visited twice over (the second pass finds the
//!    snapshots the first one left);
//! 2. step back: from a frame, one frame back at a time (what an editor's left arrow does);
//! 3. return: a seek, another one 200 frames on, then back to the first.
//!
//! The two cores run one after the other, not side by side: a DS core and its states take
//! several hundred megabytes.
//!
//! A recording made before states carried the firmware and the divider registers (the `SHUK`
//! section, 2026-10-11) can differ in `NDSG` and `SHUK` by a few bytes: loading one of its
//! keyframes leaves those registers as the core had them, so two seeks to the same frame with
//! different histories differ there with or without the index.
//!
//! Link it like `nds_bench` (see that file's header).

use std::sync::Arc;
use std::time::Instant;

use supershuckie_core::emulator::{EmulatorCore, NintendoDS};
use supershuckie_core::{std_timestamp_provider, ReplaySeekPrefill, ReplaySeekPrefillWorker, SuperShuckieCore};
use supershuckie_replay_recorder::blake3_hash;
use supershuckie_replay_recorder::keyframe_masks::transient_ranges;
use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::replay_file::ReplayConsoleType;

/// melonDS's `GPU3D::DoSavestate` after `PolygonRAM`: the command stall queue (3 x u32 + 64
/// 8-byte entries) and 5 pipeline words, then `RenderNumPolygons` and the 2048-slot render list.
const AFTER_POLYGON_RAM_TO_RENDER_COUNT: usize = 12 + 64 * 8 + 5 * 4;
const RENDER_LIST_LEN: usize = 4 + 2048 * 4;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// What a seek ended on.
#[derive(PartialEq, Clone)]
struct Landing {
    /// Hash of each melonDS section of the state, by name. Left out of the hashes are the 3D
    /// geometry banks, which any load of a keyframe that left them out makes differ from
    /// playback until the game has rewritten every slot, and the render list that points into
    /// them (rebuilt at every flush; its unused slots keep whatever an earlier scene left).
    sections: Vec<(String, [u8; 32])>,
    picture: [u8; 32],
    millis: u64,
    counters: String,
    ms: f64,
}

fn sections(state: &mut [u8]) -> Vec<(String, [u8; 32])> {
    let mut skip = transient_ranges(ReplayConsoleType::NintendoDS, state);
    if let Some(banks) = skip.first().cloned() {
        let list = banks.end + AFTER_POLYGON_RAM_TO_RENDER_COUNT;
        skip.push(list..list + RENDER_LIST_LEN);
    }
    for range in skip {
        if range.end <= state.len() {
            state[range].fill(0);
        }
    }
    let mut out = Vec::new();
    let mut offset = 16usize;
    while offset + 16 <= state.len() {
        let len = u32::from_le_bytes(state[offset + 4..offset + 8].try_into().unwrap()) as usize;
        if len < 16 {
            break;
        }
        let end = (offset + len).min(state.len());
        out.push((String::from_utf8_lossy(&state[offset..offset + 4]).into_owned(), blake3_hash(&state[offset..end])));
        offset = end;
    }
    out
}

/// `--index-dir`: where the index is kept between runs.
static INDEX_DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

fn run(rom: &Arc<Vec<u8>>, bytes: &Arc<Vec<u8>>, index: bool, targets: &[u64], prefill: Option<(usize, f64, u64)>) -> (Vec<Landing>, (usize, usize, u64, u64)) {
    // The file is shared with the player rather than copied.
    let mut player = ReplayFilePlayer::new_shared(bytes.clone(), false).expect("parse replay");
    player.enable_threading();
    if player.wants_rom() {
        player.set_rom(rom.clone());
    }
    let nds = NintendoDS::new_from_rom(rom, None, std_timestamp_provider(), false).expect("load ROM");
    let mut core = SuperShuckieCore::new(Box::new(nds), std_timestamp_provider());
    core.set_replay_seek_index_budget(if index { SuperShuckieCore::DEFAULT_SEEK_INDEX_BUDGET } else { 0 });
    core.attach_replay_player(player, true).expect("attach");

    if let Some((workers, seconds, from)) = prefill {
        let makers: Vec<ReplaySeekPrefillWorker> = (0..workers).map(|_| {
            let (rom, bytes) = (rom.clone(), bytes.clone());
            Box::new(move || {
                let mut player = ReplayFilePlayer::new_shared(bytes, false).map_err(|e| format!("{e:?}"))?;
                player.enable_threading();
                if player.wants_rom() {
                    player.set_rom(rom.clone());
                }
                let nds = NintendoDS::new_from_rom(&rom, None, std_timestamp_provider(), false)?;
                Ok((Box::new(nds) as Box<dyn EmulatorCore>, player))
            }) as ReplaySeekPrefillWorker
        }).collect();
        let file = INDEX_DIR.get().map(|directory| supershuckie_core::ReplaySeekIndexFile::in_directory(directory, bytes, Some(rom), 0));
        if let Some(file) = file.as_ref() {
            println!("index file: {} ({})", file.path.display(), match std::fs::metadata(&file.path) {
                Ok(m) => format!("{:.1} MiB there", m.len() as f64 / 1048576.0),
                Err(_) => "not there yet".to_owned()
            });
        }
        let prefill = ReplaySeekPrefill::start_with_file(makers, file);
        let monitor = prefill.monitor();
        core.set_replay_seek_prefill(Some(prefill));
        // The workers start where the user is.
        core.go_to_replay_frame(from).expect("seek");
        let t = Instant::now();
        loop {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let stats = monitor.stats();
            if (stats.workers_running == 0 && !monitor.loading()) || (seconds > 0.0 && t.elapsed().as_secs_f64() >= seconds) {
                break
            }
        }
        println!("prefill: the whole replay has snapshots: {}", monitor.all_walked());
        let stats = monitor.stats();
        let secs = t.elapsed().as_secs_f64();
        println!(
            "prefill: {workers} workers walked {} frames in {secs:.1} s ({:.0} frames a second in all), {} of {} keyframe intervals; errors: {:?}",
            stats.frames_walked, stats.frames_walked as f64 / secs, stats.intervals_done, stats.intervals_total, monitor.take_errors()
        );
    }

    let mut state = Vec::new();
    let mut landings = Vec::with_capacity(targets.len());
    for &frame in targets {
        let t = Instant::now();
        core.go_to_replay_frame(frame).expect("seek");
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(core.total_frames(), frame, "landed on the wrong frame");
        core.get_core().create_save_state_into(&mut state);
        let mut pixels = Vec::new();
        for screen in core.get_core().get_screens() {
            for pixel in &screen.pixels {
                pixels.extend_from_slice(&pixel.to_le_bytes());
            }
        }
        landings.push(Landing {
            sections: sections(&mut state),
            picture: blake3_hash(&pixels),
            millis: core.get_recording_milliseconds().0,
            counters: format!("{:?}", core.get_replay_counters()),
            ms,
        });
    }
    if prefill.is_some() {
        println!("prefill: {} of the index's snapshots came from the workers", core.replay_seek_index_adopted());
    }
    (landings, core.replay_seek_index_stats())
}

fn main() {
    let mut args = std::env::args().skip(1);
    let rom_path = args.next().expect("usage: seek_index_check <rom.nds> <file.replay> [--seeks N] [--from F] [--span N] [--seed S]");
    let replay_path = args.next().expect("replay");
    let (mut seeks, mut from, mut span, mut seed) = (40usize, 0u64, 3600u64, 0x9E37_79B9_7F4A_7C15u64);
    let (mut prefill_workers, mut prefill_seconds) = (0usize, 10.0f64);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--seeks" => seeks = args.next().unwrap().parse().unwrap(),
            "--from" => from = args.next().unwrap().parse().unwrap(),
            "--span" => span = args.next().unwrap().parse().unwrap(),
            "--seed" => seed = args.next().unwrap().parse().unwrap(),
            "--prefill" => prefill_workers = args.next().unwrap().parse().unwrap(),
            "--prefill-seconds" => prefill_seconds = args.next().unwrap().parse().unwrap(),
            "--index-dir" => INDEX_DIR.set(args.next().unwrap().into()).unwrap(),
            other => panic!("unknown argument {other}"),
        }
    }

    let rom = Arc::new(std::fs::read(&rom_path).expect("read rom"));
    let bytes = Arc::new(std::fs::read(&replay_path).expect("read replay"));
    let total = ReplayFilePlayer::new_shared(bytes.clone(), false).expect("parse replay").get_total_frames();
    let from = from.max(10).min(total.saturating_sub(span + 10));
    println!("{replay_path}: {total} frames; checking frames {from}..{}", from + span);

    // The seeks, in order, and which series each belongs to.
    let mut targets: Vec<u64> = Vec::new();
    let mut series: Vec<(&str, std::ops::Range<usize>)> = Vec::new();
    let mut rng = Rng(seed);
    let scrub: Vec<u64> = (0..seeks).map(|_| from + rng.next() % span).collect();
    for name in ["scrub, first pass", "scrub, second pass"] {
        series.push((name, targets.len()..targets.len() + scrub.len()));
        targets.extend_from_slice(&scrub);
    }
    let start = from + span / 2 + 77;
    targets.push(start);
    series.push(("step back one frame", targets.len()..targets.len() + seeks));
    targets.extend((1..=seeks as u64).map(|back| start - back));
    let returns = (seeks / 4).max(3);
    let mut return_at = Vec::new();
    for i in 0..returns as u64 {
        let start = from + (i * 397) % span.saturating_sub(300).max(1) + 50;
        targets.extend_from_slice(&[start, start + 200, start]);
        return_at.push(targets.len() - 1);
    }

    let (plain, _) = run(&rom, &bytes, false, &targets, None);
    let (indexed, (held, bytes_held, taken, loaded)) = run(&rom, &bytes, true, &targets, (prefill_workers > 0 || INDEX_DIR.get().is_some()).then_some((prefill_workers, prefill_seconds, from)));

    let mut mismatches = 0usize;
    for (i, (a, b)) in indexed.iter().zip(&plain).enumerate() {
        let differing: Vec<&str> = a.sections.iter().zip(&b.sections).filter(|(x, y)| x != y).map(|(x, _)| x.0.as_str()).collect();
        let same_state = differing.is_empty() && a.sections.len() == b.sections.len();
        if !same_state || a.picture != b.picture || a.millis != b.millis || a.counters != b.counters {
            mismatches += 1;
            println!(
                "  MISMATCH at seek {i} (frame {}): state {}, picture {}, clock and counters {}",
                targets[i],
                if same_state { "same".to_owned() } else { format!("DIFFERS in {differing:?}") },
                if a.picture == b.picture { "same" } else { "DIFFERS" },
                if a.millis == b.millis && a.counters == b.counters { "same" } else { "DIFFER" }
            );
        }
    }

    let report = |name: &str, which: &mut dyn Iterator<Item = usize>| {
        let which: Vec<usize> = which.collect();
        let stats = |landings: &[Landing]| {
            let mut ms: Vec<f64> = which.iter().map(|&i| landings[i].ms).collect();
            ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
            (ms.iter().sum::<f64>() / ms.len().max(1) as f64, ms[ms.len() / 2], ms[ms.len() - 1])
        };
        let (with, without) = (stats(&indexed), stats(&plain));
        println!(
            "  {name}: with the index {:.1} ms average, {:.1} median, {:.1} worst; without {:.1} average, {:.1} median, {:.1} worst",
            with.0, with.1, with.2, without.0, without.1, without.2
        );
    };
    for (name, range) in &series {
        report(name, &mut range.clone());
    }
    report("return after 200 frames", &mut return_at.iter().copied());

    println!("index: {held} snapshots held in {:.1} MiB ({:.1} KiB each), {taken} taken, {loaded} loaded", bytes_held as f64 / 1048576.0, bytes_held as f64 / 1024.0 / held.max(1) as f64);
    if mismatches == 0 {
        println!("all {} seeks with the index matched the seeks without it", targets.len());
    }
    else {
        println!("{mismatches} of {} seeks DID NOT MATCH", targets.len());
        std::process::exit(1);
    }
}
