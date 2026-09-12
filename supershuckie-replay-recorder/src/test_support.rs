//! Shared helpers for the crate's unit tests: a deterministic synthetic replay "script" that the
//! recorder/player round-trip tests, the v3 compatibility-fixture test and the converter tests all
//! build their expectations from.
//!
//! Everything here is a pure function of the frame index so that a test can regenerate the exact
//! state the recorder was given for any keyframe without storing it.

#![allow(dead_code)]

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use std::sync::Mutex;

use crate::replay_file::playback::ReplayFilePlayer;
use crate::replay_file::record::{ReplayFileRecorderFns, ReplayFileSink, ReplayFileWriteError};
use crate::replay_file::{ReplayConsoleType, ReplayFileMetadata, ReplayHeaderBytes};
use crate::{BookmarkMetadata, ByteVec, Counter, InputBuffer, KeyframeMetadata, Packet, PacketWriteCommand, Speed};

/// Crash-safe (temp-file) layout of the script recorded by the v3 writer: 3 completed blobs
/// followed by an uncompressed tail holding a full keyframe and 8 `DeltaKeyframe`s (plus 27
/// in-blob `DeltaKeyframe`s). Generated once from commit `7ba309f` (the last v3 writer) with a
/// throwaway test that ran [`run_script`] with `minimum_uncompressed_bytes_per_blob = 44 KiB`,
/// `compression_level = 3` and snapshotted the temp sink before `close()`.
pub const V3_SMALL: &[u8] = include_bytes!("../tests/fixtures/v3-small.replay");

/// The same recording after `close()` (all blobs, 4 of them).
pub const V3_SMALL_CLOSED: &[u8] = include_bytes!("../tests/fixtures/v3-small-closed.replay");

/// Base length of a synthetic state. Deliberately not a multiple of 4 so the trailing partial word
/// path of the region diff is exercised.
pub const STATE_LEN: usize = 4096 + 3;

/// Number of emulated frames in the script.
pub const TOTAL_FRAMES: u64 = 200;

/// A keyframe is inserted after every frame that is a multiple of this (plus the frame-0 keyframe).
pub const KEYFRAME_INTERVAL: u64 = 5;

/// Keyframe whose state shares nothing with its predecessor (forces the "delta not smaller than the
/// state" fallback to a full keyframe).
pub const REWRITTEN_FRAME: u64 = 100;

/// Keyframes in this range have a longer state than usual (forces the "length changed" fallback to
/// a full keyframe at both ends of the range).
pub const LONGER_RANGE: core::ops::Range<u64> = 150..160;

fn xorshift(mut x: u64) -> u64 {
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

/// Deterministic pseudo-random bytes.
pub fn pseudo_random_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len + 8);
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    while v.len() < len {
        x = xorshift(x);
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.truncate(len);
    v
}

/// The synthetic save state that the script hands to the recorder at frame `frame`.
///
/// Consecutive keyframes differ in a handful of clustered runs of words (so both the v3 word diff
/// and the v4 region diff kick in), except at [`REWRITTEN_FRAME`] (a completely different state)
/// and inside [`LONGER_RANGE`] (a longer state).
pub fn state_for(frame: u64) -> Vec<u8> {
    let len = if LONGER_RANGE.contains(&frame) { STATE_LEN + 16 } else { STATE_LEN };

    if frame == REWRITTEN_FRAME {
        return pseudo_random_bytes(0xDEAD_BEEF ^ frame, len);
    }

    let mut state = pseudo_random_bytes(1, len);

    // Every keyframe up to and including `frame` leaves a few clustered edits behind, so the state
    // at any frame is the accumulation of all earlier edits (like real RAM) and the diff between
    // consecutive keyframes is only the newest ones.
    let mut k = 0;
    while k <= frame {
        let seed = xorshift(k.wrapping_add(7).wrapping_mul(0x1234_5679));
        let runs = 1 + (seed % 4) as usize;
        for r in 0..runs {
            let s = xorshift(seed.wrapping_add(r as u64 * 31));
            let start = (s as usize) % (len - 64);
            let run_len = 1 + ((s >> 20) as usize) % 24;
            for (i, b) in state[start..start + run_len].iter_mut().enumerate() {
                *b = (s >> ((i % 8) * 8)) as u8 ^ (k as u8);
            }
        }
        // The last 3 bytes (the partial trailing word) change on every keyframe too.
        let tail = state.len() - 3;
        state[tail] = k as u8;
        state[tail + 1] = (k >> 8) as u8;
        state[tail + 2] = 0x5A;
        k += KEYFRAME_INTERVAL;
    }

    state
}

