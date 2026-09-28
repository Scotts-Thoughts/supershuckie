//! Print the kinds of the top-level packets around the first delta keyframes of a Nintendo 3DS
//! replay (frame numbers included): a check of how keyframes sit among a frame's packets.
//!
//! ```text
//! packet_seq <file.replay> [keyframes to show]
//! ```

use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::Packet;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: packet_seq <file.replay> [n]");
    let n: usize = args.next().map(|s| s.parse().unwrap()).unwrap_or(3);
    let bytes = std::fs::read(&path).expect("read");
    let player = ReplayFilePlayer::new(&bytes, false).expect("parse");
    let packets = player.all_uncompressed_packets();
    let mut frame = 0u64;
    let mut frames_at: Vec<u64> = Vec::with_capacity(packets.len());
    for p in packets.iter() {
        frames_at.push(frame);
        if matches!(p, Packet::NextFrame { .. }) {
            frame += 1;
        }
    }
    let mut shown = 0;
    for (i, p) in packets.iter().enumerate() {
        if let Packet::StoredKeyframe { level, metadata, .. } = p && *level > 0 {
            println!("--- keyframe packet #{i}: level {level}, metadata frame {}, frames counted before it {}", metadata.elapsed_frames, frames_at[i]);
            for j in i.saturating_sub(6)..(i + 7).min(packets.len()) {
                let kind = match &packets[j] {
                    Packet::NextFrame { .. } => "NextFrame".to_owned(),
                    Packet::ChangeInput { data } => format!("ChangeInput {:02x?}", &data[..data.len().min(4)]),
                    Packet::StoredKeyframe { level, metadata, .. } => format!("StoredKeyframe L{level} f{}", metadata.elapsed_frames),
                    Packet::Thumbnail { elapsed_frames, .. } => format!("Thumbnail f{elapsed_frames}"),
                    other => format!("{:?}", std::mem::discriminant(other)),
                };
                println!("  #{j} (frames before: {}) {kind}", frames_at[j]);
            }
            shown += 1;
            if shown >= n {
                break;
            }
        }
    }
}
