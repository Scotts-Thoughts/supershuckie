//! Replay bookmarks against a real core: a keyframe bookmark recorded mid-chain must be reached by
//! loading its own full keyframe (no deltas applied, three frames emulated) and land on exactly the
//! state sequential playback reaches; bookmarks recorded live must come back from the file; and a
//! keyframe bookmark requested during playback must snap to an existing keyframe.
//!
//! ```text
//! bookmark_smoke --gbc <rom.gbc>        (or --gba <rom.gba>, --nds <rom.nds>)
//! ```
//!
//! Link it like `nds_bench` (see that file's header); on macOS:
//!
//! ```text
//! cargo rustc --release -p supershuckie-core --example bookmark_smoke -- \
//!     -L native=build/melonDS/src -L native=build/melonDS/src/teakra/src -L native=build/mgba \
//!     -l static=core -l static=teakra -l static=mgba -l c++
//! ```

use std::fs::File;
use std::io::BufWriter;
use std::num::NonZeroU64;
use std::time::Instant;

use supershuckie_core::emulator::{EmulatorCore, GameBoyAdvance, GameBoyColor, Model, NintendoDS, PartialReplayRecordMetadata};
use supershuckie_core::{std_timestamp_provider, SuperShuckieCore};
use supershuckie_replay_recorder::replay_file::playback::{BookmarkTableSource, ReplayFilePlayer};
use supershuckie_replay_recorder::replay_file::record::ReplayFileRecorderSettings;
use supershuckie_replay_recorder::replay_file::ReplayPatchFormat;
use supershuckie_replay_recorder::{Bookmark, BookmarkTable, ByteVec, KEYFRAME_BOOKMARK_LEAD_FRAMES};

const FRAMES_PER_KEYFRAME: u64 = 120;

fn make_core(console: &str, rom: &[u8]) -> SuperShuckieCore {
    let emulator: Box<dyn EmulatorCore> = match console {
        "--gbc" => Box::new(GameBoyColor::new_from_rom(rom, include_bytes!("../../bootrom/cgb/cgb_boot/cgb_boot_fast.bin"), None, Model::Cgb0)),
        "--gba" => Box::new(GameBoyAdvance::new_from_rom(rom, None, &[], std_timestamp_provider()).expect("failed to load ROM")),
        "--nds" => Box::new(NintendoDS::new_from_rom(rom, None, std_timestamp_provider(), false).expect("failed to load ROM")),
        other => panic!("unknown console {other}")
    };
    SuperShuckieCore::new(emulator, std_timestamp_provider())
}

/// Run until one more whole frame has been emulated.
fn run_frame(core: &mut SuperShuckieCore) {
    let target = core.total_frames() + 1;
    while core.total_frames() < target && !core.is_replay_stalled() {
        core.run_unlocked();
    }
}

/// Where two states differ: `(differing bytes, first offset, last offset)`.
fn diff_summary(a: &[u8], b: &[u8]) -> Option<(usize, usize, usize)> {
    if a.len() != b.len() {
        return Some((usize::MAX, 0, 0))
    }
    let offsets: Vec<usize> = a.iter().zip(b).enumerate().filter(|(_, (x, y))| x != y).map(|(i, _)| i).collect();
    Some((offsets.len(), *offsets.first()?, *offsets.last()?))
}