/// The timestamp delta (ms) of the `frame`-th `NextFrame` (1-based).
pub fn delta_for(frame_one_based: u64) -> u64 {
    let base = 16 + (frame_one_based % 2);
    if frame_one_based % 7 == 0 { base + 50 } else { base }
}

/// Absolute timestamp after `frames` frames.
pub fn millis_at(frames: u64) -> u64 {
    (1..=frames).map(delta_for).sum()
}

/// Frames at which keyframes exist in the script (including 0).
pub fn keyframe_frames() -> Vec<u64> {
    let mut v = alloc::vec![0];
    v.extend((1..=TOTAL_FRAMES).filter(|f| f % KEYFRAME_INTERVAL == 0));
    v
}

pub fn make_metadata() -> ReplayFileMetadata {
    ReplayFileMetadata {
        console_type: ReplayConsoleType::GameBoy,
        rom_name: "TEST".to_string(),
        rom_filename: "test.gb".to_string(),
        emulator_core_name: "test-core 1.0".to_string(),
        ..Default::default()
    }
}

pub fn ib(bytes: &[u8]) -> InputBuffer {
    let mut v = InputBuffer::new();
    v.extend_from_slice(bytes);
    v
}

pub fn bv(bytes: &[u8]) -> ByteVec {
    let mut v = ByteVec::new();
    v.extend_from_slice(bytes);
    v
}

/// One step of the script, applied *before* the `NextFrame` of frame `frame` (1-based).
#[derive(Clone, Debug, PartialEq)]
pub enum ScriptOp {
    SetInput(Vec<u8>),
    SetSpeed(f64),
    Counter(String, i64),
    Bookmark(String),
    WriteMemory(u64, Vec<u8>),
    ResetConsole,
    LoadSaveState(u64),
}

/// The ops that happen just before the given frame's `NextFrame`.
pub fn ops_before_frame(frame: u64) -> Vec<ScriptOp> {
    let mut ops = Vec::new();
    match frame {
        3 => ops.push(ScriptOp::SetInput(alloc::vec![0xAB, 0xCD])),
        4 => ops.push(ScriptOp::SetSpeed(2.0)),
        6 => ops.push(ScriptOp::Counter("deaths".to_string(), 1)),
        8 => ops.push(ScriptOp::Bookmark("checkpoint".to_string())),
        10 => ops.push(ScriptOp::Counter("deaths".to_string(), 2)),
        12 => {
            ops.push(ScriptOp::WriteMemory(0x1234, alloc::vec![1, 2, 3, 4]));
            ops.push(ScriptOp::WriteMemory(0xC000, alloc::vec![0x55]));
            ops.push(ScriptOp::WriteMemory(0xD000, alloc::vec![9; 7]));
        }
        17 => ops.push(ScriptOp::ResetConsole),
        21 => ops.push(ScriptOp::LoadSaveState(1000)),
        33 => ops.push(ScriptOp::SetInput(alloc::vec![0x01])),
        77 => ops.push(ScriptOp::Bookmark("late".to_string())),
        90 => ops.push(ScriptOp::Counter("resets".to_string(), -3)),
        123 => ops.push(ScriptOp::SetSpeed(0.5)),
        166 => ops.push(ScriptOp::Bookmark("checkpoint".to_string())),
        _ => {}
    }
    ops
}

