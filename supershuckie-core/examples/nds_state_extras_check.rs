//! Checks that a Nintendo DS save state carries what melonDS's own leave out (the SHUK section,
//! see melonds-rs/interface.cpp): a console that booted the game (so its firmware holds the WFC
//! ID the game wrote) saves a state; a fresh console loads it; both reset the console and run the
//! same frames. With the section they must end identical; with it stripped (a state from before),
//! the fresh console's game writes an ID of its own and they part ways.
//!
//! ```text
//! nds_state_extras_check <rom.nds> [boot frames] [frames after the reset]
//! ```

use supershuckie_core::emulator::{EmulatorCore, NintendoDS};
use supershuckie_core::std_timestamp_provider;

fn section(state: &[u8], magic: &[u8; 4]) -> Option<(usize, usize)> {
    let mut offset = 16usize;
    while offset + 16 <= state.len() {
        let len = u32::from_le_bytes(state[offset + 4..offset + 8].try_into().unwrap()) as usize;
        if len < 16 || offset + len > state.len() {
            return None;
        }
        if &state[offset..offset + 4] == magic {
            return Some((offset, len));
        }
        offset += len;
    }
    None
}

/// `state` as a build from before SHUK would have made it.
fn strip_extras(state: &[u8]) -> Vec<u8> {
    let (at, len) = section(state, b"SHUK").expect("the state has no SHUK section");
    assert_eq!(at + len, state.len(), "SHUK is not the last section");
    let mut old = state[..at].to_vec();
    let total = old.len() as u32;
    old[8..12].copy_from_slice(&total.to_le_bytes());
    old
}

/// Sections that differ between two states, with differing byte counts.
fn differing_sections(a: &[u8], b: &[u8]) -> Vec<String> {
    if a.len() != b.len() {
        return vec![format!("length {} vs {}", a.len(), b.len())];
    }
    let mut out = Vec::new();
    let mut offset = 16usize;
    while offset + 16 <= a.len() {
        let len = u32::from_le_bytes(a[offset + 4..offset + 8].try_into().unwrap()) as usize;
        if len < 16 || offset + len > a.len() {
            break;
        }
        let n = (offset..offset + len).filter(|&i| a[i] != b[i]).count();
        if n > 0 {
            out.push(format!("{}:{n}", String::from_utf8_lossy(&a[offset..offset + 4])));
        }
        offset += len;
    }
    out
}

fn run(core: &mut NintendoDS, frames: u64) {
    for i in 0..frames {
        // Some input so the game does something: tap A every 30 frames.
        let input = if i % 30 < 3 { 1u32 } else { 0 };
        core.set_input_encoded(&input.to_le_bytes());
        core.run_unlocked();
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let rom_path = args.next().expect("usage: nds_state_extras_check <rom.nds> [boot frames] [frames after the reset]");
    let boot: u64 = args.next().map(|s| s.parse().expect("boot frames")).unwrap_or(900);
    let after: u64 = args.next().map(|s| s.parse().expect("frames")).unwrap_or(600);
    let rom = std::fs::read(&rom_path).expect("read rom");
    let new_core = || {
        let mut core = NintendoDS::new_from_rom(&rom, None, std_timestamp_provider(), false).expect("load rom");
        core.set_skip_drawing(true);
        core
    };

    let mut a = new_core();
    let pristine = a.debug_firmware().to_vec();
    run(&mut a, boot);
    let state = a.create_save_state();
    let firmware_written = a.debug_firmware() != pristine.as_slice();
    println!("booted {boot} frames: firmware {}; state {} bytes", if firmware_written { "written by the game" } else { "untouched" }, state.len());

    // Round trip: a fresh console loading the state saves the same SHUK.
    let mut b = new_core();
    b.load_save_state(&state).expect("load");
    let again = b.create_save_state();
    let shuk_a = section(&state, b"SHUK").map(|(o, l)| state[o..o + l].to_vec());
    let shuk_b = section(&again, b"SHUK").map(|(o, l)| again[o..o + l].to_vec());
    println!("SHUK after a load into a fresh console: {}", if shuk_a == shuk_b { "identical" } else { "DIFFERENT" });
    println!("firmware after the load: {}", if b.debug_firmware() == a.debug_firmware() { "the saving console's" } else { "DIFFERENT" });

    // Both reset and run the same frames.
    a.hard_reset();
    run(&mut a, after);
    b.hard_reset();
    run(&mut b, after);
    let diff = differing_sections(&a.create_save_state(), &b.create_save_state());
    println!("{after} frames after a reset, with SHUK: {}", if diff.is_empty() { "identical".to_string() } else { format!("DIFFERENT {}", diff.join(" ")) });

    // The same from a state without SHUK (as made before): the control.
    let mut a2 = new_core();
    a2.load_save_state(&state).expect("load");
    let mut c = new_core();
    c.load_save_state(&strip_extras(&state)).expect("load stripped");
    a2.hard_reset();
    run(&mut a2, after);
    c.hard_reset();
    run(&mut c, after);
    let diff = differing_sections(&a2.create_save_state(), &c.create_save_state());
    println!("{after} frames after a reset, state without SHUK: {}", if diff.is_empty() { "identical".to_string() } else { format!("differ {}", diff.join(" ")) });
}
