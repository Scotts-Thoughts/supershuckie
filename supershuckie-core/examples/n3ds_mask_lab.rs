//! Can a Nintendo 3DS keyframe leave VRAM out? Load a recorded keyframe with its VRAM replaced
//! (by an older keyframe's VRAM, or zeros), play the recorded inputs forward, and compare with
//! the same stretch played from the exact keyframe: game memory at the end (must be identical for
//! the timeline to stay exact) and both screens frame by frame (how many frames until the picture
//! is the real one). Research tool for the replay-size work.
//!
//! ```text
//! n3ds_mask_lab <game file> <file.replay> [--user-dir dir] [--samples N] [--seed S] [--frames 240]
//!               [--source prev|l1|zero] [--range vram|hot|hot2|hot3|control] [--hidden]
//! ```
//! `--source prev`: VRAM from the keyframe before (4 s stale); `l1`: from the latest level-0/1
//! keyframe before it (up to 2 min stale); `zero`: zeros. `--range hot` replaces only the VRAM
//! pages that differ between the two keyframes' VRAM (what a delta would have carried); `hot2`
//! / `hot3` only pages that also changed in the one / two intervals before (per-frame buffers,
//! not something rendered once); `control` replaces nothing (host nondeterminism baseline).
//! `--hidden`: run all but the last 3 frames with drawing skipped, as a seek does.

use std::sync::Arc;
use std::time::Instant;

use supershuckie_core::emulator::{Nintendo3DS, Nintendo3DSSettings};
use supershuckie_core::{std_timestamp_provider, SuperShuckieCore};
use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::replay_file::RomBytes;
use supershuckie_replay_recorder::Packet;

const HEADER: usize = 32;
const VRAM: usize = 6 << 20;
const DSP: usize = 512 << 10;

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

/// Raw-state layout: (fcram_len, vram range, dsp range, archive start).
fn layout(state_len: usize) -> (usize, (usize, usize), (usize, usize), usize) {
    let n3ds = state_len > 200 << 20;
    let fcram = if n3ds { 256 << 20 } else { 128 << 20 };
    let vram = (HEADER + fcram, HEADER + fcram + VRAM);
    let extra = if n3ds { 4 << 20 } else { 0 };
    let dsp = (vram.1 + extra, vram.1 + extra + DSP);
    (fcram, vram, dsp, dsp.1)
}

