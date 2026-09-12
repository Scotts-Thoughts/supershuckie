//! Measures keyframe seek latency and playback memory for a replay file.
//!
//! ```text
//! cargo run --release -p supershuckie-replay-recorder --example seek_bench -- <file.replay> [seeks]
//! ```
//!
//! Performs `seeks` (default 40) pseudo-random `go_to_keyframe` + `next_packet` pairs (cold seeks
//! into other chains as well as short forward hops), then plays the first 2,000 packets, and prints
//! the timings.

use std::time::Instant;

use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::Packet;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: seek_bench <file.replay> [seeks]");
    let seeks: usize = args.next().map(|s| s.parse().expect("seeks")).unwrap_or(40);

    let started = Instant::now();
    let bytes = std::fs::read(&path).expect("read");
    let mut player = ReplayFilePlayer::new(&bytes, false).expect("parse");
    println!(
        "{path}: v{}, {} frames, {} keyframes, parsed in {:.0} ms",
        player.get_replay_version(),
        player.get_total_frames(),
        player.all_keyframes().len(),
        started.elapsed().as_secs_f64() * 1000.0
    );

    let frames: Vec<u64> = player.all_keyframes().keys().copied().collect();
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let mut worst = 0.0f64;
    let mut total = 0.0f64;
    let mut previous = None;
    for i in 0..seeks {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        // Alternate cold random seeks with short forward hops from the previous position.
        let frame = match (i % 2, previous) {
            (1, Some(p)) => frames.iter().copied().find(|&f| f > p + 600).unwrap_or(frames[0]),
            _ => frames[(x as usize) % frames.len()],
        };
        let t = Instant::now();
        player.go_to_keyframe(frame).expect("seek");
        let packet = player.next_packet().expect("read").expect("keyframe");
        let Packet::Keyframe { metadata, state } = packet else { panic!("not a keyframe") };
        assert_eq!(metadata.elapsed_frames, frame);
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        worst = worst.max(ms);
        total += ms;
        println!("seek #{i:2} to frame {frame:>8}: {ms:7.1} ms ({} byte state)", state.len());
        previous = Some(frame);
    }
    println!("average {:.1} ms, worst {:.1} ms over {seeks} seeks", total / seeks as f64, worst);

    let t = Instant::now();
    player.go_to_keyframe(0).expect("seek 0");
    let mut packets = 0;
    let mut keyframes = 0;
    while let Some(packet) = player.next_packet().expect("read") {
        packets += 1;
        if matches!(packet, Packet::Keyframe { .. }) {
            keyframes += 1;
        }
        if packets >= 2000 {
            break;
        }
    }
    println!("played {packets} packets ({keyframes} keyframes) in {:.1} ms", t.elapsed().as_secs_f64() * 1000.0);
}
