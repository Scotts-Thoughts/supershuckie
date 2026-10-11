//! Surveys Nintendo DS replays for the firmware Wi-Fi ID question (see the HG desync notes): how
//! far into its life the recording core was when each recording started (melonDS's frame counter
//! in the first keyframe, which counts frames since the core was created and survives console
//! resets), the firmware SPI state then, and the console resets and loaded save states inside.
//!
//! ```text
//! cargo run --release -p supershuckie-replay-recorder --example nds_replay_survey -- <file.replay | folder>...
//! ```

use std::fs::File;
use std::path::{Path, PathBuf};

use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::replay_file::ReplayConsoleType;
use supershuckie_replay_recorder::Packet;

/// melonDS's `NDS::NumFrames` inside the NDSG section (the first one): after its 16-byte
/// header come the config word, main RAM, shared WRAM, ARM7 WRAM and the I/O state.
const NUM_FRAMES_OFFSET: usize = 0x10 + 0x10 + 0x10182C8;

fn section<'a>(state: &'a [u8], magic: &[u8; 4]) -> Option<&'a [u8]> {
    let mut offset = 0x10usize;
    while offset + 8 <= state.len() {
        let length = u32::from_le_bytes(state[offset + 4..offset + 8].try_into().ok()?) as usize;
        if &state[offset..offset + 4] == magic {
            return state.get(offset + 16..offset + length);
        }
        if length < 16 {
            return None;
        }
        offset += length;
    }
    None
}

fn num_frames(state: &[u8]) -> Option<u32> {
    if state.get(0x10..0x14)? != b"NDSG" {
        return None;
    }
    Some(u32::from_le_bytes(state.get(NUM_FRAMES_OFFSET..NUM_FRAMES_OFFSET + 4)?.try_into().ok()?))
}

/// The SPFW section: Hold (u32), CurCmd (u8), DataPos (u32), Data (u8), StatusReg (u8), Addr (u32).
fn firmware_spi(state: &[u8]) -> String {
    match section(state, b"SPFW") {
        Some(s) if s.len() >= 15 => {
            let hold = u32::from_le_bytes(s[0..4].try_into().unwrap());
            let cmd = s[4];
            let pos = u32::from_le_bytes(s[5..9].try_into().unwrap());
            let status = s[10];
            let addr = u32::from_le_bytes(s[11..15].try_into().unwrap());
            format!("spi hold {hold} cmd {cmd:02X} pos {pos} status {status:02X} addr {addr:05X}")
        }
        _ => "no SPFW".into(),
    }
}

/// The DS firmware's CRC16 (melonDS `CRC16`).
fn crc16(data: &[u8], start: u32) -> u16 {
    const TABLE: [u32; 8] = [0xC0C1, 0xC181, 0xC301, 0xC601, 0xCC01, 0xD801, 0xF001, 0xA001];
    let mut crc = start;
    for byte in data {
        crc ^= *byte as u32;
        for (j, value) in TABLE.iter().enumerate() {
            if crc & 1 != 0 {
                crc >>= 1;
                crc ^= value << (7 - j);
            } else {
                crc >>= 1;
            }
        }
    }
    (crc & 0xFFFF) as u16
}

/// Copies of a firmware Wi-Fi access point block (256 bytes, CRC16 of the first 0xFE at 0xFE) in
/// main RAM that hold a WFC ID: (RAM address, ID bytes 0xF0..0xFD).
fn wfc_copies(state: &[u8]) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    let Some(ram) = state.get(0x24..0x24 + 0x40_0000) else { return out };
    let mut at = 0usize;
    while at + 256 <= ram.len() {
        let block = &ram[at..at + 256];
        let id = &block[0xF0..0xFE];
        if id.iter().any(|b| *b != 0) && block[0xEF] == 1 && (block[0xE7] == 0 || block[0xE7] == 0xFF) {
            let stored = u16::from_le_bytes([block[0xFE], block[0xFF]]);
            if crc16(&block[..0xFE], 0) == stored {
                out.push((0x0200_0000 + at as u32, id.iter().map(|b| format!("{b:02X}")).collect()));
            }
        }
        at += 4;
    }
    out
}

fn survey(path: &Path) {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let Ok(file) = File::open(path) else {
        println!("{name}: cannot open");
        return;
    };
    // SAFETY: read only; nothing modifies the user's finished replays while this runs.
    let Ok(map) = (unsafe { memmap2::Mmap::map(&file) }) else {
        println!("{name}: cannot map");
        return;
    };
    let mut player = match ReplayFilePlayer::new(&map[..], true) {
        Ok(p) => p,
        Err(e) => {
            println!("{name}: cannot parse ({e:?})");
            return;
        }
    };
    if player.get_replay_metadata().console_type != ReplayConsoleType::NintendoDS {
        return;
    }
    let total = player.get_total_frames();
    let mut frame = 0u64;
    let mut resets: Vec<u64> = Vec::new();
    let mut loads: Vec<(u64, Option<u32>)> = Vec::new();
    let mut first: Option<(u64, Option<u32>, String)> = None;
    let mut copies: Vec<(u32, String)> = Vec::new();
    let mut second: Option<(u64, Option<u32>)> = None;
    loop {
        let packet = match player.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(e) => {
                println!("{name}: read error at frame {frame} ({e:?})");
                break;
            }
        };
        let mut states_done = false;
        match packet {
            Packet::NextFrame { .. } => frame += 1,
            Packet::ResetConsole => resets.push(frame),
            Packet::LoadSaveState { state } => loads.push((frame, num_frames(state.as_slice()))),
            Packet::Keyframe { metadata, state } => {
                if first.is_none() {
                    first = Some((metadata.elapsed_frames, num_frames(state.as_slice()), firmware_spi(state.as_slice())));
                    copies = wfc_copies(state.as_slice());
                } else if second.is_none() {
                    second = Some((metadata.elapsed_frames, num_frames(state.as_slice())));
                    states_done = true;
                }
            }
            _ => {}
        }
        if states_done {
            player.set_keyframe_states_wanted(false);
        }
    }
    let (kf0_frame, kf0_count, kf0_spi) = first.unwrap_or((0, None, "no keyframe".into()));
    let check = match (kf0_count, second) {
        (Some(a), Some((f, Some(b)))) if u64::from(b.wrapping_sub(a)) == f - kf0_frame => "counter ok".to_string(),
        (_, Some((f, b))) => format!("counter at kf {f}: {b:?}"),
        _ => String::new(),
    };
    let resets_text = if resets.is_empty() {
        "no resets".to_string()
    } else {
        format!("{} resets, first at frame {}", resets.len(), resets[0])
    };
    let loads_text = if loads.is_empty() { String::new() } else { format!(", {} state loads {:?}", loads.len(), &loads[..loads.len().min(4)]) };
    println!(
        "{name}: {total} frames; start counter {} ({check}); {kf0_spi}; {resets_text}{loads_text}; WFC ID copies in RAM: {:?}",
        kf0_count.map(|c| c.to_string()).unwrap_or_else(|| "?".into()),
        copies,
    );
}

fn main() {
    let mut paths: Vec<PathBuf> = Vec::new();
    for arg in std::env::args().skip(1) {
        let path = PathBuf::from(&arg);
        if path.is_dir() {
            let mut inside: Vec<PathBuf> = std::fs::read_dir(&path)
                .expect("read folder")
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "replay"))
                .collect();
            inside.sort();
            paths.extend(inside);
        } else {
            paths.push(path);
        }
    }
    for path in paths {
        survey(&path);
    }
}