/// A sink whose buffer can be inspected while the recorder still owns it (used to capture the
/// crash-safe temp-file layout, which `close()` would otherwise rewrite into the final layout).
#[derive(Clone, Default, Debug)]
pub struct SharedSink(pub Arc<Mutex<Vec<u8>>>);

impl SharedSink {
    pub fn snapshot(&self) -> Vec<u8> {
        self.0.lock().unwrap().clone()
    }
}

impl ReplayFileSink for SharedSink {
    fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), ReplayFileWriteError> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(())
    }
    fn truncate(&mut self, size: u64) -> Result<(), ReplayFileWriteError> {
        self.0.lock().unwrap().truncate(size as usize);
        Ok(())
    }
    fn overwrite_header(&mut self, header_data: &ReplayHeaderBytes) -> Result<(), ReplayFileWriteError> {
        self.0.lock().unwrap().overwrite_header(header_data)
    }
    fn write_packet_data(&mut self, instructions: &[PacketWriteCommand<'_>]) -> Result<usize, ReplayFileWriteError> {
        self.0.lock().unwrap().write_packet_data(instructions)
    }
}

/// Drive `recorder` through the whole script.
///
/// `recorder` must already contain the frame-0 keyframe (`state_for(0)` at timestamp 0), i.e. it
/// was created with `new_with_metadata`.
pub fn run_script<R: ReplayFileRecorderFns + ?Sized>(recorder: &mut R) {
    let mut running: u64 = 0;
    for frame in 1..=TOTAL_FRAMES {
        for op in ops_before_frame(frame) {
            match op {
                ScriptOp::SetInput(i) => recorder.set_input(ib(&i)).unwrap(),
                ScriptOp::SetSpeed(s) => recorder.set_speed(Speed::from_multiplier_float(s)).unwrap(),
                ScriptOp::Counter(n, d) => recorder.change_counter(n, d).unwrap(),
                ScriptOp::Bookmark(n) => recorder.add_bookmark(n).unwrap(),
                ScriptOp::WriteMemory(a, d) => recorder.write_memory(a, bv(&d)).unwrap(),
                ScriptOp::ResetConsole => recorder.reset_console().unwrap(),
                ScriptOp::LoadSaveState(f) => recorder.load_save_state(bv(&state_for(f))).unwrap(),
            }
        }

        running += delta_for(frame);
        recorder.next_frame(running.into()).unwrap();

        if frame % KEYFRAME_INTERVAL == 0 {
            recorder.insert_keyframe(bv(&state_for(frame)), running.into()).unwrap();
        }
    }
}

/// Record the script with `settings`. Returns the crash-safe temp-file layout (snapshotted just
/// before `close()`) and the closed final file.
pub fn record_script(settings: crate::replay_file::record::ReplayFileRecorderSettings) -> (Vec<u8>, Vec<u8>) {
    let temp = SharedSink::default();
    let final_sink = SharedSink::default();
    let mut recorder = crate::replay_file::record::ReplayFileRecorder::new_with_metadata(
        make_metadata(),
        ByteVec::new(),
        settings,
        0u64.into(),
        ib(&[0]),
        Speed::default(),
        bv(&state_for(0)),
        final_sink.clone(),
        temp.clone(),
    )
    .unwrap();

    run_script(&mut recorder);
    let temp_bytes = temp.snapshot();
    recorder.close().unwrap();
    (temp_bytes, final_sink.snapshot())
}

/// How a keyframe is stored in a file.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum StoredKeyframeKind {
    Full,
    V3Delta,
    RegionDelta,
}

