//! Smoke test for `SuperShuckieCore` on a real NDS ROM: recording with keyframe buffer reuse,
//! playing the recording back, and seeking through a replay without copying keyframe states.
//!
//! ```text
//! core_smoke <rom.nds> [existing.replay]
//! ```
//!
//! Link it like `nds_bench` (see that file's header).

use std::fs::File;
use std::io::BufWriter;
use std::num::NonZeroU64;

use supershuckie_core::emulator::{EmulatorCore, NintendoDS, PartialReplayRecordMetadata};
use supershuckie_core::{std_timestamp_provider, SuperShuckieCore};
use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::replay_file::record::ReplayFileRecorderSettings;
use supershuckie_replay_recorder::replay_file::ReplayPatchFormat;
use supershuckie_replay_recorder::ByteVec;

const MAIN_RAM: u32 = 0x0200_0000;
const MAIN_RAM_LEN: usize = 4 * 1024 * 1024;

fn main_ram(core: &SuperShuckieCore) -> Vec<u8> {
    let mut ram = vec![0u8; MAIN_RAM_LEN];
    core.get_core().read_ram(MAIN_RAM, &mut ram).expect("read main RAM");
    ram
}

fn new_core(rom: &[u8]) -> SuperShuckieCore {
    let nds = NintendoDS::new_from_rom(rom, None, std_timestamp_provider(), false);
    SuperShuckieCore::new(Box::new(nds), std_timestamp_provider())
}