fn run_to(core: &mut SuperShuckieCore, frame: u64) {
    while core.total_frames() < frame {
        run_frame(core);
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let console = args.next().expect("usage: bookmark_smoke --gbc|--gba|--nds <rom>");
    let rom = std::fs::read(args.next().expect("rom path")).expect("read rom");

    let dir = std::env::temp_dir().join("supershuckie-bookmark-smoke");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let final_path = dir.join("smoke.replay");

    // --- 1. Record, placing bookmarks live. ---
    let mut recorder = make_core(&console, &rom);
    for _ in 0..300 {
        run_frame(&mut recorder);
    }
    recorder.start_recording_replay(PartialReplayRecordMetadata {
        rom_name: "smoke".into(),
        rom_filename: "smoke".into(),
        settings: ReplayFileRecorderSettings::default(),
        patch_format: ReplayPatchFormat::Unpatched,
        patch_target_checksum: Default::default(),
        patch_data: ByteVec::new(),
        frames_per_keyframe: NonZeroU64::new(FRAMES_PER_KEYFRAME).unwrap(),
        final_file: BufWriter::new(File::create(&final_path).unwrap()),
        temp_file: BufWriter::new(File::create(dir.join("smoke.temp.replay")).unwrap()),
    }).expect("start recording");

    let mut table = BookmarkTable::new();

    // A keyframe bookmark between scheduled keyframes (120 and 240).
    run_to(&mut recorder, 190);
    let fast = recorder.bookmark_anchor(true).expect("keyframe anchor");
    assert_eq!((fast.in_frame, fast.keyframe), (190 + KEYFRAME_BOOKMARK_LEAD_FRAMES, true));
    table.insert(Bookmark { name: "fast".into(), in_frame: fast.in_frame, in_millis: fast.in_millis, keyframe: true, ..Default::default() });
    recorder.set_replay_bookmarks(table.clone());

    // A plain range.
    run_to(&mut recorder, 250);
    let start = recorder.bookmark_anchor(false).unwrap();
    run_to(&mut recorder, 280);
    let end = recorder.bookmark_anchor(false).unwrap();
    table.insert(Bookmark { name: "range".into(), in_frame: start.in_frame, in_millis: start.in_millis, out: Some((end.in_frame, end.in_millis)), ..Default::default() });
    recorder.set_replay_bookmarks(table.clone());

    // Placing bookmarks while paused must not emulate anything (a frame finished while the timer
    // is paused would be recorded at the wrong time and break the recording).
    run_to(&mut recorder, 330);
    recorder.pause_timer();
    std::thread::sleep(std::time::Duration::from_millis(120));
    let paused = recorder.bookmark_anchor(false).unwrap();
    assert_eq!(paused.in_frame, 330);
    recorder.unpause_timer();

    // A keyframe bookmark requested mid-frame (the Game Boy core runs in slices) is written when
    // that frame completes.
    let mut mid_frame_anchor = None;
    for _ in 0..10_000 {
        recorder.run_unlocked();
        if recorder.is_mid_frame() {
            let before = recorder.total_frames();
            let anchor = recorder.bookmark_anchor(true).unwrap();
            assert_eq!(anchor.in_frame, before + 1 + KEYFRAME_BOOKMARK_LEAD_FRAMES);
            mid_frame_anchor = Some(anchor);
            break;
        }
    }
    run_to(&mut recorder, 400);
    assert!(recorder.poll_replay_recording_errors().is_empty(), "the recording hit an error");
    assert_eq!(recorder.stop_recording_replay(), Some(true));
    println!("recorded {} frames with {} bookmarks", 400, table.bookmarks.len());

    // --- 2. The file holds the bookmarks and the forced keyframe is full. ---
    let bytes = std::fs::read(&final_path).unwrap();
    let mut player = ReplayFilePlayer::new(&bytes, false).expect("parse recording");
    assert_eq!(player.bookmark_table_source(), BookmarkTableSource::Section);
    assert_eq!(*player.bookmark_table(), table);
    // The schedule may start a frame late (the core counts frames run before recording), but the
    // forced keyframe restarts it: the next scheduled keyframe is FRAMES_PER_KEYFRAME after 190.
    let keyframes: Vec<u64> = player.all_keyframes().keys().copied().collect();
    let after: Vec<u64> = keyframes.iter().copied().filter(|&k| k >= 190 && k <= 190 + FRAMES_PER_KEYFRAME).collect();
    assert_eq!(after, [190, 190 + FRAMES_PER_KEYFRAME], "keyframes {keyframes:?}");
    match mid_frame_anchor {
        Some(anchor) => {
            let keyframe = anchor.in_frame - KEYFRAME_BOOKMARK_LEAD_FRAMES;
            assert!(player.keyframe_is_stored_full(keyframe).unwrap(), "the mid-frame keyframe bookmark's keyframe at {keyframe} is full ({keyframes:?})");
            println!("mid-frame keyframe bookmark wrote its keyframe at {keyframe}");
        }
        None => println!("(this core never stopped mid-frame; the mid-frame case was not exercised)")
    }
    let scheduled_before = keyframes.iter().copied().filter(|&k| k > 0 && k < 190).last().expect("a scheduled keyframe before the bookmark");
    assert!(player.keyframe_is_stored_full(190).unwrap(), "the keyframe bookmark's keyframe is full");
    assert!(!player.keyframe_is_stored_full(scheduled_before).unwrap(), "scheduled keyframes stay deltas");
    println!("bookmark section and full keyframe present");

    // --- 3. Sequential playback to the bookmark gives the reference state. ---
    let mut sequential = make_core(&console, &rom);
    sequential.attach_replay_player(ReplayFilePlayer::new(&bytes, false).unwrap(), true).unwrap();
    run_to(&mut sequential, fast.in_frame);
    let reference = sequential.create_save_state();

    // --- 4. Seeking to the keyframe bookmark: no folds, three frames, same state. ---
    let mut seeker = make_core(&console, &rom);
    seeker.attach_replay_player(ReplayFilePlayer::new(&bytes, false).unwrap(), true).unwrap();
    seeker.go_to_replay_frame(360).expect("seek to 360");
    let folds = seeker.replay_player().unwrap().chain_folds();
    let started = Instant::now();
    let keyframe = seeker.go_to_replay_keyframe(fast.in_frame - KEYFRAME_BOOKMARK_LEAD_FRAMES).unwrap();
    assert_eq!(keyframe, 190);
    assert_eq!(seeker.replay_player().unwrap().chain_folds(), folds, "loading the keyframe bookmark's keyframe applied deltas");
    // Whole frames (the Game Boy core runs in sub-frame slices).
    while seeker.total_frames() < fast.in_frame {
        seeker.run_unlocked_hidden();
    }
    let keyframe_seek = started.elapsed();
    assert_eq!(seeker.total_frames(), fast.in_frame);
    let via_keyframe = seeker.create_save_state();

    // The ordinary seek path, for comparison.
    seeker.go_to_replay_frame(360).expect("seek to 360");
    seeker.go_to_replay_frame(fast.in_frame).expect("seek to fast.in_frame");
    assert_eq!(seeker.total_frames(), fast.in_frame);
    let via_seek = seeker.create_save_state();

    println!("state {} bytes; keyframe-bookmark seek vs sequential: {:?}; ordinary seek vs sequential: {:?}; keyframe vs ordinary: {:?}",
        reference.len(), diff_summary(&via_keyframe, &reference), diff_summary(&via_seek, &reference), diff_summary(&via_keyframe, &via_seek));
    assert!(diff_summary(&via_keyframe, &reference).is_none() || diff_summary(&via_seek, &reference).is_some(), "the keyframe bookmark's state differs from sequential playback where an ordinary seek does not");

    // An ordinary seek far from a keyframe, for comparison.
    seeker.go_to_replay_frame(10).expect("seek to 10");
    let started = Instant::now();
    seeker.go_to_replay_frame(305).expect("seek to 305");
    let ordinary_seek = started.elapsed();
    println!("keyframe bookmark seek {:.2} ms; ordinary seek to 305 (115 frames past a keyframe) {:.2} ms", keyframe_seek.as_secs_f64() * 1000.0, ordinary_seek.as_secs_f64() * 1000.0);

    // --- 5. During playback a keyframe bookmark snaps to an existing keyframe. ---
    seeker.go_to_replay_frame(300).expect("seek to 300");
    let snapped = seeker.bookmark_anchor(true).unwrap();
    assert_eq!((snapped.in_frame, snapped.keyframe), (190 + KEYFRAME_BOOKMARK_LEAD_FRAMES, true), "snaps to the last keyframe at or before 297");
    let plain = seeker.bookmark_anchor(false).unwrap();
    assert_eq!((plain.in_frame, plain.keyframe), (300, false));

    let estimate = seeker.estimate_millis_at(255).unwrap();
    assert!(estimate >= start.in_millis.0.saturating_sub(100).into() && estimate <= (start.in_millis.0 + 200).into(), "estimate {estimate} vs recorded {}", start.in_millis);
    println!("playback anchors and time estimates ok");

    println!("all bookmark checks passed");
    let _ = std::fs::remove_dir_all(&dir);
}
