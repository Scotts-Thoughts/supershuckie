//! Smoke test and bench for the Nintendo 3DS core (Azahar) inside `SuperShuckieCore`: record a
//! replay with 170 MB keyframes, play it back, seek, and report what each step cost.
//!
//! ```text
//! n3ds_smoke <game.cci> [--frames <n>] [--keyframe <frames>] [--user-dir <dir>] [--out <dir>]
//! ```
//!
//! Azahar's core is a singleton, so one core is reused for recording, playback and seeking.
//! Links through the `link-cores` dev-feature (build/azahar/libazahar.a from
//! scripts/build-azahar-spike.ps1).

use std::fs::File;
use std::io::BufWriter;
use std::num::NonZeroU64;
use std::time::Instant;

use supershuckie_core::emulator::{EmulatorCore, Input, Nintendo3DS, Nintendo3DSSettings, PartialReplayRecordMetadata};
use supershuckie_core::{std_timestamp_provider, SuperShuckieCore};
use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::replay_file::record::ReplayFileRecorderSettings;
use supershuckie_replay_recorder::replay_file::ReplayPatchFormat;
use supershuckie_replay_recorder::ByteVec;

fn heap_hash(core: &SuperShuckieCore) -> u64 {
    // FNV over the process heap (what the sync hash covers).
    let heap = core.get_core().memory_region_data(0).unwrap_or(&[]);
    let mut h = 0xcbf29ce484222325u64;
    for chunk in heap.chunks(8) {
        let mut w = [0u8; 8];
        w[..chunk.len()].copy_from_slice(chunk);
        h ^= u64::from_le_bytes(w);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// A fixed input pattern: A twice a second, a direction held for two seconds, cycling.
fn scripted(frame: u64) -> Input {
    let mut i = Input::new();
    i.a = frame % 30 < 4;
    let leg = (frame / 120) % 8;
    let walking = frame % 120 < 100;
    i.d_down = walking && leg == 1;
    i.d_right = walking && leg == 3;
    i.d_up = walking && leg == 5;
    i.d_left = walking && leg == 7;
    i
}

fn main() {
    let mut args = std::env::args().skip(1);
    let rom_path = args.next().expect("usage: n3ds_smoke <game.cci> [--frames n] [--keyframe frames] [--user-dir dir] [--out dir]");
    let mut frames = 2400u64;
    let mut keyframe = 480u64;
    let mut user_dir = std::env::temp_dir().join("supershuckie-n3ds-smoke").join("user");
    let mut out_dir = std::env::temp_dir().join("supershuckie-n3ds-smoke");
    while let Some(a) = args.next() {
        match a.as_str() {
            "--frames" => frames = args.next().unwrap().parse().unwrap(),
            "--keyframe" => keyframe = args.next().unwrap().parse().unwrap(),
            "--user-dir" => user_dir = args.next().unwrap().into(),
            "--out" => out_dir = args.next().unwrap().into(),
            other => panic!("unknown option {other}")
        }
    }
    std::fs::create_dir_all(&out_dir).expect("out dir");
    std::fs::create_dir_all(&user_dir).expect("user dir");
    let rom = std::fs::read(&rom_path).expect("read rom");

    let t = Instant::now();
    let n3ds = Nintendo3DS::new_from_path(&rom_path, &rom, user_dir.to_str().unwrap(), std_timestamp_provider(), &Nintendo3DSSettings::default())
        .expect("failed to load the game");
    let mut core = SuperShuckieCore::new(Box::new(n3ds), std_timestamp_provider());
    println!("loaded in {:.0} ms", t.elapsed().as_secs_f64() * 1000.0);

    // Boot to the title screen.
    let t = Instant::now();
    for _ in 0..300 {
        core.run_unlocked();
    }
    println!("boot 300 frames: {:.0} ms", t.elapsed().as_secs_f64() * 1000.0);

    // --- 1. Record. Keyframes every `keyframe` frames; time each frame to see the stalls. ---
    let final_path = out_dir.join("n3ds.replay");
    let temp_path = out_dir.join("n3ds.temp.replay");
    core.start_recording_replay(PartialReplayRecordMetadata {
        rom_name: "n3ds smoke".into(),
        rom_filename: "game.cci".into(),
        settings: ReplayFileRecorderSettings::default(),
        patch_format: ReplayPatchFormat::Unpatched,
        patch_target_checksum: Default::default(),
        patch_data: ByteVec::new(),
        frames_per_keyframe: NonZeroU64::new(keyframe).unwrap(),
        final_file: BufWriter::with_capacity(8 << 20, File::create(&final_path).expect("create final")),
        temp_file: BufWriter::with_capacity(8 << 20, File::create(&temp_path).expect("create temp")),
    }).expect("start recording");

    let t = Instant::now();
    let mut slow_frames = Vec::new();
    let start_frame = core.total_frames();
    for k in 0..frames {
        core.enqueue_input(scripted(k));
        let f = Instant::now();
        core.run_unlocked();
        let ms = f.elapsed().as_secs_f64() * 1000.0;
        if ms > 20.0 {
            slow_frames.push((k, ms));
        }
    }
    let rec_ms = t.elapsed().as_secs_f64() * 1000.0;
    let recorded_hash = heap_hash(&core);
    let end_frame = core.total_frames();
    let t = Instant::now();
    assert_eq!(core.stop_recording_replay(), Some(true), "recording closed cleanly");
    let close_ms = t.elapsed().as_secs_f64() * 1000.0;
    let size = std::fs::metadata(&final_path).map(|m| m.len()).unwrap_or(0);
    println!("recorded {frames} frames in {rec_ms:.0} ms ({:.0} fps); close {close_ms:.0} ms; file {:.1} MB ({:.2} MB per keyframe interval)",
             frames as f64 * 1000.0 / rec_ms, size as f64 / 1e6, size as f64 / 1e6 / (frames as f64 / keyframe as f64));
    println!("frames over 20 ms (keyframe stalls): {}", slow_frames.iter().map(|(k, ms)| format!("{k}:{ms:.0}ms")).collect::<Vec<_>>().join(" "));

    // --- 2. Play it back on the same core (singleton); the end heap must match. ---
    let bytes = std::fs::read(&final_path).expect("read recording");
    let t = Instant::now();
    let mut player = ReplayFilePlayer::new(&bytes, false).expect("parse recording");
    println!("parsed the file in {:.0} ms: {} frames, {} keyframes", t.elapsed().as_secs_f64() * 1000.0, player.get_total_frames(), player.all_keyframes().len());
    let thumb = player.thumbnail_at_or_before(1000);
    println!("thumbnails: {}; at or before frame 1000: {}", if player.has_thumbnails() { "yes" } else { "none" },
             thumb.as_ref().map(|t| format!("frame {} top {}x{} bottom {}x{}", t.frame, t.top.0, t.top.1, t.bottom.0, t.bottom.1)).unwrap_or_else(|| "none".into()));
    player.enable_threading();
    core.attach_replay_player(player, true).expect("attach");
    let t = Instant::now();
    while !core.is_replay_stalled() {
        core.run_unlocked();
    }
    println!("playback to the end in {:.0} ms; frames {} (recorded {}..{})", t.elapsed().as_secs_f64() * 1000.0, core.total_frames(), start_frame, end_frame);
    let playback_hash = heap_hash(&core);
    println!("playback end heap {} recording end heap", if playback_hash == recorded_hash { "matches" } else { "DIFFERS from" });

    // --- 3. Seeks: to a keyframe, into the middle of an interval, back, and repeated. ---
    let targets = [
        start_frame + keyframe,
        start_frame + keyframe + keyframe / 2,
        start_frame + 3 * keyframe + 17,
        start_frame + keyframe + keyframe / 2,
        end_frame.saturating_sub(5),
    ];
    let mut hashes = Vec::new();
    for target in targets {
        let t = Instant::now();
        core.go_to_replay_frame(target).expect("seek");
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(core.total_frames(), target);
        hashes.push(heap_hash(&core));
        println!("seek to {target}: {ms:.0} ms");
    }
    println!("seek to the same frame twice: {}", if hashes[1] == hashes[3] { "identical heap" } else { "DIFFERENT heap" });
    println!("done");
}
