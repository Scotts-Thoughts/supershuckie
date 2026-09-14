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

use crate::replay_file::playback::{BookmarkTableSource, ReplayFilePlayer};
use crate::replay_file::record::{ReplayFileRecorderFns, ReplayFileSink, ReplayFileWriteError};
use crate::replay_file::{ReplayConsoleType, ReplayFileMetadata, ReplayHeaderBytes, ReplayHeaderRaw, REPLAY_VERSION_BOOKMARK_SECTION};
use crate::{Bookmark, BookmarkMetadata, BookmarkTable, BookmarkTypeRecord, ByteVec, Counter, InputBuffer, KeyframeMetadata, Packet, PacketWriteCommand, Speed};

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
    /// A point bookmark on the current frame (`frame - 1`). The v3 fixtures hold these as `Bookmark`
    /// packets; the current writer adds them to its bookmark table.
    Bookmark(String),
    WriteMemory(u64, Vec<u8>),
    ResetConsole,
    LoadSaveState(u64),
    /// Current writer only (not in the v3 fixtures): give bookmark `id` an out frame.
    BookmarkSetOut(u64, u64),
    /// Current writer only: give bookmark `id` a type.
    BookmarkSetType(u64, BookmarkTypeRecord),
    /// Current writer only: delete bookmark `id`.
    BookmarkDelete(u64),
}

impl ScriptOp {
    /// Whether the op edits the bookmark table (the ops the v3 writer did not have).
    pub fn is_table_edit(&self) -> bool {
        matches!(self, ScriptOp::BookmarkSetOut(..) | ScriptOp::BookmarkSetType(..) | ScriptOp::BookmarkDelete(..))
    }
}

