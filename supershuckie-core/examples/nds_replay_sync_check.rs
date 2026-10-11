//! Plays a Nintendo DS replay through `SuperShuckieCore`, as the app does, and compares the
//! console with every recorded keyframe on the way: checks that playback stays in sync whatever
//! the console did before the replay was opened.
//!
//! ```text
//! nds_replay_sync_check <rom.nds> <file.replay> [options]
//!
//!   --boot <n>        run the game's own boot for n frames before opening the replay (a console
//!                     that has been running, whose firmware then holds the WFC ID the game
//!                     wrote); default: a freshly created console, as right after loading the ROM
//!   --from <frame>    seek there right after opening the replay (as a timeline click does)
//!   --to <frame>      stop there (default: the end)
//!   --auto-resync     play with the auto-resync option on
//!   --quiet           print only mismatches other than `NDSG:6` and the summary
//!   --report <n>      describe the first n mismatching keyframes byte by byte
//! ```
//!
//! Each mismatching keyframe gets a line with the differing sections and byte counts. `NDSG:6`
//! alone, after a console reset, is the game's RAM copy of a WFC ID different from the
//! recording's (HeartGold): harmless. Hidden-state resyncs the core makes are listed as they
//! happen.

use supershuckie_core::emulator::{EmulatorCore, NintendoDS};
use supershuckie_core::{std_timestamp_provider, SuperShuckieCore};
use supershuckie_replay_recorder::keyframe_masks::transient_ranges;
use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::replay_file::ReplayConsoleType;
use supershuckie_replay_recorder::Packet;

struct Options {
    rom: String,
    replay: String,
    boot: u64,
    from: Option<u64>,
    to: u64,
    auto_resync: bool,
    quiet: bool,
    report: u64,
}

fn parse_args() -> Options {
    let mut args = std::env::args().skip(1);
    let usage = "usage: nds_replay_sync_check <rom.nds> <file.replay> [--boot n] [--from frame] [--to frame] [--auto-resync] [--quiet]";
    let rom = args.next().expect(usage);
    let replay = args.next().expect(usage);
    let mut o = Options { rom, replay, boot: 0, from: None, to: u64::MAX, auto_resync: false, quiet: false, report: 0 };
    while let Some(a) = args.next() {
        let mut value = || args.next().unwrap_or_else(|| panic!("{a} needs a value"));
        match a.as_str() {
            "--boot" => o.boot = value().parse().expect("--boot"),
            "--from" => o.from = Some(value().parse().expect("--from")),
            "--to" => o.to = value().parse().expect("--to"),
            "--auto-resync" => o.auto_resync = true,
            "--quiet" => o.quiet = true,
            "--report" => o.report = value().parse().expect("--report"),
            other => panic!("unknown option {other}\n{usage}"),
        }
    }
    o
}

