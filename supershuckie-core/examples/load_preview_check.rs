//! Proves that previewing a loaded save state does not change where the console ends up. Loading
//! a state through `SuperShuckieCore` draws one frame from it (so the screens show the state even
//! while paused) and then loads it again; while recording or playing together that preview frame
//! must leave no trace, or the recording (and everyone following the game) would drift from the
//! console. Two cores start from one state; one loads S with a recorder attached, the other
//! without; both then run the same scripted input and their save states and screens are compared
//! along the way.
//!
//! ```text
//! cargo run --release -p supershuckie-core --example load_preview_check -- <rom.gb|gbc|gba> [--frames n]
//! ```

use std::num::NonZeroU64;

use supershuckie_core::emulator::{EmulatorCore, GameBoyAdvance, GameBoyColor, Input, Model, PartialReplayRecordMetadata};
use supershuckie_core::{std_timestamp_provider, SuperShuckieCore};
use supershuckie_replay_recorder::replay_file::record::ReplayFileRecorderSettings;
use supershuckie_replay_recorder::replay_file::ReplayPatchFormat;
use supershuckie_replay_recorder::ByteVec;

fn new_core(rom: &[u8], gba: bool) -> Box<dyn EmulatorCore> {
    if gba {
        Box::new(GameBoyAdvance::new_from_rom(rom, None, &[], std_timestamp_provider()).expect("load ROM"))
    }
    else {
        let cgb = rom.get(0x143).map(|b| b & 0x80 != 0).unwrap_or(false);
        let (bios, model): (&[u8], Model) = if cgb {
            (include_bytes!("../../bootrom/cgb/cgb_boot/cgb_boot_fast.bin"), Model::Cgb0)
        }
        else {
            (include_bytes!("../../bootrom/dmg/dmg.bin"), Model::DmgB)
        };
        Box::new(GameBoyColor::new_from_rom(rom, bios, None, model))
    }
}

fn scripted_input(frame: u64) -> Input {
    let mut input = Input::new();
    input.start = (frame / 30) % 4 == 0;
    input.a = (frame / 30) % 4 == 2;
    input.d_right = (frame / 7) % 3 == 1;
    input
}

fn run_frame(core: &mut SuperShuckieCore, frame: u64) {
    core.enqueue_input(scripted_input(frame));
    let target = core.total_frames() + 1;
    while core.total_frames() < target {
        core.run_unlocked();
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let rom_path = args.next().expect("usage: load_preview_check <rom> [--frames n]");
    let mut frames: u64 = 1200;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--frames" => frames = args.next().expect("--frames n").parse().expect("frames"),
            other => panic!("unexpected argument {other}")
        }
    }
    let rom = std::fs::read(&rom_path).expect("read rom");
    let gba = rom_path.to_ascii_lowercase().ends_with(".gba");

    let mut plain = SuperShuckieCore::new(new_core(&rom, gba), std_timestamp_provider());
    let mut previewed = SuperShuckieCore::new(new_core(&rom, gba), std_timestamp_provider());
    // One starting point for both (the Game Boy randomises RAM at boot).
    let boot = plain.create_save_state();
    previewed.load_save_state(&boot);
    plain.load_save_state(&boot);

    // Play a while, take S, play on so that both are somewhere else when S is loaded.
    for frame in 0..600 {
        run_frame(&mut plain, frame);
        run_frame(&mut previewed, frame);
    }
    let s = plain.create_save_state();
    for frame in 600..900 {
        run_frame(&mut plain, frame);
        run_frame(&mut previewed, frame);
    }
    assert_eq!(plain.create_save_state(), previewed.create_save_state(), "both cores agree before the load");

    // The previewed core records (so it is capturing) while loading S; the plain one just loads.
    previewed.start_recording_replay(PartialReplayRecordMetadata {
        rom_name: "check".into(),
        rom_filename: "check".into(),
        settings: ReplayFileRecorderSettings::default(),
        patch_format: ReplayPatchFormat::Unpatched,
        patch_target_checksum: Default::default(),
        patch_data: ByteVec::new(),
        frames_per_keyframe: NonZeroU64::new(120).unwrap(),
        final_file: Vec::new(),
        temp_file: Vec::new()
    }).expect("start recording");
    let serial_before = previewed.run_serial();
    let frames_before = previewed.total_frames();
    previewed.load_save_state(&s);
    assert_ne!(previewed.run_serial(), serial_before, "the preview marks the screens drawn");
    assert!(previewed.last_frame_presented());
    assert_eq!(previewed.total_frames(), frames_before, "the preview frame does not count");
    plain.load_save_state(&s);

    let mut screen_mismatch_frames = 0u64;
    let mut state_checked = 0u64;
    for frame in 0..frames {
        run_frame(&mut plain, frame + 1000);
        run_frame(&mut previewed, frame + 1000);
        if frame % 60 == 0 {
            state_checked += 1;
            if plain.create_save_state() != previewed.create_save_state() {
                println!("FAIL: the save states differ {frame} frames after the load");
                std::process::exit(1);
            }
        }
        let a = plain.get_core().get_screens();
        let b = previewed.get_core().get_screens();
        if a.iter().zip(b.iter()).any(|(x, y)| x.pixels != y.pixels) {
            screen_mismatch_frames += 1;
        }
    }
    assert_eq!(previewed.stop_recording_replay(), Some(true));
    println!(
        "OK: {} â€” save states identical at {state_checked} checkpoints over {frames} frames after loading S with a recorder attached; {screen_mismatch_frames} frames drew differently",
        if gba { "GBA" } else { "GB/GBC" }
    );
    if screen_mismatch_frames > 0 {
        std::process::exit(1);
    }
}