/// The type the script gives bookmark 2.
pub fn script_route_type() -> BookmarkTypeRecord {
    BookmarkTypeRecord { id: 0x55, name: "Route".to_string(), color: 0x1B8577 }
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
        120 => ops.push(ScriptOp::BookmarkSetOut(2, 119)),
        121 => ops.push(ScriptOp::BookmarkSetType(2, script_route_type())),
        123 => ops.push(ScriptOp::SetSpeed(0.5)),
        140 => ops.push(ScriptOp::BookmarkDelete(1)),
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

/// Apply a bookmark op of frame `frame` to `table`; returns whether it was one.
fn apply_bookmark_op(table: &mut BookmarkTable, op: &ScriptOp, frame: u64) -> bool {
    let now = frame - 1;
    match op {
        ScriptOp::Bookmark(name) => {
            table.insert(Bookmark { name: name.clone(), in_frame: now, in_millis: millis_at(now).into(), ..Default::default() });
        }
        ScriptOp::BookmarkSetOut(id, out) => {
            table.get_mut(*id).expect("script bookmark").out = Some((*out, millis_at(*out).into()));
        }
        ScriptOp::BookmarkSetType(id, record) => {
            table.get_mut(*id).expect("script bookmark").type_id = record.id;
            table.set_type_record(record.clone());
            table.prune_types();
        }
        ScriptOp::BookmarkDelete(id) => {
            table.remove(*id).expect("script bookmark");
            table.prune_types();
        }
        _ => return false
    }
    true
}

/// The current writer's bookmark table once the ops of frames `1..=frame` have run.
pub fn script_table_through(frame: u64) -> BookmarkTable {
    let mut table = BookmarkTable::new();
    for f in 1..=frame.min(TOTAL_FRAMES) {
        for op in ops_before_frame(f) {
            apply_bookmark_op(&mut table, &op, f);
        }
    }
    table
}

/// The table a pre-v5 player derives from the v3 fixtures' `Bookmark` packets.
pub fn legacy_script_table() -> BookmarkTable {
    let mut legacy = Vec::new();
    for frame in 1..=TOTAL_FRAMES {
        for op in ops_before_frame(frame) {
            if let ScriptOp::Bookmark(name) = op {
                legacy.push(BookmarkMetadata { name, elapsed_frames: frame - 1, elapsed_millis: millis_at(frame - 1).into() });
            }
        }
    }
    BookmarkTable::from_legacy(&legacy)
}

/// Drive `recorder` through the whole script.
///
/// `recorder` must already contain the frame-0 keyframe (`state_for(0)` at timestamp 0), i.e. it
/// was created with `new_with_metadata`. `after_frame(frame)` runs after each frame (and its
/// keyframe), e.g. to snapshot the sinks.
pub fn run_script<R: ReplayFileRecorderFns + ?Sized>(recorder: &mut R) {
    run_script_observed(recorder, &mut |_| {});
}

/// [`run_script`] with a callback after every frame.
pub fn run_script_observed<R: ReplayFileRecorderFns + ?Sized>(recorder: &mut R, after_frame: &mut dyn FnMut(u64)) {
    let mut running: u64 = 0;
    let mut table = BookmarkTable::new();
    for frame in 1..=TOTAL_FRAMES {
        for op in ops_before_frame(frame) {
            if apply_bookmark_op(&mut table, &op, frame) {
                recorder.set_bookmark_table(table.clone()).unwrap();
                continue;
            }
            match op {
                ScriptOp::SetInput(i) => recorder.set_input(ib(&i)).unwrap(),
                ScriptOp::SetSpeed(s) => recorder.set_speed(Speed::from_multiplier_float(s)).unwrap(),
                ScriptOp::Counter(n, d) => recorder.change_counter(n, d).unwrap(),
                ScriptOp::WriteMemory(a, d) => recorder.write_memory(a, bv(&d)).unwrap(),
                ScriptOp::ResetConsole => recorder.reset_console().unwrap(),
                ScriptOp::LoadSaveState(f) => recorder.load_save_state(bv(&state_for(f))).unwrap(),
                ScriptOp::Bookmark(_) | ScriptOp::BookmarkSetOut(..) | ScriptOp::BookmarkSetType(..) | ScriptOp::BookmarkDelete(..) => unreachable!("applied above"),
            }
        }

        running += delta_for(frame);
        recorder.next_frame(running.into()).unwrap();

        if frame % KEYFRAME_INTERVAL == 0 {
            recorder.insert_keyframe(bv(&state_for(frame)), running.into()).unwrap();
        }

        after_frame(frame);
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
    /// `BookmarkTable` packets, anywhere in the stream.
    pub bookmark_snapshots: usize,
    /// Bytes after the packet stream (the bookmark section of a closed v5 file).
    pub trailing_bytes: usize,
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
            Packet::BookmarkTable { .. } => stats.bookmark_snapshots += 1,
            _ => {}
        }
    }

    let stream_end = header.packet_stream_end().map(|end| end as usize).unwrap_or(bytes.len());
    stats.trailing_bytes = bytes.len() - stream_end;
    let mut data = &bytes[2048 + header.patch_data_length as usize..stream_end];
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
///
/// With `legacy_bookmarks` (the v3 fixtures) the stream holds the script's `Bookmark` packets;
/// otherwise it holds no bookmark packets at all (callers skip the `BookmarkTable` snapshots of a
/// v5 stream, whose placement depends on the blob layout, and check them separately).
pub fn expected_packets(legacy_bookmarks: bool) -> Vec<Packet> {
    let mut packets = alloc::vec![expected_keyframe(0)];
    for frame in 1..=TOTAL_FRAMES {
        for op in ops_before_frame(frame) {
            if op.is_table_edit() || (!legacy_bookmarks && matches!(op, ScriptOp::Bookmark(_))) {
                continue;
            }
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
                ScriptOp::BookmarkSetOut(..) | ScriptOp::BookmarkSetType(..) | ScriptOp::BookmarkDelete(..) => unreachable!("skipped above"),
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

/// What the bookmarks of a replay of the script must be.
#[derive(Copy, Clone, Debug)]
pub enum BookmarkExpectation<'a> {
    /// Recorded by the current writer running the script: the resolved table is the script's final
    /// table, and every snapshot in the stream is the script's table as of where it sits.
    Script,
    /// The resolved table, and every snapshot in the stream, is this table (re-encoded files, whose
    /// table is seeded at the start; upgraded files, which have no snapshots).
    Fixed(&'a BookmarkTable),
    /// Recorded by the current writer running the script, with its section since rewritten to this
    /// table: the snapshots follow the script.
    Rewritten(&'a BookmarkTable),
}

/// Check that a replay produced from the script (by any writer/layout) plays back exactly.
///
/// Pre-v5 files must hold the script's `Bookmark` packets; v5 files are checked against
/// [`BookmarkExpectation::Script`].
pub fn check_script_replay(bytes: &[u8], context: &str) {
    check_script_replay_with(bytes, context, BookmarkExpectation::Script)
}

/// [`check_script_replay`] with explicit expectations for a v5 file's bookmarks.
///
/// Verifies the totals, the keyframe index, the bookmarks, a full sequential walk against
/// [`expected_packets`], and random-access seeks (forward, backward, cross-blob, repeated) each
/// followed by reading up to the next keyframe.
pub fn check_script_replay_with(bytes: &[u8], context: &str, bookmarks: BookmarkExpectation<'_>) {
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

    let header = ReplayHeaderRaw::from_bytes(bytes[..2048].try_into().unwrap());
    let legacy = header.replay_version < REPLAY_VERSION_BOOKMARK_SECTION;
    let has_section = header.packet_stream_end().is_some_and(|end| (end as usize) < bytes.len());

    if legacy {
        let bookmarks = player.legacy_bookmarks();
        assert_eq!(bookmarks.keys().cloned().collect::<Vec<_>>(), alloc::vec!["checkpoint".to_string(), "late".to_string()], "{context}: bookmark index");
        assert_eq!(bookmarks["checkpoint"].iter().map(|b| b.elapsed_frames).collect::<Vec<_>>(), alloc::vec![7, 165], "{context}: checkpoint frames");
        assert_eq!(bookmarks["late"].iter().map(|b| b.elapsed_frames).collect::<Vec<_>>(), alloc::vec![76], "{context}: late frames");
        assert_eq!(*player.bookmark_table(), legacy_script_table(), "{context}: legacy bookmark table");
        assert_eq!(player.bookmark_table_source(), BookmarkTableSource::Legacy, "{context}: bookmark table source");
    }
    else {
        let expected_table = match bookmarks {
            BookmarkExpectation::Script => script_table_through(TOTAL_FRAMES),
            BookmarkExpectation::Fixed(table) | BookmarkExpectation::Rewritten(table) => table.clone(),
        };
        assert_eq!(*player.bookmark_table(), expected_table, "{context}: bookmark table");
        if has_section {
            assert_eq!(player.bookmark_table_source(), BookmarkTableSource::Section, "{context}: bookmark table source");
            assert!(player.bookmark_section_error().is_none(), "{context}: {:?}", player.bookmark_section_error());
        }
    }

    // A snapshot is the table as of its position: `frames` NextFrames in, possibly after the ops of
    // the next frame.
    let snapshot_ok = |table: &BookmarkTable, frames: u64| match bookmarks {
        BookmarkExpectation::Script | BookmarkExpectation::Rewritten(_) => *table == script_table_through(frames) || *table == script_table_through(frames + 1),
        BookmarkExpectation::Fixed(expected) => table == expected,
    };

    let expected = expected_packets(legacy);

    // Sequential walk.
    player.go_to_keyframe(0).unwrap_or_else(|e| panic!("{context}: seek 0: {e:?}"));
    let mut index = 0usize;
    let mut frames = 0u64;
    let mut snapshots = 0usize;
    loop {
        let packet = player.next_packet().unwrap_or_else(|e| panic!("{context}: next_packet at {index}: {e:?}"));
        let Some(packet) = packet else { break };
        if !legacy {
            match packet {
                Packet::BookmarkTable { table } => {
                    assert!(snapshot_ok(table, frames), "{context}: snapshot after {frames} frames is {table:?}");
                    snapshots += 1;
                    continue;
                }
                // A v3/v4 file upgraded in place keeps its legacy packets.
                Packet::Bookmark { .. } => continue,
                _ => {}
            }
        }
        let Some(exp) = expected.get(index) else {
            panic!("{context}: player yielded extra packet {} after the expected {} packets", describe(packet), expected.len());
        };
        assert_packet_eq(packet, exp, &alloc::format!("{context}: sequential packet {index}"));
        if matches!(packet, Packet::NextFrame { .. }) {
            frames += 1;
        }
        index += 1;
    }
    assert_eq!(index, expected.len(), "{context}: sequential walk stopped early");
    if !legacy && matches!(bookmarks, BookmarkExpectation::Script | BookmarkExpectation::Rewritten(_)) {
        assert!(snapshots >= 5, "{context}: only {snapshots} bookmark snapshots (the script changes the table 5 times)");
    }

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
            if !legacy && matches!(packet, Packet::BookmarkTable { .. } | Packet::Bookmark { .. }) {
                continue;
            }
            assert_packet_eq(packet, &expected[index], &alloc::format!("{context}: seek #{n} to {frame}, packet {index}"));
            index += 1;
            if !first && matches!(packet, Packet::Keyframe { .. }) {
                break;
            }
            first = false;
        }
    }
}
