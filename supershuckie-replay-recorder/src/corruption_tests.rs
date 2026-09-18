//! B7 (T3): corrupt-input fuzzing scaffold.
//!
//! Applies a handful of blind byte-level corruption patterns at a swept range of offsets over
//! several real replay fixtures, and checks that the parser never *panics* on the result -- an
//! `Err` (or a tolerant, truncated `Ok`) is an accepted outcome of corrupted input, a panic is not
//! (this crate's replay files are user data: a corrupted or hand-edited file must be rejected
//! cleanly, never crash the process). Any panic these tests find is a real parser bug (a missing
//! bounds check or an arithmetic overflow) and must be fixed in the parser, not worked around here.
//!
//! Coverage here is intentionally bounded (see [`corruption_scaffold`]'s doc comment) to keep the
//! whole module's wall time in the single-digit seconds in a debug build.

use alloc::format;
use alloc::vec::Vec;
use core::fmt::Display;

use crate::replay_file::playback::ReplayFilePlayer;
use crate::replay_file::ReplayHeaderBytes;
use crate::test_support::*;
use crate::util::{compress_data, decompress_data};
use crate::{ByteVec, Packet};

/// A blind byte-level corruption applied at one offset.
#[derive(Copy, Clone, Debug)]
enum Pattern {
    XorFf,
    SetZero,
    SetFf,
    /// Overwrite with the widest possible encoding of the crate's `UnsignedInteger` (`u64`) varint
    /// (see `packet::io::PacketIO for UnsignedInteger`): a length byte of 8 followed by 2^63's 8
    /// little-endian bytes. 9 bytes wide. Wherever this lands on an actual length-prefixed field
    /// (a `Vec<T>`/`ByteVec`/`String` element count, or an `UnsignedInteger` field), it tries to
    /// make the reader believe there are ~2^63 elements or an enormous value to read.
    Varint2Pow63,
}

const PATTERNS: [Pattern; 4] = [Pattern::XorFf, Pattern::SetZero, Pattern::SetFf, Pattern::Varint2Pow63];

/// Apply `pattern` at `offset` in `bytes`, in place. Returns `false` (no-op) if the pattern does
/// not fit at `offset` (only possible for [`Pattern::Varint2Pow63`], which is 9 bytes wide).
fn apply_pattern(bytes: &mut [u8], offset: usize, pattern: Pattern) -> bool {
    match pattern {
        Pattern::XorFf => match bytes.get_mut(offset) {
            Some(b) => { *b ^= 0xFF; true }
            None => false,
        },
        Pattern::SetZero => match bytes.get_mut(offset) {
            Some(b) => { *b = 0x00; true }
            None => false,
        },
        Pattern::SetFf => match bytes.get_mut(offset) {
            Some(b) => { *b = 0xFF; true }
            None => false,
        },
        Pattern::Varint2Pow63 => match bytes.get_mut(offset..offset + 9) {
            Some(dest) => { dest.copy_from_slice(&[8, 0, 0, 0, 0, 0, 0, 0, 0x80]); true }
            None => false,
        },
    }
}

