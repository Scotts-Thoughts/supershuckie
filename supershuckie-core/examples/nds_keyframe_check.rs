//! Walks the keyframes of a Nintendo DS replay and checks the save state the player hands out for
//! each: the melonDS header's length against the state's, the section chain, the SHUK section;
//! with a ROM, also loads each into a console the way the app does. A "cannot load the save state"
//! hunting aid.
//!
//! ```text
//! nds_keyframe_check <file.replay> [rom.nds] [--from frame] [--to frame] [--all]
//! ```
//!
//! Prints the first keyframe's layout, then one line per keyframe whose header, section layout or
//! load differs from what came before (every keyframe with --all).

use supershuckie_core::emulator::{EmulatorCore, NintendoDS};
use supershuckie_core::std_timestamp_provider;
use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::Packet;

fn u32_at(s: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([s[o], s[o + 1], s[o + 2], s[o + 3]])
}

/// The state's sections as (name, offset, length), or what is wrong with its layout.
fn layout(state: &[u8]) -> Result<Vec<(String, usize, usize)>, String> {
    if state.len() < 16 {
        return Err(format!("only {} bytes", state.len()));
    }
    if &state[0..4] != b"MELN" {
        return Err(format!("magic {:02X?}", &state[0..4]));
    }
    let mut out = Vec::new();
    let mut offset = 16usize;
    while offset + 16 <= state.len() {
        let len = u32_at(state, offset + 4) as usize;
        let name = String::from_utf8_lossy(&state[offset..offset + 4]).into_owned();
        if len < 16 || offset + len > state.len() {
            return Err(format!("section {name:?} at {offset:#x} claims {len:#x} bytes; state is {:#x}", state.len()));
        }
        out.push((name, offset, len));
        offset += len;
    }
    if offset != state.len() {
        return Err(format!("sections end at {offset:#x}; state is {:#x}", state.len()));
    }
    Ok(out)
}

fn describe_header(state: &[u8]) -> String {
    if state.len() < 16 {
        return format!("{} bytes", state.len());
    }
    format!(
        "{:?} v{}.{} length field {:#x} (state {:#x}) reserved {:#x}",
        String::from_utf8_lossy(&state[0..4]),
        u16::from_le_bytes([state[4], state[5]]),
        u16::from_le_bytes([state[6], state[7]]),
        u32_at(state, 8),
        state.len(),
        u32_at(state, 12)
    )
}