fn main() {
    let mut o = parse_args();
    let rom = std::fs::read(&o.rom).expect("read rom");
    let bytes = std::fs::read(&o.replay).expect("read replay");

    let mut nds = NintendoDS::new_from_rom(&rom, None, std_timestamp_provider(), false).expect("load rom");
    for _ in 0..o.boot {
        nds.set_skip_drawing(true);
        nds.run_unlocked();
    }
    let mut core = SuperShuckieCore::new(Box::new(nds), std_timestamp_provider());
    core.set_auto_resync_keyframes_in_replays(o.auto_resync);
    let player = ReplayFilePlayer::new(&bytes, false).expect("parse replay");
    let total = player.get_total_frames();
    core.attach_replay_player(player, true).expect("attach");
    if let Some(from) = o.from {
        core.go_to_replay_frame(from).expect("seek");
    }
    let start = core.total_frames();
    println!(
        "{}: {total} frames; console {}, playing from frame {start}{}",
        o.replay,
        if o.boot > 0 { format!("booted {} frames first", o.boot) } else { "fresh".to_string() },
        if o.auto_resync { ", auto-resync on" } else { "" }
    );

    // A second reader over the same file hands out the recorded keyframes to compare with.
    let mut reference = ReplayFilePlayer::new(&bytes, false).expect("parse replay");
    reference.set_keyframe_states_wanted(false);
    let first = reference.all_keyframes().range(start..).next().map(|(f, _)| *f);
    let Some(first) = first else {
        println!("no keyframe after frame {start}");
        return;
    };
    reference.go_to_keyframe(first).expect("reference seek");

    let mut checked = 0u64;
    let mut benign = 0u64;
    let mut bad = 0u64;
    let mut first_bad = None;
    let mut resyncs = core.replay_hidden_state_resyncs();
    'outer: loop {
        // The next recorded keyframe.
        let (frame, recorded) = loop {
            match reference.next_packet() {
                Ok(Some(Packet::Keyframe { metadata, .. })) => break (metadata.elapsed_frames, reference.current_keyframe_state().to_vec()),
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break 'outer,
            }
        };
        if frame > o.to {
            break;
        }
        while core.total_frames() < frame && !core.is_replay_stalled() {
            core.run_unlocked_hidden();
            let now = core.replay_hidden_state_resyncs();
            if now != resyncs {
                // The core reacts to a keyframe on the run after reaching it.
                println!("  hidden-state resync at keyframe {} (total {now})", core.total_frames().saturating_sub(1));
                resyncs = now;
            }
        }
        if core.total_frames() != frame {
            println!("stopped at frame {} before keyframe {frame}", core.total_frames());
            break;
        }
        let live = core.create_save_state();
        let diff = compare(&live, &recorded);
        checked += 1;
        if diff.is_empty() {
            continue;
        }
        let summary: Vec<String> = diff.iter().map(|(s, n)| format!("{s}:{n}")).collect();
        let harmless = diff.len() == 1 && diff[0].0 == "NDSG" && diff[0].1 <= 6;
        if harmless {
            benign += 1;
        } else {
            bad += 1;
            first_bad.get_or_insert(frame);
        }
        if !harmless || !o.quiet {
            println!("kf {frame} {} [{}]", if harmless { "differs" } else { "BAD" }, summary.join(" "));
        }
        if o.report > 0 {
            o.report -= 1;
            for line in describe(&live, &recorded) {
                println!("    {line}");
            }
        }
    }
    let now = core.replay_hidden_state_resyncs();
    println!(
        "done at frame {}: {checked} keyframes checked, {bad} mismatched{}, {benign} only in the WFC ID copy; {} hidden-state resyncs, {} failed writes",
        core.total_frames(),
        first_bad.map(|f| format!(" (first at {f})")).unwrap_or_default(),
        now,
        core.replay_write_failures()
    );
}

/// Differing sections of two states (name, differing bytes), after neutralising what is not game
/// state: the regenerated 3D buffers masked keyframes leave stale and the GX FIFOs' ring phase
/// (as nds_bench's `states_match`). A keyframe from before states carried SHUK is compared on the
/// sections it has.
fn compare(live: &[u8], recorded: &[u8]) -> Vec<(String, usize)> {
    let (live, recorded) = common_parts(live, recorded);
    if live.len() != recorded.len() {
        return vec![("LENGTH".into(), live.len().abs_diff(recorded.len()))];
    }
    let mut a = live.to_vec();
    let mut b = recorded.to_vec();
    for r in transient_ranges(ReplayConsoleType::NintendoDS, recorded) {
        a[r.clone()].fill(0);
        b[r].fill(0);
    }
    if let Some(gp3d) = find_section(recorded, b"GP3D") {
        for (offset, count) in [(16usize, 256usize), (16 + 2060, 4), (1_563_889, 64)] {
            canonicalize_fifo(&mut a, gp3d + offset, count);
            canonicalize_fifo(&mut b, gp3d + offset, count);
        }
    }
    let mut out = Vec::new();
    let mut offset = 16usize;
    while offset + 16 <= a.len() {
        let len = u32::from_le_bytes([a[offset + 4], a[offset + 5], a[offset + 6], a[offset + 7]]) as usize;
        if len < 16 || offset + len > a.len() {
            break;
        }
        let differing = (offset..offset + len).filter(|&i| a[i] != b[i]).count();
        if differing > 0 {
            out.push((String::from_utf8_lossy(&a[offset..offset + 4]).into_owned(), differing));
        }
        offset += len;
    }
    out
}