fn materialise(player: &mut ReplayFilePlayer, frame: u64) -> Vec<u8> {
    player.go_to_keyframe(frame).expect("seek");
    match player.next_packet() {
        Ok(Some(Packet::Keyframe { .. })) => {}
        other => panic!("expected a keyframe at {frame}, got {other:?}"),
    }
    player.current_keyframe_state().to_vec()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Count differing bytes of two equal-length slices.
fn differing(a: &[u8], b: &[u8]) -> usize {
    a.chunks(8).zip(b.chunks(8)).filter(|(x, y)| x != y).map(|(x, y)| x.iter().zip(y.iter()).filter(|(p, q)| p != q).count()).sum()
}

fn main() {
    let mut args = std::env::args().skip(1);
    let game_path = args.next().expect("usage: n3ds_mask_lab <game> <replay> ...");
    let replay_path = args.next().expect("replay");
    let mut user_dir = std::env::temp_dir().join("supershuckie-n3ds-mask-lab").join("user");
    let (mut samples, mut seed, mut frames, mut source, mut range, mut hidden) = (20usize, 1u64, 240u64, "prev".to_owned(), "vram".to_owned(), false);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--user-dir" => user_dir = args.next().unwrap().into(),
            "--samples" => samples = args.next().unwrap().parse().unwrap(),
            "--seed" => seed = args.next().unwrap().parse().unwrap(),
            "--frames" => frames = args.next().unwrap().parse().unwrap(),
            "--source" => source = args.next().unwrap(),
            "--range" => range = args.next().unwrap(),
            "--hidden" => hidden = true,
            o => panic!("unknown option {o}")
        }
    }
    std::fs::create_dir_all(&user_dir).unwrap();

    let rom = map(&game_path);
    let n3ds = Nintendo3DS::new_from_path(&game_path, &[], user_dir.to_str().unwrap(), std_timestamp_provider(), &Nintendo3DSSettings::default()).expect("load game");
    let mut core = SuperShuckieCore::new(Box::new(n3ds), std_timestamp_provider());
    core.set_auto_resync_keyframes_in_replays(false);

    let source_bytes: Arc<dyn AsRef<[u8]> + Send + Sync> = map(&replay_path);
    let mut player = ReplayFilePlayer::new_shared(source_bytes.clone(), false).expect("parse");
    player.set_rom(rom.clone());
    let total = player.get_total_frames();
    // Stored keyframes: (frame, level).
    let kfs: Vec<(u64, u8)> = player.all_uncompressed_packets().iter().filter_map(|p| match p {
        Packet::StoredKeyframe { metadata, level, .. } => Some((metadata.elapsed_frames, *level)),
        _ => None
    }).collect();
    println!("replay: version {}, {total} frames, {} keyframes", player.get_replay_version(), kfs.len());
    core.attach_replay_player(player, true).expect("attach");

    // A second player materialises states for the lab.
    let mut lab_player = ReplayFilePlayer::new_shared(source_bytes, false).expect("parse");
    lab_player.set_rom(rom.clone());
    lab_player.set_keyframe_states_wanted(false);
    lab_player.set_materialise_every_keyframe(true);

    let candidates: Vec<usize> = (3..kfs.len()).filter(|&i| kfs[i].1 == 2 && kfs[i].0 + frames < total).collect();
    let mut rng = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut next = || { rng ^= rng << 13; rng ^= rng >> 7; rng ^= rng << 17; rng };
    let mut picked: Vec<usize> = (0..samples).map(|_| candidates[(next() % candidates.len() as u64) as usize]).collect();
    picked.sort();
    picked.dedup();

    let (mut heap_ok, mut outside_ok, mut converged_within, mut never) = (0usize, 0usize, vec![0usize; 8], 0usize);
    let (mut worst_converge, mut total_ms) = (0usize, 0f64);
    for &i in &picked {
        let (k, _) = kfs[i];
        let src_frame = match source.as_str() {
            "prev" => Some(kfs[i - 1].0),
            "l1" => kfs[..i].iter().rev().find(|(_, l)| *l <= 1).map(|(f, _)| *f),
            "zero" => None,
            o => panic!("unknown source {o}")
        };

        // Truth: the exact keyframe, played forward.
        let loaded = core.go_to_replay_keyframe(k).expect("keyframe");
        assert_eq!(loaded, k);
        let mut truth_screens = Vec::with_capacity(frames as usize);
        for f in 0..frames {
            if hidden && f + 3 < frames { core.run_unlocked_hidden(); } else { core.run_unlocked(); }
            truth_screens.push(screen_hashes(&core));
        }
        let truth_heap = heap_hash(&core);
        let mut truth_state = Vec::new();
        core.get_core().create_save_state_into(&mut truth_state);

        // The masked keyframe.
        let mut state = materialise(&mut lab_player, k);
        let (_, vram, dsp, arch) = layout(state.len());
        let src = src_frame.map(|f| materialise(&mut lab_player, f));
        let mut replaced_pages = 0usize;
        // States of the keyframes before the source, for hot2/hot3.
        let older: Vec<Vec<u8>> = match range.as_str() {
            "hot2" => vec![materialise(&mut lab_player, kfs[i - 2].0)],
            "hot3" => vec![materialise(&mut lab_player, kfs[i - 2].0), materialise(&mut lab_player, kfs[i - 3].0)],
            _ => Vec::new(),
        };
        match range.as_str() {
            "control" => {}
            "hot2" | "hot3" => {
                // Pages that changed in this interval and in each earlier one: the previous keyframe
                // (the source, kfs[i-1]) vs kfs[i-2], and kfs[i-2] vs kfs[i-3].
                let s = src.as_ref().expect("needs a source");
                let mut chain: Vec<&Vec<u8>> = vec![s];
                chain.extend(older.iter());
                for p in (vram.0..vram.1).step_by(4096) {
                    let changed_now = state[p..p + 4096] != s[p..p + 4096];
                    let changed_before = chain.windows(2).all(|w| w[0][p..p + 4096] != w[1][p..p + 4096]);
                    if changed_now && changed_before {
                        state[p..p + 4096].copy_from_slice(&s[p..p + 4096]);
                        replaced_pages += 1;
                    }
                }
            }
            "vram" => {
                match &src {
                    Some(s) => state[vram.0..vram.1].copy_from_slice(&s[vram.0..vram.1]),
                    None => state[vram.0..vram.1].fill(0),
                }
                replaced_pages = VRAM / 4096;
            }
            "hot" => {
                // Only pages that differ between the keyframe and its source (a delta's content).
                let s = src.as_ref().expect("--range hot needs a source");
                for p in (vram.0..vram.1).step_by(4096) {
                    if state[p..p + 4096] != s[p..p + 4096] {
                        state[p..p + 4096].copy_from_slice(&s[p..p + 4096]);
                        replaced_pages += 1;
                    }
                }
            }
            o => panic!("unknown range {o}")
        }
        let t = Instant::now();
        core.go_to_replay_keyframe(k).expect("keyframe");
        core.get_core_mut().load_save_state(&state).expect("load masked state");
        let mut mismatches = Vec::new();
        for f in 0..frames {
            if hidden && f + 3 < frames { core.run_unlocked_hidden(); } else { core.run_unlocked(); }
            let got = screen_hashes(&core);
            let want = truth_screens[f as usize];
            if got != want {
                mismatches.push((f, got.0 != want.0, got.1 != want.1));
            }
        }
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        total_ms += ms;
        let heap_same = heap_hash(&core) == truth_heap;
        let mut masked_state = Vec::new();
        core.get_core().create_save_state_into(&mut masked_state);
        let (fcram_diff, vram_diff, dsp_diff, arch_diff, len_same) = if masked_state.len() == truth_state.len() && !truth_state.is_empty() {
            (differing(&masked_state[HEADER..vram.0], &truth_state[HEADER..vram.0]),
             differing(&masked_state[vram.0..vram.1], &truth_state[vram.0..vram.1]),
             differing(&masked_state[dsp.0..dsp.1], &truth_state[dsp.0..dsp.1]),
             differing(&masked_state[arch..], &truth_state[arch..]), true)
        } else { (0, 0, 0, 0, false) };
        let outside_same = len_same && fcram_diff == 0 && dsp_diff == 0 && arch_diff == 0;
        if len_same && (arch_diff > 0 || dsp_diff > 0) {
            // Where in the archive / DSP RAM the bytes differ (offsets relative to each part).
            let mut spots = Vec::new();
            for (name, a, b) in [("archive", &masked_state[arch..], &truth_state[arch..]), ("dsp", &masked_state[dsp.0..dsp.1], &truth_state[dsp.0..dsp.1])] {
                for (ci, (x, y)) in a.chunks(8).zip(b.chunks(8)).enumerate() {
                    if x != y && spots.len() < 12 {
                        spots.push(format!("{name}+{} ({} -> {})", ci * 8, hex(y), hex(x)));
                    }
                }
            }
            println!("    differing bytes: {}", spots.join("; "));
        }
        let states_note = if truth_state.is_empty() || masked_state.is_empty() { " [a state save was refused: bytes not compared]" } else { "" };
        heap_ok += usize::from(heap_same);
        outside_ok += usize::from(outside_same);
        // Converged: the frame after the last mismatch (frames are 0-based, so +1 = frames needed).
        let converged = mismatches.last().map(|m| m.0 as usize + 1);
        match converged {
            None => converged_within[0] += 1,
            Some(n) if n < frames as usize => { converged_within[(n.min(7)) as usize] += 1; worst_converge = worst_converge.max(n); }
            Some(_) => never += 1,
        }
        let hidden_note = if hidden { " (hidden walk)" } else { "" };
        println!("keyframe {k} (#{i}), VRAM from {}, {replaced_pages} pages replaced{hidden_note}: heap {}, outside VRAM {}{states_note} (fcram {fcram_diff} B, dsp {dsp_diff} B, archive {arch_diff} B, vram {vram_diff} B), picture mismatches {} frames{}, {ms:.0} ms",
                 src_frame.map_or("zeros".to_owned(), |f| format!("{f} ({} s older)", (k - f) / 60)),
                 if heap_same { "ok" } else { "DIFFERS" }, if outside_same { "ok" } else { "DIFFERS" },
                 mismatches.len(),
                 if mismatches.is_empty() { String::new() } else {
                     format!(" (first {} frames: {}; last mismatch at frame {}{})", mismatches.len().min(6),
                             mismatches.iter().take(6).map(|(f, t, b)| format!("{f}{}{}", if *t { "T" } else { "" }, if *b { "B" } else { "" })).collect::<Vec<_>>().join(","),
                             mismatches.last().unwrap().0, if mismatches.last().unwrap().0 + 1 == frames { " = STILL WRONG AT THE END" } else { "" })
                 });
    }
    let n = picked.len();
    println!("\n{n} keyframes, source {source}, range {range}{}: heap identical {heap_ok}/{n}, memory outside VRAM identical {outside_ok}/{n}; picture exact from the first frame {}, within 2 frames {}, within 4 {}, within 7 {}, later {}, still wrong at the end {never}; worst {worst_converge} frames; {:.0} ms per masked run",
             if hidden { " (hidden walk)" } else { "" },
             converged_within[0], converged_within[0] + converged_within[1] + converged_within[2],
             converged_within[..5].iter().sum::<usize>(), converged_within.iter().sum::<usize>(), n - converged_within.iter().sum::<usize>() - never,
             total_ms / n.max(1) as f64);
}