fn main() {
    let mut args = std::env::args().skip(1);
    let usage = "usage: nds_keyframe_check <file.replay> [rom.nds] [--from frame] [--to frame] [--all]";
    let replay = args.next().expect(usage);
    let mut rom_path = None;
    let (mut from, mut to, mut all) = (0u64, u64::MAX, false);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--from" => from = args.next().expect("--from").parse().expect("--from"),
            "--to" => to = args.next().expect("--to").parse().expect("--to"),
            "--all" => all = true,
            other if rom_path.is_none() && !other.starts_with("--") => rom_path = Some(other.to_string()),
            other => panic!("unknown option {other}\n{usage}"),
        }
    }

    let bytes = std::fs::read(&replay).expect("read replay");
    let mut player = ReplayFilePlayer::new(&bytes, false).expect("parse replay");
    player.set_keyframe_states_wanted(true);

    if let Some(spec) = std::env::var_os("GEOMETRY") {
        // GEOMETRY=a,b,...: the 3D render list each of these keyframes restores (scrub_check's
        // layout): RenderNumPolygons and its first entries' polygon slots (-1: none).
        player.set_keyframe_states_wanted(false);
        for frame in spec.to_string_lossy().split(',').map(|f| f.parse::<u64>().expect("frame")) {
            player.go_to_keyframe(frame).expect("seek");
            let _ = player.next_packet();
            let state = player.current_keyframe_state();
            let range = supershuckie_replay_recorder::keyframe_masks::transient_ranges(
                supershuckie_replay_recorder::replay_file::ReplayConsoleType::NintendoDS, state).into_iter().next().expect("GP3D");
            let vertex_ram = range.start;
            let polygon_ram = vertex_ram + 12_288 * 64;
            let render_count_at = polygon_ram + 4_096 * 188 + 12 + 64 * 8 + 5 * 4;
            let count = u32_at(state, render_count_at) as usize;
            let entries: Vec<i64> = (0..2048).map(|i| {
                let v = u32_at(state, render_count_at + 4 + 4 * i);
                if v == u32::MAX { -1 } else { v as i64 }
            }).collect();
            let null_within = entries[..count.min(2048)].iter().filter(|&&e| e < 0).count();
            println!(
                "kf {frame}: stale {} RenderNumPolygons {count}, {null_within} null within it; bank {} pending {} flush {}; first entries {:?}",
                player.current_keyframe_has_stale_transients(),
                u32_at(state, vertex_ram - 24), u32_at(state, vertex_ram - 16), u32_at(state, vertex_ram - 8),
                &entries[..12]
            );
        }
        return;
    }

    if let Some(spec) = std::env::var_os("REVISIT") {
        // REVISIT=a,b: materialise keyframe a, play on past keyframe b, seek back to a, and compare
        // the two materialisations of a (what a seek back after playing hands the emulator).
        let spec = spec.to_string_lossy().into_owned();
        let (a, b) = spec.split_once(',').expect("REVISIT=a,b");
        let (a, b): (u64, u64) = (a.parse().expect("a"), b.parse().expect("b"));
        let materialise = |player: &mut ReplayFilePlayer, frame: u64| -> Vec<u8> {
            player.go_to_keyframe(frame).expect("seek");
            match player.next_packet() {
                Ok(Some(Packet::Keyframe { .. })) => {}
                _ => panic!("no keyframe at {frame}"),
            }
            player.current_keyframe_state().to_vec()
        };
        player.set_keyframe_states_wanted(false);
        let first = materialise(&mut player, a);
        let mut passed = 0;
        while let Ok(Some(packet)) = player.next_packet() {
            if let Packet::Keyframe { metadata, .. } = packet {
                passed += 1;
                if metadata.elapsed_frames >= b {
                    break;
                }
            }
        }
        let after_play = player.current_keyframe_state().to_vec();
        let again = materialise(&mut player, a);
        println!("keyframe {a}: {} bytes first, {} bytes after playing past {passed} keyframes to {b} ({} bytes there) and seeking back", first.len(), again.len(), after_play.len());
        println!("  header first: {}", describe_header(&first));
        println!("  header again: {}", describe_header(&again));
        let n = first.len().min(again.len());
        let differing = (0..n).filter(|&i| first[i] != again[i]).count();
        println!("  {differing} differing bytes in the common {n}");
        if let Ok(sections) = layout(&first) {
            for (name, offset, len) in sections {
                let end = (offset + len).min(n);
                let d = (offset..end).filter(|&i| first[i] != again[i]).count();
                if d > 0 {
                    println!("  {name}: {d}");
                }
            }
        }
        return;
    }
    let frames: Vec<u64> = player.all_keyframes().range(from..=to).map(|(f, _)| *f).collect();
    println!("{replay}: v{}, {} frames, checking {} keyframes", player.get_replay_version(), player.get_total_frames(), frames.len());

    let mut core = rom_path.map(|p| {
        let rom = std::fs::read(&p).expect("read rom");
        let mut nds = NintendoDS::new_from_rom(&rom, None, std_timestamp_provider(), false).expect("load rom");
        nds.set_skip_drawing(true);
        nds
    });

    if let Some(spec) = std::env::var_os("RELOAD") {
        // RELOAD=frame[,strip]: on the bare core, load that keyframe, run a frame, load it again
        // (what a seek right after opening a replay does); `strip` drops a trailing SHUK first.
        let spec = spec.to_string_lossy().into_owned();
        let mut parts = spec.split(',');
        let frame: u64 = parts.next().unwrap().parse().expect("frame");
        let option = parts.next();
        let strip = option == Some("strip");
        // `damage`: the length field as the first SHUK build left it in a buffer it had loaded.
        let damage = option == Some("damage");
        player.set_keyframe_states_wanted(false);
        player.go_to_keyframe(frame).expect("seek");
        let _ = player.next_packet();
        let mut state = player.current_keyframe_state().to_vec();
        if strip {
            if let Ok(sections) = layout(&state) {
                if let Some((_, at, _)) = sections.iter().find(|(n, _, _)| n == "SHUK") {
                    state.truncate(*at);
                    let total = state.len() as u32;
                    state[8..12].copy_from_slice(&total.to_le_bytes());
                }
            }
        }
        if damage {
            let sections = layout(&state).expect("layout");
            let (_, at, _) = sections.iter().find(|(n, _, _)| n == "SHUK").expect("no SHUK");
            let at = *at as u32;
            state[8..12].copy_from_slice(&at.to_le_bytes());
        }
        let original = state.clone();
        let core = core.as_mut().expect("RELOAD needs the ROM");
        core.set_skip_drawing(false);
        println!("state {} bytes ({}); loading", state.len(), describe_header(&state));
        core.load_save_state(&state).expect("first load");
        println!("loaded (buffer {}); running a frame", if state == original { "unchanged" } else { "CHANGED" });
        core.run_unlocked();
        let after_frame = core.create_save_state();
        println!("ran; loading again");
        core.load_save_state(&state).expect("second load");
        println!("loaded again (buffer {}); running a frame", if state == original { "unchanged" } else { "CHANGED" });
        core.run_unlocked();
        let again = core.create_save_state();
        println!("the frame after each load: {}", if after_frame == again { "identical" } else { "DIFFERENT" });
        println!("ok");
        return;
    }

    let mut previous_layout: Option<Vec<(String, usize)>> = None;
    let mut previous_header: Option<Vec<u8>> = None;
    let (mut bad_layout, mut bad_header, mut failed_loads) = (0u64, 0u64, 0u64);
    for frame in frames {
        if let Err(e) = player.go_to_keyframe(frame) {
            println!("kf {frame}: go_to_keyframe failed: {e:?}");
            continue;
        }
        match player.next_packet() {
            Ok(Some(Packet::Keyframe { .. })) => {}
            Ok(_) => {
                println!("kf {frame}: no keyframe packet");
                continue;
            }
            Err(e) => {
                println!("kf {frame}: next_packet failed: {e:?}");
                continue;
            }
        }
        let stale = player.current_keyframe_has_stale_transients();
        let state = player.current_keyframe_state().to_vec();

        let mut notes = Vec::new();
        let header_ok = state.len() >= 16 && u32_at(&state, 8) as usize == state.len();
        if !header_ok {
            bad_header += 1;
            notes.push(format!("HEADER {}", describe_header(&state)));
        }
        let header: Vec<u8> = state.iter().take(16).copied().collect();
        if previous_header.as_ref().is_some_and(|h| *h != header) {
            notes.push(format!("header changed: {}", describe_header(&state)));
        }
        match layout(&state) {
            Ok(sections) => {
                let shape: Vec<(String, usize)> = sections.iter().map(|(n, _, l)| (n.clone(), *l)).collect();
                match &previous_layout {
                    None => println!(
                        "kf {frame}: {}; sections {}",
                        describe_header(&state),
                        sections.iter().map(|(n, o, l)| format!("{n}@{o:#x}+{l:#x}")).collect::<Vec<_>>().join(" ")
                    ),
                    Some(p) if *p != shape => notes.push(format!(
                        "layout changed: {}",
                        sections.iter().map(|(n, o, l)| format!("{n}@{o:#x}+{l:#x}")).collect::<Vec<_>>().join(" ")
                    )),
                    _ => {}
                }
                previous_layout = Some(shape);
            }
            Err(e) => {
                bad_layout += 1;
                notes.push(format!("LAYOUT {e}"));
            }
        }
        previous_header = Some(header);

        if let Some(core) = core.as_mut() {
            let loaded = if stale { core.load_save_state_with_stale_output(&state) } else { core.load_save_state(&state) };
            if let Err(e) = loaded {
                failed_loads += 1;
                notes.push(format!("LOAD FAILED ({}): {e}", if stale { "stale output" } else { "plain" }));
            }
        }

        if all || !notes.is_empty() {
            println!("kf {frame}: {} bytes{}{}", state.len(), if stale { ", stale transients" } else { "" }, if notes.is_empty() { String::new() } else { format!(": {}", notes.join("; ")) });
        }
    }
    println!("{bad_header} bad headers, {bad_layout} bad layouts, {failed_loads} failed loads");
}