/// Raw layout statistics of a replay file (walks the packets directly, decompressing blobs).
#[derive(Clone, Default, Debug)]
pub struct FileStats {
    pub blobs: usize,
    pub top_level_packets: usize,
    /// `(frame, kind, in_blob)` for every keyframe-class packet in stream order.
    pub keyframes: Vec<(u64, StoredKeyframeKind, bool)>,
}

impl FileStats {
    pub fn count(&self, kind: StoredKeyframeKind) -> usize {
        self.keyframes.iter().filter(|k| k.1 == kind).count()
    }

    pub fn kind_at(&self, frame: u64) -> StoredKeyframeKind {
        self.keyframes.iter().find(|k| k.0 == frame).unwrap_or_else(|| panic!("no keyframe at {frame}")).1
    }
}

pub fn file_stats(bytes: &[u8]) -> FileStats {
    use crate::PacketIO;

    let header = crate::replay_file::ReplayHeaderRaw::from_bytes(bytes[..2048].try_into().unwrap());
    let version = header.replay_version;
    let mut stats = FileStats::default();

    fn note(stats: &mut FileStats, packet: &Packet, in_blob: bool) {
        match packet {
            Packet::Keyframe { metadata, .. } => stats.keyframes.push((metadata.elapsed_frames, StoredKeyframeKind::Full, in_blob)),
            Packet::DeltaKeyframe { metadata, .. } => stats.keyframes.push((metadata.elapsed_frames, StoredKeyframeKind::V3Delta, in_blob)),
            Packet::RegionDeltaKeyframe { metadata, .. } => stats.keyframes.push((metadata.elapsed_frames, StoredKeyframeKind::RegionDelta, in_blob)),
            _ => {}
        }
    }

    let mut data = &bytes[2048 + header.patch_data_length as usize..];
    while !data.is_empty() {
        let packet = Packet::read_all(&mut data, version).expect("packet");
        stats.top_level_packets += 1;
        if let Packet::CompressedBlob { compressed_data, uncompressed_size, .. } = &packet {
            stats.blobs += 1;
            let raw = crate::decompress_data(compressed_data.as_slice(), *uncompressed_size as usize).expect("decompress");
            let mut inner = raw.as_slice();
            while !inner.is_empty() {
                note(&mut stats, &Packet::read_all(&mut inner, version).expect("inner packet"), true);
            }
        }
        else {
            note(&mut stats, &packet, false);
        }
    }

    stats
}

/// Expected input bytes in effect at frame `frame` (as recorded in keyframe metadata).
pub fn expected_input_at(frame: u64) -> Vec<u8> {
    let mut input = alloc::vec![0u8];
    for f in 1..=frame {
        for op in ops_before_frame(f) {
            if let ScriptOp::SetInput(i) = op {
                input = i;
            }
        }
    }
    input
}

/// Expected counters at frame `frame` (sorted by name, as the recorder emits them).
pub fn expected_counters_at(frame: u64) -> Vec<(String, i64)> {
    let mut map = alloc::collections::BTreeMap::new();
    for f in 1..=frame {
        for op in ops_before_frame(f) {
            if let ScriptOp::Counter(n, d) = op {
                *map.entry(n).or_insert(0i64) += d;
            }
        }
    }
    map.into_iter().collect()
}

/// Expected speed in effect at frame `frame`.
pub fn expected_speed_at(frame: u64) -> Speed {
    let mut speed = Speed::default();
    for f in 1..=frame {
        for op in ops_before_frame(f) {
            if let ScriptOp::SetSpeed(s) = op {
                speed = Speed::from_multiplier_float(s);
            }
        }
    }
    speed
}

/// The exact keyframe packet the player must yield for the keyframe at `frame`.
pub fn expected_keyframe(frame: u64) -> Packet {
    Packet::Keyframe {
        metadata: KeyframeMetadata {
            input: ib(&expected_input_at(frame)),
            speed: expected_speed_at(frame),
            elapsed_frames: frame,
            elapsed_millis: millis_at(frame).into(),
            counters: expected_counters_at(frame)
                .into_iter()
                .map(|(name, value)| Counter { name, value })
                .collect(),
        },
        state: bv(&state_for(frame)),
    }
}

