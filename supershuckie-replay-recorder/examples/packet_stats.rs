//! Counts the packets of each kind in a replay file, and how many `ChangeInput` packets there are
//! per frame -- the recorder should write about one per emulated frame, and a paced core polled
//! many times per frame used to write one per poll.
//!
//! ```text
//! cargo run --release -p supershuckie-replay-recorder --example packet_stats -- <file.replay> [max_frames]
//! ```

use std::collections::BTreeMap;

use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::Packet;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: packet_stats <file.replay> [max_frames]");
    let max_frames: u64 = args.next().map(|s| s.parse().expect("max_frames")).unwrap_or(u64::MAX);

    let bytes = std::fs::read(&path).expect("read");
    let mut player = ReplayFilePlayer::new(&bytes, false).expect("parse");
    println!("{path}: v{}, {} frames, {} keyframes", player.get_replay_version(), player.get_total_frames(), player.all_keyframes().len());

    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut frames = 0u64;
    let mut change_inputs_this_frame = 0u64;
    let mut max_change_inputs_in_a_frame = 0u64;
    while let Some(packet) = player.next_packet().expect("read packet") {
        let name = match packet {
            Packet::NoOp => "NoOp",
            Packet::NextFrame { .. } => "NextFrame",
            Packet::WriteMemory { .. } => "WriteMemory",
            Packet::ChangeInput { .. } => "ChangeInput",
            Packet::ChangeSpeed { .. } => "ChangeSpeed",
            Packet::ResetConsole => "ResetConsole",
            Packet::LoadSaveState { .. } => "LoadSaveState",
            Packet::Bookmark { .. } => "Bookmark",
            Packet::BookmarkTable { .. } => "BookmarkTable",
            Packet::Keyframe { .. } => "Keyframe",
            _ => "Other"
        };
        *counts.entry(name).or_default() += 1;
        match packet {
            Packet::ChangeInput { .. } => change_inputs_this_frame += 1,
            Packet::NextFrame { .. } => {
                frames += 1;
                max_change_inputs_in_a_frame = max_change_inputs_in_a_frame.max(change_inputs_this_frame);
                change_inputs_this_frame = 0;
                if frames >= max_frames {
                    break
                }
            }
            _ => {}
        }
    }

    for (name, count) in &counts {
        println!("  {name:<14} {count}");
    }
    let change_inputs = counts.get("ChangeInput").copied().unwrap_or(0);
    if frames > 0 {
        println!(
            "ChangeInput per frame: {:.1} average, {} max (over {frames} frames)",
            change_inputs as f64 / frames as f64,
            max_change_inputs_in_a_frame
        );
    }
}
