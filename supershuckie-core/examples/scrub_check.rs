//! Timeline-scrub check for Nintendo DS replays: what a seek puts in front of melonDS's 3D
//! renderer, and what the frames shown after a seek look like next to plain playback.
//!
//! Delta keyframes of a masked replay (see `keyframe_masks`) carry the chain restart's copy of
//! the melonDS vertex/polygon banks, while the render list and the pending-polygon count in the
//! same state are live. This tool measures what that means for seeks.
//!
//! ```text
//! scrub_check <rom.nds> <file.replay> [options]
//!
//!   (no option)        scan: read every keyframe (no emulator) and report, per keyframe, the
//!                      render list it restores, how many of its entries point at polygon slots
//!                      with no vertices (the software renderer dereferences a null vertex for
//!                      those), and how many already-submitted polygons the next flush will add
//!   --compare <n>      emulate: for n keyframes spread over the file, compare with plain playback
//!                      (from 60 frames earlier) what a timeline drag shows (a coarse seek to
//!                      keyframe + POST_LOAD_FRAMES, which may settle a few frames further on)
//!                      and what an exact seek to keyframe + POST_LOAD_FRAMES shows (a keyframe
//!                      bookmark's frame), then the frames after it
//!   --scrub <n>        emulate: n back-to-back drag seeks to random keyframes, as dragging the
//!                      timeline issues them
//!   --sweep            emulate: a drag seek to every stale keyframe, in order, reporting how far
//!                      each settled
//!   --only-empty       with --compare/--scrub: only keyframes whose render list points at empty
//!                      slots (the ones that can crash the renderer)
//!   --skip-empty       with --compare/--scrub: leave those keyframes out
//!   --keyframes <list> with --compare/--scrub: these keyframes (comma-separated frames) only
//!   --after <frames>   with --compare: frames compared after the exact seek (default 6)
//!   --lead <frames>    with --compare: how far before the keyframe plain playback starts
//!                      (default 60; a long static 3D scene needs a start before it began)
//! ```
//!
//! Link it like `nds_bench` (see that file's header).

use std::collections::BTreeMap;
use std::time::Instant;

use supershuckie_core::emulator::NintendoDS;
use supershuckie_core::{std_timestamp_provider, SuperShuckieCore};
use supershuckie_replay_recorder::keyframe_masks::transient_ranges;
use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::replay_file::ReplayConsoleType;
use supershuckie_replay_recorder::Packet;

/// `GPU3D::DoSavestate` layout after `VertexRAM` (see `keyframe_masks.rs`, `GPU3D.cpp`).
const VERTEX_RAM_LEN: usize = 12_288 * 64;
const POLYGON_LEN: usize = 188;
const POLYGON_SLOTS: usize = 4_096;
const POLYGON_RAM_LEN: usize = POLYGON_SLOTS * POLYGON_LEN;
/// `CmdStallQueue` (3 x u32 + 64 x 8-byte entries), then 5 pipeline/slot u32s.
const AFTER_POLYGON_RAM_TO_RENDER_COUNT: usize = 12 + 64 * 8 + 5 * 4;
const RENDER_LIST_SLOTS: usize = 2_048;

/// What one keyframe's state restores into the 3D engine.
#[derive(Clone, Debug, Default)]
struct GeometryState {
    /// `RenderNumPolygons`: the list the renderer draws on load and until the next flush.
    render_list: usize,
    /// Render-list entries whose polygon slot has no vertices (null `Vertices[]`).
    render_list_empty: usize,
    /// `NumPolygons`: polygons already submitted to the current bank; the next flush includes them.
    pending: usize,
    /// Pending polygons whose slot has no vertices.
    pending_empty: usize,
    flush_requested: bool,
}

fn u32_at(state: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(state[offset..offset + 4].try_into().expect("4 bytes"))
}