fn main() {
    let mut args = std::env::args().skip(1);
    let rom_path = args.next().expect("usage: core_smoke <rom.nds> [existing.replay]");
    let existing = args.next();
    let rom = std::fs::read(&rom_path).expect("read rom");

    // --- 1. Record 500 frames from boot (keyframes every 120 frames -> the buffer pool cycles). ---
    let dir = std::env::temp_dir().join("supershuckie-core-smoke");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let final_path = dir.join("smoke.replay");
    let temp_path = dir.join("smoke.temp.replay");

    let mut rec = new_core(&rom);
    for _ in 0..200 {
        rec.run_unlocked();
    }
    rec.start_recording_replay(PartialReplayRecordMetadata {
        rom_name: "smoke".into(),
        rom_filename: "smoke.nds".into(),
        settings: ReplayFileRecorderSettings::default(),
        patch_format: ReplayPatchFormat::Unpatched,
        patch_target_checksum: Default::default(),
        patch_data: ByteVec::new(),
        frames_per_keyframe: NonZeroU64::new(120).unwrap(),
        final_file: BufWriter::new(File::create(&final_path).expect("create final")),
        temp_file: BufWriter::new(File::create(&temp_path).expect("create temp")),
    }).expect("start recording");

    for _ in 0..500 {
        rec.run_unlocked();
    }
    let recorded_ram = main_ram(&rec);
    assert_eq!(rec.stop_recording_replay(), Some(true), "recording closed cleanly");
    println!("recorded 500 frames to {}", final_path.display());

    // --- 2. Play it back into a fresh core; the end state must match the recorder's. ---
    let bytes = std::fs::read(&final_path).expect("read recording");
    let mut player = ReplayFilePlayer::new(&bytes, false).expect("parse recording");
    let total = player.get_total_frames();
    let keyframes = player.all_keyframes().len();
    assert_eq!(total, 500, "recorded frame count");
    assert!(keyframes >= 5, "expected the initial keyframe plus one every 120 frames, got {keyframes}");
    player.enable_threading();

    let mut play = new_core(&rom);
    play.attach_replay_player(player, true).expect("attach");
    while !play.is_replay_stalled() {
        play.run_unlocked();
    }
    assert_eq!(play.total_frames(), 500, "playback reached the end");
    assert!(main_ram(&play) == recorded_ram, "playback end state differs from the recording");
    println!("playback of the recording matches ({keyframes} keyframes)");

    // --- 3. Seeks (clone-free keyframe states): same target twice must give the same state. ---
    play.go_to_replay_frame(400);
    assert_eq!(play.total_frames(), 400);
    let a = main_ram(&play);
    play.go_to_replay_frame(120);
    assert_eq!(play.total_frames(), 120);
    play.go_to_replay_frame(400);
    let b = main_ram(&play);
    assert!(a == b, "seeking to the same frame twice gave different states");
    println!("seeks are reproducible");

    // --- 4. Optionally the same on a real replay: a long seek, then a short one. ---
    if let Some(path) = existing {
        let bytes = std::fs::read(&path).expect("read replay");
        let mut player = ReplayFilePlayer::new(&bytes, true).expect("parse replay");
        player.enable_threading();
        let total = player.get_total_frames();
        let mut core = new_core(&rom);
        core.attach_replay_player(player, true).expect("attach");
        let target = total / 3;
        core.go_to_replay_frame(target);
        assert_eq!(core.total_frames(), target);
        let a = main_ram(&core);
        let sa = core.create_save_state();
        let sram_a = core.save_sram();
        core.go_to_replay_frame(target);
        let sb = core.create_save_state();
        println!("real replay: seek {target} -> {target} again: {}", if sa == sb { "identical".to_string() } else { describe_diff(&sa, &sb) });
        core.go_to_replay_frame(target + 300);
        core.go_to_replay_frame(target);
        let sc = core.create_save_state();
        let sram_c = core.save_sram();
        println!("real replay: seek {target} -> {} -> {target}: {}", target + 300, if sa == sc { "identical".to_string() } else { describe_diff(&sa, &sc) });
        let sram_diff = sram_a.iter().zip(sram_c.iter()).filter(|(x, y)| x != y).count();
        println!("real replay: SRAM ({} bytes) differs in {sram_diff} bytes between those two seeks", sram_a.len());
        // Known, pre-existing: a handful of main-RAM bytes after a seek can depend on what the
        // emulator ran before the state load (HeartGold: 9 bytes at 0x021E1A27, values vs zeros),
        // with the unpatched melonDS as well. Nothing in the save state differs, so this is state
        // melonDS does not serialise. Reported, not asserted; the fresh-core comparison below is
        // the check that seeking itself is deterministic.
        let ram_c = main_ram(&core);
        for (i, (x, y)) in a.iter().zip(ram_c.iter()).enumerate().filter(|(_, (x, y))| x != y).take(16) {
            println!("  (history-dependent) main RAM 0x{:08X}: {x:02X} -> {y:02X}", 0x0200_0000 + i);
        }
        let mut fresh = new_core(&rom);
        let bytes2 = std::fs::read(&path).expect("read replay");
        let mut player2 = ReplayFilePlayer::new(&bytes2, true).expect("parse replay");
        player2.enable_threading();
        fresh.attach_replay_player(player2, true).expect("attach");
        fresh.go_to_replay_frame(target);
        let sd = fresh.create_save_state();
        println!("real replay: fresh core seek {target}: {}", if sa == sd { "identical".to_string() } else { describe_diff(&sa, &sd) });
        assert!(sa == sd, "seeking to the same frame from a fresh core gave a different state");
        for _ in 0..600 {
            core.run_unlocked();
        }
        println!("real replay: seek to {target} reproducible from a fresh core, 600 frames played");
    }

    let _ = std::fs::remove_file(&temp_path);
    println!("ok");
}

/// Which melonDS save-state sections differ (magic, byte count, first offset within the section).
fn describe_diff(a: &[u8], b: &[u8]) -> String {
    if a.len() != b.len() {
        return format!("length {} vs {}", a.len(), b.len());
    }
    let mut out = Vec::new();
    let mut offset = 16usize;
    while offset + 16 <= a.len() {
        let len = u32::from_le_bytes([a[offset + 4], a[offset + 5], a[offset + 6], a[offset + 7]]) as usize;
        if len < 16 || offset + len > a.len() {
            break;
        }
        let magic = String::from_utf8_lossy(&a[offset..offset + 4]).into_owned();
        let sa = &a[offset..offset + len];
        let sb = &b[offset..offset + len];
        let diff = sa.iter().zip(sb).filter(|(x, y)| x != y).count();
        if diff > 0 {
            let first = sa.iter().zip(sb).position(|(x, y)| x != y).unwrap_or(0);
            out.push(format!("{magic}: {diff} bytes (first at +{first} of {len})"));
        }
        offset += len;
    }
    out.join(", ")
}
