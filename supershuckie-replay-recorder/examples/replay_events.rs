//! Lists the events of a replay that change emulator state outside of plain input: console
//! resets, loaded save states, memory writes (grouped by address) and the keyframes around them,
//! plus the packet sequence of the first frames. A desync hunting aid; no emulator involved.
//!
//! ```text
//! cargo run --release -p supershuckie-replay-recorder --example replay_events -- <file.replay> [first frames to list]
//! ```

use std::collections::BTreeMap;

use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::Packet;

fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" ")
}

fn kind(packet: &Packet) -> String {
    match packet {
        Packet::NoOp => "NoOp".into(),
        Packet::NextFrame { timestamp_delta } => format!("NextFrame(+{timestamp_delta} ms)"),
        Packet::WriteMemory { address, data } => format!("WriteMemory({address:08X}, [{}])", hex(data)),
        Packet::ChangeInput { data } => format!("ChangeInput([{}])", hex(data)),
        Packet::ChangeSpeed { speed } => format!("ChangeSpeed({speed:?})"),
        Packet::ResetConsole => "ResetConsole".into(),
        Packet::LoadSaveState { state } => format!("LoadSaveState({} bytes)", state.len()),
        Packet::Bookmark { metadata } => format!("Bookmark({:?} @ {})", metadata.name, metadata.elapsed_frames),
        Packet::BookmarkTable { .. } => "BookmarkTable".into(),
        Packet::Keyframe { metadata, state } => format!("Keyframe(frame {}, input [{}], {} bytes)", metadata.elapsed_frames, hex(&metadata.input), state.len()),
        Packet::IncrementCounter { name, delta } => format!("IncrementCounter({name:?}, {delta})"),
        Packet::SerialIn { .. } => "SerialIn".into(),
        other => {
            let s = format!("{other:?}");
            s.chars().take(60).collect()
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: replay_events <file.replay> [first frames to list]");
    let list = args.next().unwrap_or_else(|| "3".into());
    // "n" lists the first n frames, "a..b" the frames a to b; inputs are listed only when they change.
    let (list_from, list_to): (u64, u64) = match list.split_once("..") {
        Some((a, b)) => (a.parse().expect("from"), b.parse().expect("to")),
        None => (0, list.parse().expect("frames")),
    };
    let mut last_input: Vec<u8> = Vec::new();

    let bytes = std::fs::read(&path).expect("read");
    let mut player = ReplayFilePlayer::new(&bytes, false).expect("parse");
    player.set_keyframe_states_wanted(false);
    println!("{path}: v{}, {} frames, {} keyframes", player.get_replay_version(), player.get_total_frames(), player.all_keyframes().len());
    let kf: Vec<u64> = player.all_keyframes().iter().map(|(frame, _)| *frame).collect();
    let shared: Vec<(u64, usize)> = player.all_keyframes().iter().filter(|(_, k)| k.len() > 1).map(|(frame, k)| (*frame, k.len())).collect();
    println!("first keyframes: {:?}", &kf[..kf.len().min(12)]);
    println!("frames holding more than one keyframe ({}): {:?}", shared.len(), &shared[..shared.len().min(40)]);
    let irregular: Vec<(u64, u64)> = kf.windows(2).filter(|w| w[1] - w[0] != 120).map(|w| (w[0], w[1])).collect();
    println!("keyframe gaps other than 120 frames ({}): {:?}", irregular.len(), &irregular[..irregular.len().min(40)]);

    let mut frame = 0u64;
    let mut writes: BTreeMap<(u64, usize), (u64, u64, u64, Vec<u8>)> = BTreeMap::new(); // (addr,len) -> (count, first frame, last frame, last data)
    let mut write_frames = 0u64;
    let mut wrote_this_frame = false;
    let mut recent: Vec<String> = Vec::new();
    let mut after_event: u32 = 0;
    let mut others: BTreeMap<String, u64> = BTreeMap::new();
    let (common_address, common_len) = (0x021D4938u64, 4usize);
    // Replay time as the timeline shows it: the recorded wall-clock deltas, summed.
    let mut millis = 0u64;
    let clock = |ms: u64| format!("{}:{:02}:{:02}.{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    while let Some(packet) = player.next_packet().expect("read packet") {
        let k = kind(packet);
        if (list_from..list_to).contains(&frame) {
            let repeat_input = matches!(packet, Packet::ChangeInput { data } if data.as_slice() == last_input.as_slice());
            let routine = matches!(packet, Packet::NextFrame { .. }) || matches!(packet, Packet::WriteMemory { address, .. } if *address == common_address);
            if !repeat_input && !routine {
                println!("  f{frame}: {k}");
            }
        }
        if let Packet::ChangeInput { data } = packet {
            last_input = data.to_vec();
        }
        match packet {
            Packet::NextFrame { timestamp_delta } => {
                frame += 1;
                millis += timestamp_delta.0;
                if frame % 50_000 == 0 {
                    println!("  (frame {frame} = replay time {})", clock(millis));
                }
                if wrote_this_frame {
                    write_frames += 1;
                }
                wrote_this_frame = false;
            }
            Packet::WriteMemory { address, data } => {
                wrote_this_frame = true;
                if (*address, data.len()) != (common_address, common_len) {
                    println!("  write at frame {frame} (replay time {}): {k}", clock(millis));
                }
                let e = writes.entry((*address, data.len())).or_insert((0, frame, frame, Vec::new()));
                e.0 += 1;
                e.2 = frame;
                e.3 = data.to_vec();
            }
            Packet::IncrementCounter { .. } => {
                println!("  {k} at frame {frame} (replay time {})", clock(millis));
            }
            Packet::ResetConsole | Packet::LoadSaveState { .. } | Packet::ChangeSpeed { .. } | Packet::Bookmark { .. } => {
                println!("=== {k} at frame {frame} (replay time {}); preceding packets:", clock(millis));
                for r in &recent {
                    println!("      {r}");
                }
                after_event = 8;
            }
            Packet::ChangeInput { .. } | Packet::Keyframe { .. } => {}
            _ => {
                *others.entry(k.chars().take(24).collect()).or_default() += 1;
            }
        }
        if after_event > 0 && !matches!(packet, Packet::ResetConsole | Packet::LoadSaveState { .. } | Packet::ChangeSpeed { .. } | Packet::Bookmark { .. }) {
            println!("    then f{frame}: {k}");
            after_event -= 1;
        }
        recent.push(format!("f{frame}: {k}"));
        if recent.len() > 6 {
            recent.remove(0);
        }
    }
    println!("{frame} frames walked; frames with at least one memory write: {write_frames}");
    println!("memory writes by (address, length):");
    let mut by_count: Vec<_> = writes.iter().collect();
    by_count.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
    for ((address, len), (count, first, last, data)) in by_count.iter().take(40) {
        println!("  {address:08X} x{len}: {count} writes, frames {first}..{last}, last data [{}]", hex(data));
    }
    if writes.len() > 40 {
        println!("  ... {} more (address, length) pairs", writes.len() - 40);
    }
    println!("other packets: {others:?}");
}