/// Walk every packet from the cursor's current position to the end (or the first error),
/// discarding the result -- corrupted input is expected to sometimes fail mid-stream.
fn drain(player: &mut ReplayFilePlayer) {
    loop {
        match player.next_packet() {
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
}

/// Exercise `player`: seek to every keyframe it reports (each seek on its own -- not followed by a
/// full drain, which would make this quadratic in the keyframe count), then do a single sequential
/// walk from the start to the end (or the first error), then read the bookmark accessors. Used once
/// a `ReplayFilePlayer` has already been constructed.
fn probe_player(player: &mut ReplayFilePlayer) {
    let frames: Vec<u64> = player.all_keyframes().keys().copied().collect();
    for frame in &frames {
        let _ = player.go_to_keyframe(*frame);
    }
    if player.go_to_keyframe(0).is_ok() {
        drain(player);
    }
    let _ = player.bookmark_table();
    let _ = player.bookmark_table_source();
    let _ = player.stream_truncated();
}

/// Construct a player from `bytes` under both corruption-tolerance modes and, for whichever
/// construction succeeds, run [`probe_player`] (using that player's own reported keyframes).
fn probe(bytes: &[u8]) {
    for allow_some_corruption in [false, true] {
        if let Ok(mut player) = ReplayFilePlayer::new(bytes, allow_some_corruption) {
            probe_player(&mut player);
        }
    }
}

/// [`probe`], but seeking the given `frames` (from the *uncorrupted* fixture's keyframe metadata)
/// rather than whatever the corrupted player itself reports -- used by
/// [`corruption_inside_blobs`], where the corruption never touches the top-level keyframe index.
fn probe_frames(bytes: &[u8], frames: &[u64]) {
    for allow_some_corruption in [false, true] {
        if let Ok(mut player) = ReplayFilePlayer::new(bytes, allow_some_corruption) {
            for &frame in frames {
                let _ = player.go_to_keyframe(frame);
            }
            if frames.first().is_some_and(|&f| player.go_to_keyframe(f).is_ok()) {
                drain(&mut player);
            }
            let _ = player.bookmark_table();
            let _ = player.bookmark_table_source();
        }
    }
}

/// Run `f` inside `catch_unwind`; a panic fails the test with `context` in the message (tests
/// unwind even though the app's release profile aborts on panic, so `catch_unwind` works here).
fn check_no_panic(context: impl Display, f: impl FnOnce() + std::panic::UnwindSafe) {
    if std::panic::catch_unwind(f).is_err() {
        panic!("{context}: parser panicked on corrupted input (see the panic message above)");
    }
}

/// The fixtures corruption is applied to: the two on-disk v3 compatibility fixtures (temp/crash and
/// closed layouts) plus a freshly recorded temp/closed pair from the current writer (so the current
/// v5 format -- region deltas, bookmark section, etc. -- gets covered too, not just the legacy v3
/// encoding the on-disk fixtures use).
fn fixtures() -> Vec<(&'static str, Vec<u8>)> {
    let (fresh_temp, fresh_closed) = record_script(settings(45, usize::MAX));
    alloc::vec![
        ("V3_SMALL", V3_SMALL.to_vec()),
        ("V3_SMALL_CLOSED", V3_SMALL_CLOSED.to_vec()),
        ("fresh (temp layout)", fresh_temp),
        ("fresh (closed)", fresh_closed),
    ]
}

/// Offsets to corrupt: dense over `dense_end` (the region not otherwise exhaustively covered --
/// see [`corruption_scaffold`]'s doc comment), then a stride over the remainder of the file.
fn offsets_for(len: usize, dense_end: usize, stride: usize) -> impl Iterator<Item = usize> {
    let dense_end = dense_end.min(len);
    (0..dense_end).chain((dense_end..len).step_by(stride.max(1)))
}

/// The main corruption scaffold: every pattern, at a swept range of offsets, over several
/// fixtures, must never make the parser panic.
///
/// Coverage note (deviation from a literal "every offset in the first 6 KiB, then stride 13"):
/// measured against these ~20-30 KiB, ~200-frame, multi-blob fixtures, each `(offset, pattern)`
/// probe costs close to 1 ms in a debug build -- seeking every keyframe plus one full sequential
/// walk touches every blob's real zstd decompression, and that cost is close to constant
/// regardless of where the corruption lands (a failure in one blob does not skip the attempt to
/// decompress the others, since the keyframe index itself is corruption-independent metadata built
/// before any blob is touched). A dense sweep of the first 6 KiB alone, times 4 patterns, times 4
/// fixtures, is already ~100k probes (~100 s); stride 13 over the remaining ~24 KiB would add
/// another ~30k. Both are far outside a ~10 s budget.
///
/// The header (2048 bytes) is instead covered exhaustively by [`header_corruption`] (measured
/// ~4 s for the full 2048 * 4 = 8192 combinations, since -- per the note above -- most header
/// offsets are outside `_padding_2` and fail to parse before any blob is ever touched), so there is
/// no need for `corruption_scaffold` to also cover it densely. `DENSE_END` is 0 (no dense window)
/// and `STRIDE` alone sweeps the whole file, which still samples the header, the start of the
/// packet stream (where a `CompressedBlob`/`Keyframe` packet's own framing lives) and the bulk of
/// the blob payloads at a uniform, coarse resolution. Measured total: ~10k probes, ~6-7 s. If a
/// future regression needs finer coverage in a specific byte range, narrow `STRIDE` (or set
/// `DENSE_END` there) locally rather than lowering it here for every run.
const DENSE_END: usize = 0;
const STRIDE: usize = 47;

#[test]
fn corruption_scaffold() {
    for (name, original) in fixtures() {
        for offset in offsets_for(original.len(), DENSE_END, STRIDE) {
            for pattern in PATTERNS {
                let mut bytes = original.clone();
                if !apply_pattern(&mut bytes, offset, pattern) {
                    continue;
                }
                check_no_panic(format!("corruption_scaffold {name}: offset {offset}, pattern {pattern:?}"), || probe(&bytes));
            }
        }
    }
}

/// Corruption inside a blob's *decompressed* content (rather than the raw file bytes, most of
/// which are opaque compressed bytes that the outer patterns above rarely land inside a meaningful
/// boundary of): decompress the first blob of `V3_SMALL_CLOSED`, corrupt its plain packet bytes
/// (where `DeltaKeyframe.diff`/`counters` lengths and other in-blob fields actually live),
/// recompress, and rebuild a one-blob file to probe. This is the path the outer, whole-file sweep
/// almost never reaches, since it corrupts compressed bytes that usually just fail zstd's own
/// checksum/frame validation before the corrupted plaintext bytes would ever be parsed as packets.
#[test]
fn corruption_inside_blobs() {
    let player = ReplayFilePlayer::new(V3_SMALL_CLOSED, false).unwrap();
    let Packet::CompressedBlob {
        compressed_data,
        uncompressed_size,
        keyframes,
        keyframe_offsets,
        bookmarks,
        timestamp_start,
        timestamp_end,
        elapsed_frames_start,
        elapsed_frames_end,
    } = &player.all_uncompressed_packets()[0] else {
        panic!("expected the fixture's first packet to be a compressed blob");
    };

    let inner = decompress_data(compressed_data.as_slice(), *uncompressed_size as usize)
        .expect("the fixture's first blob should decompress cleanly");
    let frames: Vec<u64> = keyframes.iter().map(|k| k.elapsed_frames).collect();
    assert!(!frames.is_empty(), "the fixture's first blob should have keyframes");

    for offset in (0..inner.len()).step_by(7) {
        for pattern in PATTERNS {
            let mut patched = inner.clone();
            if !apply_pattern(&mut patched, offset, pattern) {
                continue;
            }
            let recompressed = compress_data(&patched, 1).expect("recompression should not fail");

            let corrupted_blob = Packet::CompressedBlob {
                keyframes: keyframes.clone(),
                keyframe_offsets: keyframe_offsets.clone(),
                bookmarks: bookmarks.clone(),
                compressed_data: ByteVec::Heap(recompressed),
                uncompressed_size: patched.len() as u64,
                timestamp_start: *timestamp_start,
                timestamp_end: *timestamp_end,
                elapsed_frames_start: *elapsed_frames_start,
                elapsed_frames_end: *elapsed_frames_end,
            };
            let bytes = file_with_packets(&[corrupted_blob]);

            check_no_panic(format!("corruption_inside_blobs: inner offset {offset}, pattern {pattern:?}"), || probe_frames(&bytes, &frames));
        }
    }
}

/// Every offset of the 2048-byte header, all patterns: cheap (a corrupted header almost always
/// fails to parse before any packet, let alone any blob, is ever touched), so this gets full
/// density regardless of [`STRIDE`].
#[test]
fn header_corruption() {
    let header_len = size_of::<ReplayHeaderBytes>();
    for offset in 0..header_len {
        for pattern in PATTERNS {
            let mut bytes = V3_SMALL_CLOSED.to_vec();
            if !apply_pattern(&mut bytes, offset, pattern) {
                continue;
            }
            check_no_panic(format!("header_corruption V3_SMALL_CLOSED: offset {offset}, pattern {pattern:?}"), || probe(&bytes));
        }
    }
}
