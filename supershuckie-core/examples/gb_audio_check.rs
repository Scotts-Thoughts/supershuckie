//! Proves that audio does not change Game Boy emulation: two `GameBoyColor` cores boot the same
//! ROM from one snapshot and are stepped in lockstep with the same (scripted) input, one with
//! audio on and drained every step, one with audio off; their save states must stay
//! byte-identical (checked every `--every` frames), and the audible one must keep producing
//! samples.
//!
//! ```text
//! cargo run --release -p supershuckie-core --example gb_audio_check -- <rom.gb|gbc> [--frames n] [--every n]
//! ```
//!
//! Needs no extra link arguments (SameBoy comes with the `safeboy` crate). See the audio notes on
//! `GameBoyColor` for why the emulated instance itself must never get a sample rate; the
//! `gb_audio_probe` example demonstrates the SameBoy quirk directly.

use supershuckie_core::emulator::{EmulatorCore, GameBoyColor, Input, Model};

fn main() {
    let mut args = std::env::args().skip(1);
    let rom_path = args.next().expect("usage: gb_audio_check <rom> [--frames n] [--every n]");
    let mut frames: u64 = 3000;
    let mut every: u64 = 60;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--frames" => frames = args.next().expect("--frames n").parse().expect("frames"),
            "--every" => every = args.next().expect("--every n").parse().expect("every"),
            other => panic!("unexpected argument {other}"),
        }
    }

    let rom = std::fs::read(&rom_path).expect("read rom");
    let cgb = rom.get(0x143).map(|b| b & 0x80 != 0).unwrap_or(false);
    let (bios, model): (&[u8], Model) = if cgb {
        (include_bytes!("../../bootrom/cgb/cgb_boot/cgb_boot_fast.bin"), Model::Cgb0)
    } else {
        (include_bytes!("../../bootrom/dmg/dmg.bin"), Model::DmgB)
    };

    let mut silent = GameBoyColor::new_from_rom(&rom, bios, None, model);
    let mut audible = GameBoyColor::new_from_rom(&rom, bios, None, model);
    // SameBoy randomises initial RAM (and RTC time is the host's), so start both from one state
    // the way a replay does.
    let boot = silent.create_save_state();
    audible.load_save_state(&boot).expect("load boot state");
    audible.set_audio_enabled(true);

    let mut scratch = Vec::new();
    let mut encoded = Vec::new();
    let mut total_samples: u64 = 0;
    let mut checked = 0u64;
    let mut frame = 0u64;
    let mut samples_at_last_report = 0u64;

    // Mash Start/A every so often so the game leaves its title screen and makes some noise.
    while frame < frames {
        let mut input = Input::new();
        input.start = (frame / 30) % 4 == 0;
        input.a = (frame / 30) % 4 == 2;
        encoded.clear();
        silent.encode_input(input, &mut encoded);
        silent.set_input_encoded(&encoded);
        audible.set_input_encoded(&encoded);

        // GB_run steps less than a frame; both wrappers step their emulated instance identically.
        let a = silent.run_unlocked();
        let b = audible.run_unlocked();
        assert_eq!(a.frames, b.frames, "cores stepped differently at frame {frame}");
        audible.take_audio(&mut scratch);
        total_samples += (scratch.len() / 2) as u64;
        scratch.clear();

        if a.frames > 0 {
            frame += a.frames;
            if frame % 60 == 0 {
                let got = total_samples - samples_at_last_report;
                samples_at_last_report = total_samples;
                if got < 60 * 780 || got > 60 * 830 {
                    eprintln!("frame {frame}: {got} samples in the last 60 frames ({:.1} per frame), resyncs so far {}", got as f64 / 60.0, audible.audio_resyncs());
                }
            }
            if frame % every == 0 {
                let sa = silent.create_save_state();
                let sb = audible.create_save_state();
                if sa != sb {
                    let first = sa.iter().zip(&sb).position(|(x, y)| x != y);
                    panic!("DESYNC at frame {frame}: states differ ({} vs {} bytes, first at {first:?})", sa.len(), sb.len());
                }
                checked += 1;
            }
        }
    }

    println!(
        "{rom_path}: {frame} frames, {checked} state comparisons identical; {total_samples} stereo frames of audio ({:.1} per frame; ~803 expected at 48 kHz), audio shadow resynced {} time(s)",
        total_samples as f64 / frame.max(1) as f64,
        audible.audio_resyncs()
    );
}