/// Byte-level detail of what `compare` found: main RAM as address ranges with both values, other
/// sections as offsets.
fn describe(live: &[u8], recorded: &[u8]) -> Vec<String> {
    let (live, recorded) = common_parts(live, recorded);
    let mut out = Vec::new();
    if live.len() != recorded.len() {
        return out;
    }
    let mut offset = 16usize;
    while offset + 16 <= live.len() {
        let len = u32::from_le_bytes([live[offset + 4], live[offset + 5], live[offset + 6], live[offset + 7]]) as usize;
        if len < 16 || offset + len > live.len() {
            break;
        }
        let magic = String::from_utf8_lossy(&live[offset..offset + 4]).into_owned();
        let differing: Vec<usize> = (0..len).filter(|&i| live[offset + i] != recorded[offset + i]).collect();
        if !differing.is_empty() && magic != "GP3D" {
            let mut ranges: Vec<(usize, usize)> = Vec::new();
            for &i in &differing {
                match ranges.last_mut() {
                    Some((_, end)) if i <= *end + 4 => *end = i,
                    _ => ranges.push((i, i)),
                }
            }
            let parts: Vec<String> = ranges.iter().take(16).map(|&(s, e)| {
                let a: String = live[offset + s..=offset + e].iter().map(|b| format!("{b:02X}")).collect();
                let b: String = recorded[offset + s..=offset + e].iter().map(|b| format!("{b:02X}")).collect();
                if magic == "NDSG" && (20..20 + 0x100_0000).contains(&s) {
                    format!("{:08X}..{:08X} live {a} rec {b}", 0x0200_0000 + s - 20, 0x0200_0000 + e - 20)
                } else {
                    format!("+{s}..{e} live {a} rec {b}")
                }
            }).collect();
            out.push(format!("{magic}: {}", parts.join("; ")));
        }
        offset += len;
    }
    out
}

fn canonicalize_fifo(state: &mut [u8], at: usize, count: usize) {
    let end = at + 12 + 8 * count;
    if end > state.len() {
        return;
    }
    let u32_at = |s: &[u8], o: usize| u32::from_le_bytes([s[o], s[o + 1], s[o + 2], s[o + 3]]) as usize;
    let occupied = u32_at(state, at).min(count);
    let read = u32_at(state, at + 4) % count.max(1);
    let entries = at + 12;
    let mut canonical = vec![0u8; 8 * count];
    for i in 0..occupied {
        let src = entries + 8 * ((read + i) % count);
        canonical[8 * i..8 * i + 5].copy_from_slice(&state[src..src + 5]);
    }
    state[at + 4..at + 12].fill(0);
    state[entries..end].copy_from_slice(&canonical);
}

/// Where the SHUK section Super Shuckie appends to every state starts, when `state` ends with one.
fn extra_section_start(state: &[u8]) -> Option<usize> {
    find_section(state, b"SHUK").filter(|&at| {
        let len = u32::from_le_bytes([state[at + 4], state[at + 5], state[at + 6], state[at + 7]]) as usize;
        at + len == state.len()
    })
}

fn common_parts<'a>(a: &'a [u8], b: &'a [u8]) -> (&'a [u8], &'a [u8]) {
    match (extra_section_start(a), extra_section_start(b)) {
        (Some(x), None) => (&a[..x], b),
        (None, Some(y)) => (a, &b[..y]),
        _ => (a, b),
    }
}

fn find_section(state: &[u8], magic: &[u8; 4]) -> Option<usize> {
    let mut offset = 16usize;
    while offset + 16 <= state.len() {
        let len = u32::from_le_bytes([state[offset + 4], state[offset + 5], state[offset + 6], state[offset + 7]]) as usize;
        if len < 16 || offset + len > state.len() {
            return None;
        }
        if &state[offset..offset + 4] == magic {
            return Some(offset);
        }
        offset += len;
    }
    None
}
