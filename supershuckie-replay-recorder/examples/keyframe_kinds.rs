//! How a replay's keyframes are stored: full or delta, how many deltas copy from the ROM and how
//! much, and how many keep their regenerated output buffers (a reader can show the picture
//! straight after loading those) rather than leaving them stale.
//!
//! ```text
//! cargo run --release -p supershuckie-replay-recorder --example keyframe_kinds -- <file.replay> [rom]
//! ```
//!
//! The ROM is only needed for the stale/exact count of a file whose keyframes copy from it.

use std::sync::Arc;

use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::{decompress_data, Packet, PacketIO};

#[derive(Default)]
struct Counts {
    full: u64,
    full_bytes: u64,
    delta: u64,
    delta_bytes: u64,
    with_rom: u64,
    rom_ref_bytes: u64,
}

fn count(packet: &Packet, counts: &mut Counts) {
    match packet {
        Packet::Keyframe { state, .. } => {
            counts.full += 1;
            counts.full_bytes += state.len() as u64;
        }
        Packet::RegionDeltaKeyframe { control, rom_refs, data, .. } => {
            counts.delta += 1;
            counts.delta_bytes += (control.len() + rom_refs.len() + data.len()) as u64;
            if !rom_refs.is_empty() {
                counts.with_rom += 1;
                counts.rom_ref_bytes += rom_refs.len() as u64;
            }
        }
        Packet::DeltaKeyframe { diff, .. } => {
            counts.delta += 1;
            counts.delta_bytes += diff.len() as u64 * 8;
        }
        _ => {}
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: keyframe_kinds <file.replay> [rom]");
    let rom = args.next().map(|p| Arc::new(std::fs::read(p).expect("read rom")));
    let bytes = std::fs::read(&path).expect("read replay");
    let mut player = ReplayFilePlayer::new(&bytes, true).expect("parse replay");
    let version = player.get_replay_version();
    println!("{path}: v{version}, {} frames, {} keyframes, {} bytes", player.get_total_frames(), player.all_keyframes().len(), bytes.len());

    let mut counts = Counts::default();
    for packet in player.all_uncompressed_packets() {
        match packet {
            Packet::CompressedBlob { compressed_data, uncompressed_size, .. } => {
                let raw = decompress_data(compressed_data.as_slice(), *uncompressed_size as usize).expect("decompress blob");
                let mut from = raw.as_slice();
                while !from.is_empty() {
                    let Ok(inner) = Packet::read_all(&mut from, version) else { break };
                    count(&inner, &mut counts);
                }
            }
            other => count(other, &mut counts),
        }
    }
    println!(
        "stored: {} full keyframes ({} bytes before compression), {} deltas ({} bytes), of which {} copy from the ROM ({} bytes of references)",
        counts.full, counts.full_bytes, counts.delta, counts.delta_bytes, counts.with_rom, counts.rom_ref_bytes
    );

    if player.wants_rom() {
        match rom {
            Some(rom) => player.set_rom(rom),
            None => {
                println!("(pass the ROM to count stale and exact keyframes: this file's keyframes copy from it)");
                return;
            }
        }
    }
    player.set_keyframe_states_wanted(false);
    let (mut stale, mut exact) = (0u64, 0u64);
    loop {
        match player.next_packet() {
            Ok(Some(Packet::Keyframe { .. })) => {
                if player.current_keyframe_has_stale_transients() { stale += 1 } else { exact += 1 }
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(e) => {
                println!("read error: {e:?}");
                break;
            }
        }
    }
    println!("as read: {exact} keyframes with exact output buffers, {stale} that left them stale");
}
