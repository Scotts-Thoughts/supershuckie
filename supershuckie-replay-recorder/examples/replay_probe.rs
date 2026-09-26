//! Open a replay and seek to every keyframe, reporting what the player hands out: a quick check
//! of a file's keyframe chain without an emulator.
//!
//! ```text
//! cargo run --release -p supershuckie-replay-recorder --example replay_probe -- <file.replay> [max keyframes]
//! ```

use std::time::Instant;

use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::Packet;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: replay_probe <file.replay> [max keyframes]");
    let max: usize = args.next().map(|s| s.parse().unwrap()).unwrap_or(usize::MAX);
    let bytes = std::fs::read(&path).expect("read");
    let t = Instant::now();
    let mut player = ReplayFilePlayer::new(&bytes, false).expect("parse");
    println!("parsed in {:.0} ms: version {}, {} frames, {} keyframes, {} top-level packets",
             t.elapsed().as_secs_f64() * 1000.0, player.get_replay_version(), player.get_total_frames(),
             player.all_keyframes().len(), player.all_uncompressed_packets().len());
    let levels: Vec<String> = player.all_uncompressed_packets().iter().filter_map(|p| match p {
        Packet::StoredKeyframe { level, frame_len, frame_offset, metadata, .. } => Some(format!("f{}:L{level}:{}B@{frame_offset}", metadata.elapsed_frames, frame_len)),
        Packet::Keyframe { metadata, .. } => Some(format!("f{}:full", metadata.elapsed_frames)),
        Packet::RegionDeltaKeyframe { metadata, .. } => Some(format!("f{}:delta", metadata.elapsed_frames)),
        Packet::CompressedBlob { keyframes, .. } => Some(format!("blob[{}]", keyframes.len())),
        _ => None
    }).collect();
    println!("keyframes: {}", levels.join(" "));
    player.set_keyframe_states_wanted(true);
    let frames: Vec<u64> = player.all_keyframes().keys().copied().collect();
    let mut order: Vec<u64> = frames.iter().rev().copied().collect();
    order.extend(frames.iter().copied());
    for frame in order.into_iter().take(max) {
        let t = Instant::now();
        match player.go_to_keyframe(frame) {
            Err(e) => { println!("frame {frame}: go_to_keyframe failed: {e:?}"); continue; }
            Ok(()) => {}
        }
        match player.next_packet() {
            Ok(Some(Packet::Keyframe { metadata, state })) => println!("frame {frame}: keyframe at {} with {} bytes of state, {:.0} ms", metadata.elapsed_frames, state.len(), t.elapsed().as_secs_f64() * 1000.0),
            Ok(Some(other)) => println!("frame {frame}: handed out a different packet: {}", match other { Packet::NextFrame { .. } => "NextFrame", Packet::StoredKeyframe { .. } => "StoredKeyframe (not materialised)", _ => "other" }),
            Ok(None) => println!("frame {frame}: end of stream"),
            Err(e) => println!("frame {frame}: next_packet failed: {e:?}"),
        }
    }
}