/// The exact flat packet stream (blobs expanded, deltas materialised) a player must yield for the
/// script, starting from the frame-0 keyframe.
pub fn expected_packets() -> Vec<Packet> {
    let mut packets = alloc::vec![expected_keyframe(0)];
    for frame in 1..=TOTAL_FRAMES {
        for op in ops_before_frame(frame) {
            packets.push(match op {
                ScriptOp::SetInput(i) => Packet::ChangeInput { data: ib(&i) },
                ScriptOp::SetSpeed(s) => Packet::ChangeSpeed { speed: Speed::from_multiplier_float(s) },
                ScriptOp::Counter(name, delta) => Packet::IncrementCounter { name, delta },
                ScriptOp::Bookmark(name) => Packet::Bookmark {
                    metadata: BookmarkMetadata {
                        name,
                        // Ops happen before frame `frame`'s NextFrame, i.e. at frame `frame - 1`.
                        elapsed_frames: frame - 1,
                        elapsed_millis: millis_at(frame - 1).into(),
                    },
                },
                ScriptOp::WriteMemory(address, data) => Packet::WriteMemory { address, data: bv(&data) },
                ScriptOp::ResetConsole => Packet::ResetConsole,
                ScriptOp::LoadSaveState(f) => Packet::LoadSaveState { state: bv(&state_for(f)) },
            });
        }
        packets.push(Packet::NextFrame { timestamp_delta: delta_for(frame).into() });
        if frame % KEYFRAME_INTERVAL == 0 {
            packets.push(expected_keyframe(frame));
        }
    }
    packets
}

/// Index into [`expected_packets`] of the keyframe at `frame`.
pub fn expected_keyframe_index(packets: &[Packet], frame: u64) -> usize {
    packets
        .iter()
        .position(|p| matches!(p, Packet::Keyframe { metadata, .. } if metadata.elapsed_frames == frame))
        .unwrap_or_else(|| panic!("no expected keyframe at frame {frame}"))
}

fn describe(p: &Packet) -> String {
    match p {
        Packet::Keyframe { metadata, state } => alloc::format!("Keyframe(frame {}, {} bytes)", metadata.elapsed_frames, state.len()),
        Packet::DeltaKeyframe { metadata, .. } => alloc::format!("DeltaKeyframe(frame {})", metadata.elapsed_frames),
        Packet::RegionDeltaKeyframe { metadata, .. } => alloc::format!("RegionDeltaKeyframe(frame {})", metadata.elapsed_frames),
        Packet::CompressedBlob { .. } => "CompressedBlob".to_string(),
        other => alloc::format!("{other:?}"),
    }
}

/// Assert that `got` equals `expected` with a readable message.
pub fn assert_packet_eq(got: &Packet, expected: &Packet, context: &str) {
    if got != expected {
        match (got, expected) {
            (Packet::Keyframe { metadata: gm, state: gs }, Packet::Keyframe { metadata: em, state: es }) => {
                assert_eq!(gm, em, "{context}: keyframe metadata differs");
                assert_eq!(gs.len(), es.len(), "{context}: keyframe state length differs");
                let first = gs.iter().zip(es.iter()).position(|(a, b)| a != b);
                panic!("{context}: keyframe state (frame {}) differs at byte {first:?}", em.elapsed_frames);
            }
            _ => panic!("{context}: got {} expected {}", describe(got), describe(expected)),
        }
    }
}

