//! Records a stretch of a Nintendo DS replay again from the emulator's own states, the way a
//! live recording is made (a state every 120 frames into a `ReplayFileRecorder`, with the ROM),
//! and checks what comes out: research and regression tool for format v11 (ROM references) and
//! for keyframes that keep their regenerated output buffers.
//!
//! ```text
//! nds_rerecord_lab <rom.nds> <file.replay> <out.replay> [--start F] [--keyframes N] [--no-rom] [--no-keep]
//! ```
//!
//! `--no-keep` records as builds before 0.5.4 did: every delta keyframe leaves the regenerated
//! output buffers out, whether the game redrew them or not (for comparing sizes).
//!
//! Every keyframe read back from the output must equal the state it was made from outside the
//! regenerated output buffers (the 3D geometry banks), and inside them too when the reader says
//! the keyframe is exact. Prints how many keyframes are exact and the file's size.
//!
//! Link it like `nds_bench` (see that file's header).

use std::sync::Arc;

use supershuckie_core::emulator::NintendoDS;
use supershuckie_core::{std_timestamp_provider, SuperShuckieCore};
use supershuckie_replay_recorder::blake3_hash;
use supershuckie_replay_recorder::keyframe_masks::transient_ranges;
use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::replay_file::record::{ReplayFileRecorder, ReplayFileRecorderSettings};
use supershuckie_replay_recorder::replay_file::ReplayConsoleType;
use supershuckie_replay_recorder::{ByteVec, InputBuffer, Packet, Speed};

fn main() {
    let mut args = std::env::args().skip(1);
    let usage = "usage: nds_rerecord_lab <rom.nds> <file.replay> <out.replay> [--start F] [--keyframes N] [--no-rom] [--no-keep]";
    let rom_path = args.next().expect(usage);
    let replay_path = args.next().expect(usage);
    let out_path = args.next().expect(usage);
    let (mut start, mut keyframes, mut with_rom, mut keep) = (0u64, 300u64, true, true);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--start" => start = args.next().unwrap().parse().unwrap(),
            "--keyframes" => keyframes = args.next().unwrap().parse().unwrap(),
            "--no-rom" => with_rom = false,
            "--no-keep" => keep = false,
            other => panic!("unknown argument {other}"),
        }
    }

    let rom = Arc::new(std::fs::read(&rom_path).expect("read rom"));
    let bytes = Arc::new(std::fs::read(&replay_path).expect("read replay"));
    let mut player = ReplayFilePlayer::new_shared(bytes.clone(), false).expect("parse replay");
    if player.wants_rom() {
        player.set_rom(rom.clone());
    }
    let metadata = player.get_replay_metadata().clone();
    let nds = NintendoDS::new_from_rom(&rom, None, std_timestamp_provider(), false).expect("load ROM");
    let mut core = SuperShuckieCore::new(Box::new(nds), std_timestamp_provider());
    core.set_replay_seek_index_budget(0);
    core.attach_replay_player(player, true).expect("attach");
    // Far enough past a keyframe that the geometry banks are the game's own again.
    core.go_to_replay_frame(start.max(1) + 60).expect("seek");

    // (hash outside the banks, hash of the banks) of every state handed to the recorder.
    let fingerprint = |state: &[u8]| -> ([u8; 32], [u8; 32]) {
        let range = transient_ranges(ReplayConsoleType::NintendoDS, state).into_iter().next().expect("a melonDS state");
        let mut outside = state.to_vec();
        outside[range.clone()].fill(0);
        (blake3_hash(&outside), blake3_hash(&state[range]))
    };

    let mut state = Vec::new();
    core.get_core().create_save_state_into(&mut state);
    let mut made = vec![fingerprint(&state)];
    let mut recorder = ReplayFileRecorder::new_with_metadata(
        metadata,
        ByteVec::new(),
        ReplayFileRecorderSettings::default(),
        0u64.into(),
        InputBuffer::new(),
        Speed::default(),
        ByteVec::Heap(state.clone()),
        Vec::<u8>::new(),
        Vec::<u8>::new(),
    )
    .expect("create recorder");
    if with_rom {
        recorder.set_rom(rom.clone());
    }

    let mut millis = 0u64;
    for _ in 0..keyframes {
        for _ in 0..120 {
            core.run_unlocked_hidden();
            millis += 16;
            recorder.next_frame(millis.into()).expect("next frame");
        }
        if core.is_replay_stalled() {
            break;
        }
        core.get_core().create_save_state_into(&mut state);
        made.push(fingerprint(&state));
        recorder.next_keyframe_has_stale_transients(!keep);
        recorder.insert_keyframe(ByteVec::Heap(state.clone()), millis.into()).expect("insert keyframe");
    }
    let (out, _) = recorder.close().map_err(|(_, _, e)| e).expect("close");
    std::fs::write(&out_path, &out).expect("write output");

    let mut check = ReplayFilePlayer::new(&out, false).expect("parse output");
    if check.wants_rom() {
        check.set_rom(rom.clone());
    }
    check.set_keyframe_states_wanted(false);
    let (mut exact, mut stale, mut wrong) = (0u64, 0u64, 0u64);
    let mut index = 0usize;
    while let Some(packet) = check.next_packet().expect("read output") {
        let Packet::Keyframe { metadata, .. } = packet else { continue };
        let frame = metadata.elapsed_frames;
        let is_stale = check.current_keyframe_has_stale_transients();
        let (outside, banks) = fingerprint(check.current_keyframe_state());
        let expected = made[index];
        if outside != expected.0 {
            wrong += 1;
            println!("keyframe {index} (frame {frame}): state DIFFERS outside the geometry banks");
        }
        if is_stale {
            stale += 1;
        }
        else {
            exact += 1;
            if banks != expected.1 {
                wrong += 1;
                println!("keyframe {index} (frame {frame}): said to be exact, but its geometry banks DIFFER");
            }
        }
        index += 1;
    }
    assert_eq!(index, made.len(), "every keyframe is read back");
    println!(
        "{} keyframes from frame {} on: {} bytes ({:.1} MiB per hour); {exact} exact, {stale} left the geometry banks stale; {}",
        made.len(),
        start.max(1) + 60,
        out.len(),
        out.len() as f64 / 1048576.0 / ((made.len() - 1) as f64 * 120.0 / 215_374.0),
        if wrong == 0 { "all read back as recorded".to_owned() } else { format!("{wrong} WRONG") }
    );
    if wrong != 0 {
        std::process::exit(1);
    }
}