fn geometry_state(state: &[u8]) -> Option<GeometryState> {
    let range = transient_ranges(ReplayConsoleType::NintendoDS, state).into_iter().next()?;
    let vertex_ram = range.start;
    let polygon_ram = vertex_ram + VERTEX_RAM_LEN;
    let render_count_at = polygon_ram + POLYGON_RAM_LEN + AFTER_POLYGON_RAM_TO_RENDER_COUNT;

    let cur_bank = u32_at(state, vertex_ram - 24) as usize;
    let pending = u32_at(state, vertex_ram - 16) as usize;
    let flush_requested = u32_at(state, vertex_ram - 8) != 0;
    let render_list = u32_at(state, render_count_at) as usize;
    if cur_bank > 1 || pending > 2048 || render_list > RENDER_LIST_SLOTS {
        return None
    }

    let polygon_vertices = |slot: usize| u32_at(state, polygon_ram + slot * POLYGON_LEN + 40);

    let mut render_list_empty = 0;
    for i in 0..render_list {
        let slot = u32_at(state, render_count_at + 4 + i * 4) as usize;
        if slot >= POLYGON_SLOTS || polygon_vertices(slot) == 0 {
            render_list_empty += 1;
        }
    }
    let bank_start = if cur_bank == 1 { 2048 } else { 0 };
    let pending_empty = (0..pending).filter(|i| polygon_vertices(bank_start + i) == 0).count();

    Some(GeometryState { render_list, render_list_empty, pending, pending_empty, flush_requested })
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn clock(frame: u64) -> String {
    let s = frame / 60;
    format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

/// Every keyframe's geometry state, plus whether its transient bytes are the previous keyframe's
/// (i.e. masked: the banks are a stale copy).
fn scan(bytes: &[u8]) -> BTreeMap<u64, (GeometryState, bool)> {
    let mut player = ReplayFilePlayer::new(bytes, false).expect("parse replay");
    player.set_keyframe_states_wanted(false);
    let mut out = BTreeMap::new();
    let mut previous_transient: Option<Vec<u8>> = None;
    while let Some(packet) = player.next_packet().expect("read replay") {
        let Packet::Keyframe { metadata, .. } = packet else { continue };
        let frame = metadata.elapsed_frames;
        let state = player.current_keyframe_state();
        let Some(range) = transient_ranges(ReplayConsoleType::NintendoDS, state).into_iter().next() else {
            previous_transient = None;
            continue
        };
        let transient = &state[range];
        let masked = previous_transient.as_deref() == Some(transient);
        if !masked {
            previous_transient = Some(transient.to_vec());
        }
        if let Some(g) = geometry_state(state) {
            out.insert(frame, (g, masked));
        }
    }
    out
}

fn screens(core: &SuperShuckieCore) -> Vec<Vec<u32>> {
    core.get_core().get_screens().iter().map(|s| s.pixels.clone()).collect()
}

fn diff(a: &[Vec<u32>], b: &[Vec<u32>]) -> usize {
    a.iter().zip(b).map(|(x, y)| x.iter().zip(y).filter(|(p, q)| p != q).count()).sum()
}

fn main() {
    let mut args = std::env::args().skip(1);
    let usage = "usage: scrub_check <rom.nds> <file.replay> [--compare n | --scrub n | --sweep] [--only-empty | --skip-empty] [--after frames]";
    let rom_path = args.next().expect(usage);
    let replay_path = args.next().expect(usage);
    let (mut compare, mut scrub, mut sweep, mut only_empty, mut skip_empty, mut after) = (0usize, 0usize, false, false, false, 6u64);
    let mut only: Option<Vec<u64>> = None;
    let mut lead = 60u64;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--lead" => lead = args.next().and_then(|s| s.parse().ok()).expect(usage),
            "--keyframes" => only = Some(args.next().expect(usage).split(',').map(|s| s.trim().parse().expect("keyframe frame")).collect()),
            "--compare" => compare = args.next().and_then(|s| s.parse().ok()).expect(usage),
            "--scrub" => scrub = args.next().and_then(|s| s.parse().ok()).expect(usage),
            "--sweep" => sweep = true,
            "--only-empty" => only_empty = true,
            "--skip-empty" => skip_empty = true,
            "--after" => after = args.next().and_then(|s| s.parse().ok()).expect(usage),
            _ => panic!("{usage}")
        }
    }

    let bytes = std::fs::read(&replay_path).expect("read replay");
    let t = Instant::now();
    let keyframes = scan(&bytes);
    let masked = keyframes.values().filter(|(_, m)| *m).count();
    println!("{replay_path}: {} keyframes ({masked} with stale 3D banks), scanned in {:.1} s", keyframes.len(), t.elapsed().as_secs_f64());

    let stale: Vec<(&u64, &GeometryState)> = keyframes.iter().filter(|(_, (_, m))| *m).map(|(f, (g, _))| (f, g)).collect();
    let crash_on_load: Vec<u64> = stale.iter().filter(|(_, g)| g.render_list_empty > 0).map(|(f, _)| **f).collect();
    let crash_on_flush = stale.iter().filter(|(_, g)| g.pending_empty > 0).count();
    let with_pending = stale.iter().filter(|(_, g)| g.pending > 0).count();
    let flush_requested = stale.iter().filter(|(_, g)| g.flush_requested).count();
    let avg = |f: &dyn Fn(&GeometryState) -> usize| stale.iter().map(|(_, g)| f(g)).sum::<usize>() as f64 / stale.len().max(1) as f64;
    println!("stale keyframes: render list {:.0} polygons on average (all drawn from the stale banks on load)", avg(&|g| g.render_list));
    println!("  {} render lists point at slots with no vertices (the renderer reads a null vertex)", crash_on_load.len());
    println!("  {with_pending} have already-submitted polygons that the next flush adds from the stale banks ({:.0} on average), {crash_on_flush} of them at empty slots", avg(&|g| g.pending));
    println!("  {flush_requested} were taken with a flush pending");
    for f in crash_on_load.iter().take(12) {
        let g = &keyframes[f].0;
        println!("    keyframe {f:>7} ({}): {} of {} render-list entries empty, {} pending ({} empty)", clock(*f), g.render_list_empty, g.render_list, g.pending, g.pending_empty);
    }

    if compare == 0 && scrub == 0 && !sweep {
        return
    }

    let pick: Vec<u64> = stale.iter()
        .filter(|(f, g)| **f >= lead + 180 && (!only_empty || g.render_list_empty > 0) && (!skip_empty || (g.render_list_empty == 0 && g.pending_empty == 0)))
        .filter(|(f, _)| only.as_ref().is_none_or(|list| list.contains(*f)))
        .map(|(f, _)| **f)
        .collect();
    assert!(!pick.is_empty(), "no keyframe matches");

    let rom = std::fs::read(&rom_path).expect("read rom");
    let mut player = ReplayFilePlayer::new(&bytes, false).expect("parse replay");
    player.enable_threading();
    let nds = NintendoDS::new_from_rom(&rom, None, std_timestamp_provider(), false).expect("load ROM");
    let mut core = SuperShuckieCore::new(Box::new(nds), std_timestamp_provider());
    core.attach_replay_player(player, true).expect("attach");
    let post = SuperShuckieCore::POST_LOAD_FRAMES;

    if scrub > 0 {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let t = Instant::now();
        for i in 0..scrub {
            let k = pick[(rng.next() as usize) % pick.len()];
            core.go_to_replay_frame_coarse(k + post).expect("seek");
            println!("  scrub {i:>5}: keyframe {k} + {post}, shown +{}", core.total_frames() - k);
        }
        println!("{scrub} drag seeks done in {:.1} s, no crash", t.elapsed().as_secs_f64());
    }

    if sweep {
        let t = Instant::now();
        let mut settled: BTreeMap<u64, usize> = BTreeMap::new();
        let mut still_stale = Vec::new();
        for (i, (k, _)) in stale.iter().enumerate() {
            let k = **k;
            if k < post {
                continue
            }
            core.go_to_replay_frame_coarse(k + post).expect("seek");
            *settled.entry(core.total_frames() - k).or_default() += 1;
            if core.shows_stale_output() {
                still_stale.push(k);
            }
            if i % 1000 == 0 {
                println!("  {i} of {} ({:.0} s)", stale.len(), t.elapsed().as_secs_f64());
            }
        }
        println!("drag seeks to {} stale keyframes in {:.1} s, no crash; frame shown (keyframe + n):", stale.len(), t.elapsed().as_secs_f64());
        for (n, count) in settled {
            println!("  +{n}: {count}");
        }
        println!("  still missing geometry after {} frames: {} {:?}", SuperShuckieCore::MAX_COARSE_SETTLE_FRAMES, still_stale.len(), &still_stale[..still_stale.len().min(20)]);
    }

    if compare > 0 {
        let settle = SuperShuckieCore::MAX_COARSE_SETTLE_FRAMES;
        let step = (pick.len() / compare).max(1);
        let (mut drag_dirty, mut exact_dirty, mut exact_later_dirty, mut fell_back) = (0usize, 0usize, 0usize, 0usize);
        let mut drag_landed: BTreeMap<u64, usize> = BTreeMap::new();
        let mut tested = 0;
        for k in pick.iter().step_by(step).take(compare).copied() {
            // Reference: plain playback through k, from `lead` frames earlier (itself a seek, but
            // to a keyframe far enough back that nothing stale is left by k).
            core.go_to_replay_frame(k - lead).expect("reference seek");
            let mut reference = BTreeMap::new();
            while core.total_frames() < k + post + after.max(settle) {
                core.run_unlocked();
                if core.total_frames() >= k + post {
                    reference.insert(core.total_frames(), screens(&core));
                }
            }

            // A timeline drag over k: the coarse seek, wherever it settles.
            core.go_to_replay_frame_coarse(k + post).expect("coarse seek");
            let landed = core.total_frames();
            let drag = diff(&screens(&core), &reference[&landed]);
            *drag_landed.entry(landed - k).or_default() += 1;

            // An exact seek to k + POST_LOAD_FRAMES (a keyframe bookmark, the frame server), then
            // the frames after it.
            core.go_to_replay_frame(k + post).expect("seek");
            let from = core.replay_keyframe_loaded();
            let mut diffs = vec![diff(&screens(&core), &reference[&(k + post)])];
            while core.total_frames() < k + post + after {
                core.run_unlocked();
                diffs.push(diff(&screens(&core), &reference[&core.total_frames()]));
            }

            let g = &keyframes[&k].0;
            drag_dirty += usize::from(drag != 0);
            exact_dirty += usize::from(diffs[0] != 0);
            exact_later_dirty += usize::from(diffs[1..].iter().any(|d| *d != 0));
            fell_back += usize::from(from != k);
            println!("  keyframe {k:>7} ({}) list {:>4} pending {:>4} empty {:>3}/{:<3}: drag shows +{} ({drag} px off); exact seek from {}: {:?} px off",
                clock(k), g.render_list, g.pending, g.render_list_empty, g.pending_empty, landed - k,
                if from == k { "it" } else { "the keyframe before" }, diffs);
            tested += 1;
        }
        println!("{tested} keyframes: the drag picture differs from playback in {drag_dirty}; the exact seek's in {exact_dirty} (it fell back to the keyframe before in {fell_back}), a later frame in {exact_later_dirty}");
        for (n, count) in drag_landed {
            println!("  drag shows keyframe + {n}: {count}");
        }
    }
}