/// A fixed pseudo-random seek order over the script's keyframes that goes forward, backward, across
/// blob boundaries and repeats itself.
pub fn seek_order() -> Vec<u64> {
    let frames = keyframe_frames();
    let mut order = Vec::new();
    let mut x = 0x1234_5678_9ABC_DEF0u64;
    for _ in 0..3 * frames.len() {
        x = xorshift(x);
        order.push(frames[(x as usize) % frames.len()]);
    }
    // Some deliberate patterns: same keyframe twice, last, first, adjacent forward, adjacent back.
    let last = *frames.last().unwrap();
    order.extend([last, last, 0, 0, KEYFRAME_INTERVAL, 2 * KEYFRAME_INTERVAL, KEYFRAME_INTERVAL, last, 0]);
    order
}

/// Check that a replay produced from the script (by any writer/layout) plays back exactly.
///
/// Verifies the totals, the keyframe/bookmark indexes, a full sequential walk against
/// [`expected_packets`], and random-access seeks (forward, backward, cross-blob, repeated) each
/// followed by reading up to the next keyframe.
pub fn check_script_replay(bytes: &[u8], context: &str) {
    let mut player = ReplayFilePlayer::new(bytes, false)
        .unwrap_or_else(|e| panic!("{context}: failed to open: {e:?}"));

    assert_eq!(player.get_total_frames(), TOTAL_FRAMES, "{context}: total frames");
    assert_eq!(player.get_total_milliseconds().0, millis_at(TOTAL_FRAMES), "{context}: total millis");
    assert_eq!(player.all_keyframes().keys().copied().collect::<Vec<_>>(), keyframe_frames(), "{context}: keyframe index");
    for (frame, metadata_list) in player.all_keyframes() {
        for metadata in metadata_list {
            let Packet::Keyframe { metadata: expected, .. } = expected_keyframe(*frame) else { unreachable!() };
            assert_eq!(**metadata, expected, "{context}: keyframe index metadata at frame {frame}");
        }
    }

    let bookmarks = player.all_bookmarks();
    assert_eq!(bookmarks.keys().cloned().collect::<Vec<_>>(), alloc::vec!["checkpoint".to_string(), "late".to_string()], "{context}: bookmark index");
    assert_eq!(bookmarks["checkpoint"].iter().map(|b| b.elapsed_frames).collect::<Vec<_>>(), alloc::vec![7, 165], "{context}: checkpoint frames");
    assert_eq!(bookmarks["late"].iter().map(|b| b.elapsed_frames).collect::<Vec<_>>(), alloc::vec![76], "{context}: late frames");

    let expected = expected_packets();

    // Sequential walk.
    player.go_to_keyframe(0).unwrap_or_else(|e| panic!("{context}: seek 0: {e:?}"));
    let mut index = 0usize;
    loop {
        let packet = player.next_packet().unwrap_or_else(|e| panic!("{context}: next_packet at {index}: {e:?}"));
        let Some(packet) = packet else { break };
        let Some(exp) = expected.get(index) else {
            panic!("{context}: player yielded extra packet {} after the expected {} packets", describe(packet), expected.len());
        };
        assert_packet_eq(packet, exp, &alloc::format!("{context}: sequential packet {index}"));
        index += 1;
    }
    assert_eq!(index, expected.len(), "{context}: sequential walk stopped early");

    // Random access.
    for (n, frame) in seek_order().into_iter().enumerate() {
        player.go_to_keyframe(frame).unwrap_or_else(|e| panic!("{context}: seek #{n} to {frame}: {e:?}"));
        let mut index = expected_keyframe_index(&expected, frame);
        let mut first = true;
        loop {
            let packet = player.next_packet().unwrap_or_else(|e| panic!("{context}: next_packet after seek #{n} to {frame}: {e:?}"));
            let Some(packet) = packet else {
                assert_eq!(index, expected.len(), "{context}: stream ended early after seek #{n} to {frame}");
                break;
            };
            assert_packet_eq(packet, &expected[index], &alloc::format!("{context}: seek #{n} to {frame}, packet {index}"));
            index += 1;
            if !first && matches!(packet, Packet::Keyframe { .. }) {
                break;
            }
            first = false;
        }
    }
}
