//! Replay file recording functionality.
//!
//! See [`ReplayFileRecorder`] and [`NonBlockingReplayFileRecorder`].

use crate::replay_file::bookmark_section::encode_bookmark_section;
use crate::replay_file::{ReplayFileMetadata, ReplayHeaderBytes, ReplayHeaderRaw};
use crate::{BookmarkTable, ByteVec, Counter, InputBuffer, KeyframeMetadata, Packet, PacketIO, PacketWriteCommand, SignedInteger, Speed, TimestampMillis, UnsignedInteger};
use alloc::string::String;
use alloc::borrow::Cow;
use alloc::vec::Vec;
use alloc::format;
use core::fmt::{Display, Formatter};
use zstd_sys::ZSTD_defaultCLevel;

#[cfg(not(feature = "std"))]
use spin::Lazy as LazyLock;

#[cfg(feature = "std")]
use std::sync::LazyLock;

#[cfg(feature = "std")]
mod thread;

#[cfg(feature = "std")]
pub use thread::*;

mod resume;
pub use resume::*;

#[cfg(feature = "std")]
use std::{
    io::{Seek, SeekFrom, Write},
    fs::File
};
use alloc::collections::BTreeMap;
use crate::keyframe_masks::apply_masks;
use crate::util::region_diff;

/// Records a replay file
///
/// IMPORTANT: To finish the stream, you must call [`ReplayFileRecorder::close`]. It is highly
/// recommended to also add a keyframe immediately before calling close so that the length of the
/// replay can be estimated accurately.
pub struct ReplayFileRecorder<Final: ReplayFileSink, Temp: ReplayFileSink> {
    settings: ReplayFileRecorderSettings,

    current_blob: Vec<u8>,
    current_blob_keyframes: Vec<KeyframeMetadata>,
    /// Byte offset in `current_blob` of each keyframe-class packet, parallel to
    /// `current_blob_keyframes` (the v6 offset table of the blob being built).
    current_blob_keyframe_offsets: Vec<u64>,
    current_blob_offset: u64,

    elapsed_frames: UnsignedInteger,
    elapsed_millis: TimestampMillis,
    last_keyframe_frames: UnsignedInteger,
    last_state_to_diff: Option<ByteVec>,

    /// The state buffer most recently displaced from `last_state_to_diff`, kept so the producer of
    /// keyframe states can take it back and reuse the allocation (see [`Self::take_recycled_state`]).
    recycled_state: Option<Vec<u8>>,

    current_speed: Speed,
    current_input: InputBuffer,

    sink: Option<SinkTuple<Final, Temp>>,
    header: ReplayHeaderRaw,

    counters: BTreeMap<String, SignedInteger>,

    /// The replay's bookmarks; written to the stream on every change and as the bookmark section
    /// when the file is closed.
    bookmarks: BookmarkTable,

    /// Whether a bookmark table was ever written in this recording; from then on every blob repeats
    /// the table after its first keyframe (see [`Packet::BookmarkTable`]).
    bookmarks_written: bool,

    /// A table change arrived while the in-progress blob had no keyframe yet (a blob must start
    /// with one), so the snapshot is written after the next keyframe instead.
    bookmark_snapshot_pending: bool,
}

/// How [`ReplayFileRecorder::insert_keyframe_with`] stores a keyframe.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum KeyframeEncoding {
    /// A region delta against the previous keyframe when that is smaller, else a full keyframe.
    #[default]
    Auto,

    /// Always a full keyframe, so seeking to it applies no deltas (keyframe bookmarks).
    Full
}

struct SinkTuple<Final: ReplayFileSink, Temp: ReplayFileSink> {
    final_sink: Final,
    temp_sink: Temp,

    /// Set when the final sink (the actual output file) fails a write; the failing operation
    /// propagates the error, and the in-memory data it was writing is retained so the SAME
    /// operation (a later `next_blob`, or `close`) can retry it. Cleared the next time a final-sink
    /// write succeeds.
    final_failed: bool,

    /// Set the first time the temp sink (the crash-safety scratch copy) fails a write. That first
    /// failure is surfaced once as [`ReplayFileWriteError::TempSink`]; afterwards temp writes are
    /// skipped silently (the recording itself is unaffected -- the temp file is disposable).
    temp_failed: Option<ReplayFileWriteError>
}

/// Settings for [`ReplayFileRecorder`]
#[derive(Clone, Debug)]
pub struct ReplayFileRecorderSettings {
    /// Hard cap on *actual* buffered uncompressed packet bytes before a blob is closed.
    ///
    /// (Format v3 counted every delta keyframe as a full state here; that estimator is gone, so
    /// this now only bounds the recorder's in-progress buffer and the player's per-chain memory.)
    ///
    /// Default is [`DEFAULT_MINIMUM_UNCOMPRESSED_BYTES_PER_BLOB`]
    pub minimum_uncompressed_bytes_per_blob: usize,

    /// Close the blob at the first keyframe at or after this many frames since the blob's first
    /// keyframe. `0` = unlimited.
    ///
    /// This bounds the length of a delta chain, i.e. how many deltas a cold seek may have to apply
    /// (and, since format v6, how much of a blob it may have to decompress: a seek decodes only up
    /// to its keyframe).
    ///
    /// Default is [`DEFAULT_MAX_FRAMES_PER_BLOB`] (30 minutes at 60 fps).
    pub max_frames_per_blob: u64,

    /// zstd compression level
    ///
    /// Default is [`DEFAULT_ZSTD_COMPRESSION_LEVEL_V4`]
    pub compression_level: i32,

    /// Copy regenerated output buffers (see [`crate::keyframe_masks`]) from the previous keyframe
    /// before diffing, so delta keyframes do not carry them.
    ///
    /// Restart keyframes stay exact; a delta keyframe then reconstructs with the restart's copy of
    /// those buffers, which the game overwrites on its next frame. Purely a size/cosmetic trade;
    /// determinism is unaffected. Default `true`.
    pub mask_transient_buffers: bool
}

/// Default minimum uncompressed bytes per blob
pub const DEFAULT_MINIMUM_UNCOMPRESSED_BYTES_PER_BLOB: usize = 1024 * 1024 * 1024;

/// Default maximum frames per blob (30 minutes at 60 fps).
///
/// Every blob starts with a full keyframe (about 1.75 MiB compressed for a Nintendo DS state),
/// which in a masked file is a third of a 15-minute blob, and zstd matches deltas against every
/// earlier one in the blob; so on a 2h14 HeartGold replay 15-minute blobs give 192 MiB, 30-minute
/// ones 143 MiB and 60-minute ones 117 MiB. With the v6 offset table a cold seek decodes only up
/// to its keyframe, so 30 minutes seeks faster than 15 minutes did without it (measured 2026-09-17:
/// 68 ms vs 97 ms in the core, 60 minutes = 101 ms).
pub const DEFAULT_MAX_FRAMES_PER_BLOB: u64 = 108_000;

/// Default compression level for format v4 files.
///
/// Level 9 costs nothing measurable on region-diff data (132-177 MiB/s) and is 8-18% smaller than
/// zstd's own default of 3.
pub const DEFAULT_ZSTD_COMPRESSION_LEVEL_V4: i32 = 9;

/// zstd's own default compression level
///
/// This is generally going to be equal to `3`. Format v3 files were written with it; v4 defaults
/// to [`DEFAULT_ZSTD_COMPRESSION_LEVEL_V4`].
pub static DEFAULT_ZSTD_COMPRESSION_LEVEL: LazyLock<i32> = LazyLock::new(|| unsafe { ZSTD_defaultCLevel() } as i32);

impl<Final: ReplayFileSink, Temp: ReplayFileSink> ReplayFileRecorder<Final, Temp> {
    /// Start a new replay file.
    pub fn new_with_metadata(
        replay_file_metadata: ReplayFileMetadata,
        patch_data: ByteVec,
        settings: ReplayFileRecorderSettings,
        starting_timestamp: TimestampMillis,
        starting_input: InputBuffer,
        starting_speed: Speed,
        initial_keyframe_state: ByteVec,
        final_sink: Final,
        temp_sink: Temp
    ) -> Result<ReplayFileRecorder<Final, Temp>, ReplayFileWriteError> {
        let mut recorder = Self::new_blank(
            replay_file_metadata,
            patch_data,
            settings,
            starting_input,
            starting_speed,
            final_sink,
            temp_sink
        )?;

        recorder.insert_keyframe(
            initial_keyframe_state,
            starting_timestamp
        )?;

        Ok(recorder)
    }

    /// Start a new replay file, writing the header and patch to both sinks but WITHOUT inserting an
    /// initial frame-0 keyframe.
    ///
    /// This is the low-level entry point used by the resume machinery (see
    /// [`build_resumed_recorder`](crate::replay_file::record::build_resumed_recorder)), which fills
    /// in the leading data itself — either by copying completed compressed blobs verbatim or by
    /// re-feeding packets. Ordinary recording should use [`Self::new_with_metadata`], which inserts
    /// the frame-0 keyframe for you.
    pub(crate) fn new_blank(
        replay_file_metadata: ReplayFileMetadata,
        patch_data: ByteVec,
        mut settings: ReplayFileRecorderSettings,
        starting_input: InputBuffer,
        starting_speed: Speed,
        mut final_sink: Final,
        mut temp_sink: Temp
    ) -> Result<ReplayFileRecorder<Final, Temp>, ReplayFileWriteError> {
        if settings.minimum_uncompressed_bytes_per_blob == 0 {
            settings.minimum_uncompressed_bytes_per_blob = 1024 * 1024 * 512;
        }

        let mut metadata = replay_file_metadata
            .as_raw_header()
            .map_err(|e| ReplayFileWriteError::Other { explanation: Cow::Owned(e) })?;

        metadata.patch_data_length = u64::try_from(patch_data.len())
            .map_err(|_| ReplayFileWriteError::Other { explanation: Cow::Borrowed("patch data too large") })?;

        let metadata_bytes = metadata.as_bytes();
        let current_blob_offset = metadata_bytes.len() + patch_data.len();

        // The header/patch write is final-sink-first, temp-sink-second, same as every other write
        // (see `SinkTuple`): a final-sink failure here fails construction outright (there is no
        // recorder yet to retry through), while a temp-sink failure is only recorded -- the
        // recording can still proceed from a working final sink alone. There is no recorder to
        // return the one-time `TempSink` notice through yet, so it first surfaces (if it hasn't
        // already been superseded) on the first write after construction.
        let mut temp_failed = None;
        fn try_temp<T: ReplayFileSink>(sink: &mut T, failed: &mut Option<ReplayFileWriteError>, bytes: &[u8]) {
            if failed.is_some() {
                return;
            }
            if let Err(e) = sink.write_bytes(bytes) {
                *failed = Some(e);
            }
        }

        try_temp(&mut temp_sink, &mut temp_failed, metadata_bytes.as_slice());
        final_sink.write_bytes(metadata_bytes.as_slice())?;

        try_temp(&mut temp_sink, &mut temp_failed, patch_data.as_slice());
        final_sink.write_bytes(patch_data.as_slice())?;

        Ok(ReplayFileRecorder {
            settings,
            elapsed_frames: 0,
            elapsed_millis: 0.into(),
            last_keyframe_frames: 0,
            current_speed: starting_speed,
            current_input: starting_input,
            current_blob: Vec::new(),
            current_blob_keyframes: Vec::new(),
            current_blob_keyframe_offsets: Vec::new(),
            current_blob_offset: u64::try_from(current_blob_offset).expect("failed to read"),
            header: metadata,
            last_state_to_diff: None,
            recycled_state: None,
            counters: BTreeMap::new(),
            bookmarks: BookmarkTable::new(),
            bookmarks_written: false,
            bookmark_snapshot_pending: false,
            sink: Some(SinkTuple {
                final_sink, temp_sink, final_failed: false, temp_failed
            })
        })
    }

    /// Append an already-compressed blob packet to both sinks verbatim, without decompressing it.
    ///
    /// Used by the resume fast path to carry the source replay's completed blobs forward unchanged.
    /// The blob is re-serialized via its packet-write instructions (the same path
    /// [`Self::next_blob`] uses), so the result is a byte-for-byte valid blob. `blob` must be a
    /// [`Packet::CompressedBlob`]; passing anything else is a programming error.
    ///
    /// This must only be called while the in-progress blob is empty (i.e. before any packets have
    /// been written to `current_blob`), which is the case during the verbatim-copy phase of a
    /// resume.
    pub(crate) fn append_compressed_blob_verbatim(&mut self, blob: &Packet) -> Result<(), ReplayFileWriteError> {
        debug_assert!(matches!(blob, Packet::CompressedBlob { .. }), "append_compressed_blob_verbatim given a non-blob packet");
        debug_assert!(self.current_blob.is_empty(), "append_compressed_blob_verbatim called with a non-empty in-progress blob");

        self.assert_not_closed()?;
        self.refuse_if_final_failed()?;

        let write_instructions = blob.write_packet_instructions();
        let offset = self.current_blob_offset;

        // Keep the temp file identical to the final file during the verbatim-copy phase: there is
        // no in-progress region yet, so truncating to the current offset is a no-op that simply
        // guards against any stray trailing bytes before we re-append the blob.
        let written = self.final_write(|final_sink| {
            final_sink.truncate(offset)?;
            let written = final_sink.write_packet_data(&write_instructions)?;
            final_sink.flush()?;
            Ok(written)
        })?;
        let written = u64::try_from(written).expect("failing to convert written blob size from usize to u64");
        self.current_blob_offset = offset.checked_add(written).expect("overflowed adding current_blob_offset");

        self.temp_write(|temp_sink| {
            temp_sink.truncate(offset)?;
            temp_sink.write_packet_data(&write_instructions)?;
            Ok(())
        })
    }

    /// Prime the recorder's running state to continue from an existing keyframe (resume support).
    ///
    /// Sets the elapsed frame/time counters, the current input/speed, and the counter snapshot to
    /// the values recorded at `kf`, and clears the in-progress blob so the next inserted keyframe
    /// begins a fresh blob with a full (undiffed) keyframe — which the reader requires as the first
    /// packet of every blob.
    pub(crate) fn prime_for_resume(&mut self, kf: &KeyframeMetadata) {
        self.elapsed_frames = kf.elapsed_frames;
        self.elapsed_millis = kf.elapsed_millis;
        self.last_keyframe_frames = kf.elapsed_frames;
        self.current_input = kf.input.clone();
        self.current_speed = kf.speed;
        self.counters = kf.counters.iter().map(|c| (c.name.clone(), c.value)).collect();
        self.last_state_to_diff = None;
        self.current_blob.clear();
        self.current_blob_keyframes.clear();
        self.current_blob_keyframe_offsets.clear();
    }

    /// Start the resumed recording's bookmarks with `table` (resume support).
    ///
    /// Unlike [`Self::set_bookmark_table`] this always writes a snapshot (after the next keyframe),
    /// even for a table equal to the current one: blobs copied verbatim from the source may carry
    /// snapshots of bookmarks that the resumed file must not recover.
    pub(crate) fn seed_bookmark_table(&mut self, table: BookmarkTable) {
        self.bookmarks = table;
        self.bookmarks_written = true;
        self.bookmark_snapshot_pending = true;
    }

    /// Returns `true` if the stream was closed.
    #[inline]
    pub fn is_closed(&self) -> bool {
        self.sink.is_none()
    }

    /// Close the replay file recorder.
    ///
    /// Flushes the in-progress blob, then records the end of the packet stream in the header and
    /// appends the bookmark section, in that order: if the process dies in between, the section is
    /// merely invalid and players recover the bookmarks from the stream.
    ///
    /// You can no longer write to this.
    ///
    /// # Panics
    ///
    /// Panics if already closed.
    pub fn close(&mut self) -> Result<(Final, Temp), (Final, Temp, ReplayFileWriteError)> {
        assert!(!self.is_closed(), "Already closed...");

        // Flush the final in-progress blob (now a no-op if nothing is buffered). Capture the result
        // BEFORE taking the sinks; calling next_blob() again after the take would just fail
        // assert_not_closed and spuriously report the close as failed.
        //
        // A temp-sink-only failure (`TempSink`) must not stop the bookmark section from being
        // written -- the final file is unaffected by it, and the temp file is disposable scratch.
        // Only a genuine final-sink failure skips straight to returning the sinks.
        let blob_result = self.next_blob();
        let section_result = if matches!(blob_result, Err(ref e) if !is_temp_sink_error(e)) {
            Ok(())
        }
        else {
            self.write_bookmark_section()
        };

        let Some(SinkTuple { final_sink, temp_sink, .. }) = self.sink.take() else {
            unreachable!();
        };

        // The first genuine (non-`TempSink`) error from either step is what makes close() fail; a
        // `TempSink` notice (already delivered to the caller once, from whichever call produced it)
        // does not.
        let final_error = [blob_result, section_result].into_iter().find_map(|r| match r {
            Err(e) if !is_temp_sink_error(&e) => Some(e),
            _ => None,
        });

        match final_error {
            None => Ok((final_sink, temp_sink)),
            Some(e) => Err((final_sink, temp_sink, e)),
        }
    }

    /// Returns true if an unrecoverable error occurred: the final sink failed and has not since
    /// succeeded, or the recorder is closed. A temp-sink-only failure does not count (see
    /// [`Self::temp_sink_failed`]).
    pub fn is_poisoned(&self) -> bool {
        self.sink.as_ref().map(|s| s.final_failed).unwrap_or(true)
    }

    /// The temp sink's own error, if it has ever failed a write. `None` once the recorder is
    /// closed (the temp sink, and its failure state, no longer exist).
    pub fn temp_sink_failed(&self) -> Option<&ReplayFileWriteError> {
        self.sink.as_ref().and_then(|s| s.temp_failed.as_ref())
    }

    /// Refuse to attempt a final-sink write when a previous one is still unresolved. Used by
    /// everything except [`Self::next_blob`], which is the operation that retries (and clears) a
    /// stale failure; gating it too would make that retry impossible.
    fn refuse_if_final_failed(&self) -> Result<(), ReplayFileWriteError> {
        if self.sink.as_ref().is_some_and(|s| s.final_failed) {
            Err(ReplayFileWriteError::Poisoned)
        }
        else {
            Ok(())
        }
    }

    /// Attempt a write to the final sink. Clears the sink's `final_failed` flag on success, sets it
    /// on failure (the caller's in-memory data is left for the caller to decide whether to retry).
    fn final_write<T, F: FnOnce(&mut Final) -> Result<T, ReplayFileWriteError>>(&mut self, f: F) -> Result<T, ReplayFileWriteError> {
        self.assert_not_closed()?;
        let sink = self.sink.as_mut().expect("checked by assert_not_closed");
        match f(&mut sink.final_sink) {
            Ok(v) => {
                sink.final_failed = false;
                Ok(v)
            }
            Err(e) => {
                sink.final_failed = true;
                Err(e)
            }
        }
    }

    /// Attempt a write to the temp sink. Once the temp sink has failed, every later call here is a
    /// silent no-op (`Ok(())`): the first failure is the only one ever surfaced, as
    /// [`ReplayFileWriteError::TempSink`], and the recording continues regardless (the temp file is
    /// a disposable scratch copy).
    fn temp_write<F: FnOnce(&mut Temp) -> Result<(), ReplayFileWriteError>>(&mut self, f: F) -> Result<(), ReplayFileWriteError> {
        self.assert_not_closed()?;
        let sink = self.sink.as_mut().expect("checked by assert_not_closed");
        if sink.temp_failed.is_some() {
            return Ok(());
        }
        match f(&mut sink.temp_sink) {
            Ok(()) => Ok(()),
            Err(e) => {
                let explanation = Cow::Owned(format!("Temp file error: {e}"));
                sink.temp_failed = Some(e);
                Err(ReplayFileWriteError::TempSink { explanation })
            }
        }
    }

    /// End the packet stream: record its end in the header, then append the bookmark section.
    fn write_bookmark_section(&mut self) -> Result<(), ReplayFileWriteError> {
        self.header.packet_stream_end = self.current_blob_offset;
        let header_result = self.sync_header();
        if let Err(e) = &header_result {
            if !is_temp_sink_error(e) {
                return header_result;
            }
        }

        let section = encode_bookmark_section(&self.bookmarks);
        self.final_write(|final_sink| final_sink.write_bytes(&section))?;
        let section_result = self.temp_write(|temp_sink| temp_sink.write_bytes(&section));

        // At most one of these is ever `Err` here: `temp_write` only reports a temp failure once,
        // and whichever of the two calls hit it first is the one that gets to report it.
        header_result.and(section_result)
    }

    /// The replay's bookmarks as last set.
    pub fn bookmark_table(&self) -> &BookmarkTable {
        &self.bookmarks
    }

    /// Replace the replay's bookmarks.
    ///
    /// Does nothing if `table` equals the current table. Otherwise the table is written to the
    /// stream as a [`Packet::BookmarkTable`] (after the next keyframe, if the in-progress blob has
    /// none yet), repeated after the first keyframe of every later blob, and saved as the bookmark
    /// section when the file is closed.
    pub fn set_bookmark_table(&mut self, table: BookmarkTable) -> Result<(), ReplayFileWriteError> {
        self.assert_not_closed()?;
        if table == self.bookmarks {
            return Ok(())
        }

        self.bookmarks = table;
        self.bookmarks_written = true;

        if self.current_blob_keyframes.is_empty() {
            self.bookmark_snapshot_pending = true;
            return Ok(())
        }

        self.write_bookmark_snapshot()
    }

    fn write_bookmark_snapshot(&mut self) -> Result<(), ReplayFileWriteError> {
        self.bookmark_snapshot_pending = false;
        let packet = Packet::BookmarkTable { table: self.bookmarks.clone() };
        self.write_packet_data(&packet)
    }

    /// Advance a new frame.
    pub fn next_frame(&mut self, timestamp: TimestampMillis) -> Result<(), ReplayFileWriteError> {
        let elapsed_old = self.elapsed_millis;
        let timestamp_delta = timestamp.0.checked_sub(self.elapsed_millis.0)
            .ok_or_else(|| ReplayFileWriteError::BadInput { explanation: Cow::Owned(format!("Timestamp overflowed ({elapsed_old} -> {timestamp}")) })?;

        self.elapsed_frames += 1;
        self.elapsed_millis = timestamp;

        self.write_packet_data(&Packet::NextFrame { timestamp_delta: timestamp_delta.into() })
    }

    /// Add a new keyframe.
    ///
    /// The keyframe is written as a [`Packet::RegionDeltaKeyframe`] against the previous keyframe
    /// of the current blob whenever that is smaller than the state itself (it practically always
    /// is), and as a full [`Packet::Keyframe`] otherwise: at the start of every blob, when the
    /// state length changed, or when the state was rewritten wholesale.
    ///
    /// Returns the frame index the keyframe is on.
    pub fn insert_keyframe(&mut self, state: ByteVec, elapsed_millis: TimestampMillis) -> Result<u64, ReplayFileWriteError> {
        self.insert_keyframe_with(state, elapsed_millis, KeyframeEncoding::Auto)
    }

    /// Add a new keyframe stored as `encoding` asks (see [`Self::insert_keyframe`]).
    ///
    /// A [`KeyframeEncoding::Full`] keyframe may share its frame with a keyframe written just before
    /// it; seeking to that frame uses the last one.
    ///
    /// Returns the frame index the keyframe is on.
    pub fn insert_keyframe_with(&mut self, state: ByteVec, elapsed_millis: TimestampMillis, encoding: KeyframeEncoding) -> Result<u64, ReplayFileWriteError> {
        self.assert_not_closed()?;
        if elapsed_millis < self.elapsed_millis {
            return Err(ReplayFileWriteError::BadInput { explanation: Cow::Owned(format!("keyframe timestamp went backwards ({} -> {elapsed_millis})", self.elapsed_millis)) })
        }

        self.elapsed_millis = elapsed_millis;
        self.last_keyframe_frames = self.elapsed_frames;

        if self.blob_limit_reached() {
            self.next_blob()?;
            let displaced = self.last_state_to_diff.take();
            self.recycle_state(displaced);
        }

        let metadata = KeyframeMetadata {
            input: self.current_input.clone(),
            speed: self.current_speed,
            elapsed_frames: self.elapsed_frames,
            elapsed_millis,
            counters: self.counters.iter().map(|i| Counter {
                name: i.0.clone(),
                value: *i.1
            }).collect()
        };

        self.current_blob_keyframes.push(metadata.clone());
        self.current_blob_keyframe_offsets.push(u64::try_from(self.current_blob.len()).expect("blob offset exceeds u64"));

        let mut state = state;
        let mut masked = Vec::new();
        let delta = match self.last_state_to_diff.as_ref() {
            Some(previous) if previous.len() == state.len() && encoding == KeyframeEncoding::Auto => {
                if self.settings.mask_transient_buffers {
                    masked = apply_masks(self.header.console_type.get_or_default(), previous.as_slice(), state.as_mut_slice());
                }
                let diff = region_diff(previous.as_slice(), state.as_slice()).expect("lengths were checked");
                (diff.encoded_len() < state.len()).then_some(diff)
            },
            _ => None
        };

        if let Some(diff) = delta {
            self.write_packet_data(&Packet::RegionDeltaKeyframe {
                metadata,
                state_len: u64::try_from(state.len()).expect("state length exceeds u64"),
                control: ByteVec::Heap(diff.control),
                data: ByteVec::Heap(diff.data)
            })?;
        }
        else {
            // Falling back to a full keyframe: restore whatever `apply_masks` overwrote above, so
            // the stored (and, below, diffed-against) state is exact -- full keyframes are
            // documented to reconstruct bit-exactly, and masking must never leak into one.
            if !masked.is_empty() {
                let state = state.as_mut_slice();
                for (range, original) in &masked {
                    state[range.clone()].copy_from_slice(original);
                }
            }
            self.write_packet_data(&Packet::Keyframe {
                metadata,
                state: state.clone()
            })?;
        }

        // The next delta is taken against the state exactly as the player will reconstruct it
        // (masked, if it was masked).
        let displaced = self.last_state_to_diff.replace(state);
        self.recycle_state(displaced);

        // A blob that starts after bookmarks were written repeats the table, so a file that is
        // never closed can recover it from its last blob alone.
        let first_in_blob = self.current_blob_keyframes.len() == 1;
        if self.bookmark_snapshot_pending || (first_in_blob && self.bookmarks_written) {
            self.write_bookmark_snapshot()?;
        }

        Ok(self.elapsed_frames)
    }

    fn recycle_state(&mut self, displaced: Option<ByteVec>) {
        if let Some(ByteVec::Heap(buffer)) = displaced {
            self.recycled_state = Some(buffer);
        }
    }

    /// Take back a state buffer that the recorder no longer needs, so the next keyframe can be
    /// written into an allocation that is already mapped. Only heap-allocated states are ever
    /// recycled; returns `None` when there is nothing to hand back.
    pub fn take_recycled_state(&mut self) -> Option<Vec<u8>> {
        self.recycled_state.take()
    }

    /// Whether the in-progress blob should be closed before the next keyframe: it has run for
    /// `max_frames_per_blob` frames since its first keyframe, or buffers at least
    /// `minimum_uncompressed_bytes_per_blob` bytes.
    fn blob_limit_reached(&self) -> bool {
        let Some(first_keyframe) = self.current_blob_keyframes.first() else {
            return false
        };

        let frame_limit = self.settings.max_frames_per_blob;
        let frames_in_blob = self.elapsed_frames.saturating_sub(first_keyframe.elapsed_frames);

        (frame_limit > 0 && frames_in_blob >= frame_limit)
            || self.current_blob.len() >= self.settings.minimum_uncompressed_bytes_per_blob
    }

    /// Compress and flush the in-progress blob to both sinks, if it holds any keyframes.
    ///
    /// The final sink is written (and flushed) FIRST; only once that has actually succeeded are
    /// `current_blob` / `current_blob_keyframes` cleared and `current_blob_offset` advanced. If the
    /// final write fails, this returns `Err` with the blob left exactly as it was: the next call
    /// that reaches `next_blob` (the next keyframe that hits the blob limit, or [`Self::close`])
    /// retries the identical bytes. A stale final-sink failure never blocks this retry (unlike
    /// every other final-sink operation, which refuses to attempt while one is unresolved) --
    /// `next_blob` is what resolves it.
    fn next_blob(&mut self) -> Result<(), ReplayFileWriteError> {
        self.assert_not_closed()?;

        // Nothing buffered (e.g. closing immediately after a blob split): a blob with no keyframes
        // is invalid and would panic below, so there is simply nothing to flush.
        if self.current_blob_keyframes.is_empty() {
            return Ok(());
        }

        let uncompressed_size = self.current_blob.len();
        let compressed = crate::compress_data(self.current_blob.as_slice(), self.settings.compression_level)
            .map_err(|e| ReplayFileWriteError::Other { explanation: Cow::Owned(format!("next_blob failed to compress: {e}")) })?;

        let (first_frames, first_millis) = {
            let first_keyframe = self.current_blob_keyframes.first().expect("no keyframes in blob?");
            (first_keyframe.elapsed_frames, first_keyframe.elapsed_millis)
        };

        let compressed_blob = Packet::CompressedBlob {
            elapsed_frames_start: first_frames,
            elapsed_frames_end: self.elapsed_frames,
            timestamp_start: first_millis,
            timestamp_end: self.elapsed_millis,

            // Cloned, not taken: if the final-sink write below fails, `current_blob_keyframes` must
            // still hold the whole blob so a later call can retry it (see the doc comment above).
            keyframes: self.current_blob_keyframes.clone(),
            keyframe_offsets: self.current_blob_keyframe_offsets.clone(),
            // Format v5 keeps bookmarks in `BookmarkTable` packets and the bookmark section.
            bookmarks: Vec::new(),
            compressed_data: ByteVec::Heap(compressed),
            uncompressed_size: u64::try_from(uncompressed_size).expect("failed to convert uncompressed_size from usize to u64"),
        };

        let write_instructions = compressed_blob.write_packet_instructions();
        let offset = self.current_blob_offset;

        let written = self.final_write(|final_sink| {
            // Undo any bytes a previous failed attempt at this same blob left behind, so a retry
            // does not duplicate them.
            final_sink.truncate(offset)?;
            let written = final_sink.write_packet_data(&write_instructions)?;
            final_sink.flush()?;
            Ok(written)
        })?;
        let written = u64::try_from(written).expect("failing to convert written packet data from usize to u64");

        // Only now that the final sink safely holds the blob do we clear it and advance past it.
        self.current_blob.clear();
        let keyframes_len = self.current_blob_keyframes.len();
        self.current_blob_keyframes.clear();
        self.current_blob_keyframes.reserve(keyframes_len + 1024);
        self.current_blob_keyframe_offsets.clear();
        self.current_blob_offset = offset.checked_add(written).expect("overflowed adding current_blob_offset");

        self.temp_write(|temp_sink| {
            temp_sink.truncate(offset)?;
            temp_sink.write_packet_data(&write_instructions)?;
            Ok(())
        })
    }

    /// Set the current input.
    pub fn set_input(&mut self, input_buffer: InputBuffer) -> Result<(), ReplayFileWriteError> {
        self.current_input = input_buffer.clone();
        self.write_packet_data(&Packet::ChangeInput { data: input_buffer })
    }

    /// Hard-reset the console.
    pub fn reset_console(&mut self) -> Result<(), ReplayFileWriteError> {
        self.write_packet_data(&Packet::ResetConsole)
    }

    /// Write RAM to an address.
    pub fn write_memory(&mut self, address: UnsignedInteger, data: ByteVec) -> Result<(), ReplayFileWriteError> {
        self.write_packet_data(&Packet::WriteMemory { address, data })
    }

    /// Set the current speed.
    pub fn set_speed(&mut self, speed: Speed) -> Result<(), ReplayFileWriteError> {
        if self.current_speed == speed {
            return Ok(())
        }

        self.current_speed = speed;
        self.write_packet_data(&Packet::ChangeSpeed { speed })
    }

    /// Load a given save state immediately.
    pub fn load_save_state(&mut self, state: ByteVec) -> Result<(), ReplayFileWriteError> {
        self.write_packet_data(&Packet::LoadSaveState { state })
    }

    /// What the console received over its link cable during the frame being recorded (see
    /// [`Packet::SerialIn`]); written before that frame's [`Self::next_frame`]. Nothing is written
    /// for empty `data`.
    pub fn serial_in(&mut self, data: ByteVec) -> Result<(), ReplayFileWriteError> {
        if data.is_empty() {
            return Ok(())
        }
        self.write_packet_data(&Packet::SerialIn { data })
    }

    fn write_packet_data<'a, P: PacketIO<'a>>(&mut self, what: &'a P) -> Result<(), ReplayFileWriteError> {
        self.write_packet_unchecked(what)
    }

    /// Write a packet to the in-memory in-progress blob, then mirror it to the temp sink.
    ///
    /// Never touches the final sink: per-packet data only ever reaches the final sink as part of a
    /// whole compressed blob (see [`Self::next_blob`]). A temp-sink failure here is recorded and
    /// surfaced once (as [`ReplayFileWriteError::TempSink`]) but never stops the recording.
    fn write_packet_unchecked<'a, P: PacketIO<'a>>(&mut self, what: &'a P) -> Result<(), ReplayFileWriteError> {
        self.assert_not_closed()?;
        let instructions = what.write_packet_instructions();
        self.current_blob.write_packet_data(&instructions)?;
        self.temp_write(|temp_sink| {
            temp_sink.write_packet_data(&instructions)?;
            Ok(())
        })
    }

    fn assert_not_closed(&self) -> Result<(), ReplayFileWriteError> {
        if self.is_closed() {
            Err(ReplayFileWriteError::StreamClosed)
        }
        else {
            Ok(())
        }
    }

    /// Mark the current position as the start.
    pub fn mark_start(&mut self, timer_offset: TimestampMillis) -> Result<(), ReplayFileWriteError> {
        self.header.crop_start_frame = self.elapsed_frames;
        self.header.crop_start_millis = self.elapsed_millis;
        self.header.crop_start = 1;
        self.header.crop_timer_offset = timer_offset;
        self.sync_header()
    }

    /// Mark the current position as the end.
    pub fn mark_end(&mut self) -> Result<(), ReplayFileWriteError> {
        self.header.crop_end_frame = self.elapsed_frames;
        self.header.crop_end_millis = self.elapsed_millis;
        self.header.crop_end = 1;
        self.sync_header()
    }

    /// Rewrite the header in place (final sink first, then temp). Refuses to attempt this while a
    /// previous final-sink failure is unresolved (see [`Self::refuse_if_final_failed`]).
    fn sync_header(&mut self) -> Result<(), ReplayFileWriteError> {
        self.assert_not_closed()?;
        self.refuse_if_final_failed()?;

        let header_bytes = *self.header.as_bytes();
        self.final_write(|final_sink| final_sink.overwrite_header(&header_bytes))?;
        self.temp_write(|temp_sink| temp_sink.overwrite_header(&header_bytes))
    }

    /// Modify the counter.
    pub fn change_counter(&mut self, name: String, delta: SignedInteger) -> Result<(), ReplayFileWriteError> {
        self.assert_not_closed()?;
        if let Some(v) = self.counters.get_mut(&name) {
            *v = v.wrapping_add(delta);
        }
        else {
            self.counters.insert(name.clone(), delta);
        }

        self.write_packet_unchecked(&Packet::IncrementCounter { name, delta })
    }
}

/// Whether `error` is the one-time notice from a temp-sink-only failure (see
/// [`ReplayFileRecorder::temp_write`]) rather than a genuine (final-sink or otherwise fatal) error.
fn is_temp_sink_error(error: &ReplayFileWriteError) -> bool {
    matches!(error, ReplayFileWriteError::TempSink { .. })
}

impl Default for ReplayFileRecorderSettings {
    fn default() -> Self {
        Self {
            minimum_uncompressed_bytes_per_blob: DEFAULT_MINIMUM_UNCOMPRESSED_BYTES_PER_BLOB,
            max_frames_per_blob: DEFAULT_MAX_FRAMES_PER_BLOB,
            compression_level: DEFAULT_ZSTD_COMPRESSION_LEVEL_V4,
            mask_transient_buffers: true,
        }
    }
}

/// Describes something that can store bytes contiguously, making it suitable for a replay file.
pub trait ReplayFileSink {
    /// Writes bytes to the end of the sink.
    fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), ReplayFileWriteError>;

    /// Truncates the sink to the given size.
    fn truncate(&mut self, size: u64) -> Result<(), ReplayFileWriteError>;

    /// Write the header at the start of the file.
    fn overwrite_header(&mut self, header_data: &ReplayHeaderBytes) -> Result<(), ReplayFileWriteError>;

    /// Writes the given packet data.
    fn write_packet_data(&mut self, instructions: &[PacketWriteCommand<'_>]) -> Result<usize, ReplayFileWriteError> {
        let mut written = 0usize;
        for i in instructions {
            let bytes = i.bytes();
            self.write_bytes(bytes)?;
            written += bytes.len();
        }
        Ok(written)
    }

    /// Flush any buffered writes so they are actually visible to whatever will read this sink back
    /// (e.g. a later [`Self::truncate`]/[`Self::overwrite_header`], or another process). The default
    /// is a no-op, for sinks with nothing to flush (an in-memory buffer, or a raw unbuffered file);
    /// a buffered file sink overrides this.
    fn flush(&mut self) -> Result<(), ReplayFileWriteError> {
        Ok(())
    }
}

impl ReplayFileSink for Vec<u8> {
    fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), ReplayFileWriteError> {
        self.try_reserve(bytes.len()).map_err(|_| ReplayFileWriteError::Other { explanation: Cow::Borrowed("write_bytes failed to reserve memory") })?;
        self.extend_from_slice(bytes);
        Ok(())
    }

    #[inline]
    fn truncate(&mut self, size: u64) -> Result<(), ReplayFileWriteError> {
        self.truncate(usize::try_from(size).expect("converting u64 to usize should work when truncating"));
        Ok(())
    }

    fn write_packet_data(&mut self, instructions: &[PacketWriteCommand<'_>]) -> Result<usize, ReplayFileWriteError> {
        let mut total_len = 0usize;
        for i in instructions {
            total_len = total_len.saturating_add(i.bytes().len());
        }
        self.try_reserve(total_len).map_err(|_| ReplayFileWriteError::Other { explanation: Cow::Borrowed("write_packet_data failed to reserve memory") })?;
        for i in instructions {
            self.extend_from_slice(i.bytes())
        }
        Ok(total_len)
    }

    fn overwrite_header(&mut self, header_data: &ReplayHeaderBytes) -> Result<(), ReplayFileWriteError> {
        if self.len() > header_data.len() {
            self[..header_data.len()].copy_from_slice(header_data);
        }
        else {
            self.clear();
            self.extend_from_slice(header_data);
        }
        Ok(())
    }
}

#[cfg(feature = "std")]
impl ReplayFileSink for File {
    fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), ReplayFileWriteError> {
        self.write_all(bytes)?;
        Ok(())
    }

    fn truncate(&mut self, size: u64) -> Result<(), ReplayFileWriteError> {
        self.set_len(size)?;
        self.seek(SeekFrom::End(0))?;
        Ok(())
    }

    fn overwrite_header(&mut self, header_data: &ReplayHeaderBytes) -> Result<(), ReplayFileWriteError> {
        self.seek(SeekFrom::Start(0))?;
        self.write_all(header_data.as_slice())?;
        self.seek(SeekFrom::End(0))?;
        Write::flush(self)?;
        Ok(())
    }
}

#[cfg(feature = "std")]
impl ReplayFileSink for std::io::BufWriter<File> {
    fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), ReplayFileWriteError> {
        self.write_all(bytes)?;
        Ok(())
    }

    fn truncate(&mut self, size: u64) -> Result<(), ReplayFileWriteError> {
        Write::flush(self)?;

        let this = self.get_mut();
        this.set_len(size)?;
        this.seek(SeekFrom::End(0))?;
        Ok(())
    }

    fn overwrite_header(&mut self, header_data: &ReplayHeaderBytes) -> Result<(), ReplayFileWriteError> {
        Write::flush(self)?;
        let this = self.get_mut();
        this.overwrite_header(header_data)
    }

    fn flush(&mut self) -> Result<(), ReplayFileWriteError> {
        Write::flush(self)?;
        Ok(())
    }
}

#[cfg(feature = "std")]
impl From<std::io::Error> for ReplayFileWriteError {
    fn from(value: std::io::Error) -> Self {
        Self::Other { explanation: Cow::Owned(format!("I/O error: {value}")) }
    }
}

/// Describes an error that occurred when writing
#[derive(Clone, PartialEq, Debug)]
pub enum ReplayFileWriteError {
    /// Bad input was given. The stream might still be functional.
    #[allow(missing_docs)]
    BadInput { explanation: Cow<'static, str> },

    /// The stream has closed. The stream is no longer functional.
    StreamClosed,

    /// A previous write to the final sink failed and has not since succeeded, so this operation
    /// was refused without being attempted. Unlike the other variants, this is not necessarily
    /// permanent: a later call to [`ReplayFileRecorder::close`] (or the keyframe that next closes a
    /// blob) retries the failed write, and once that succeeds the stream is usable again.
    Poisoned,

    /// The temp sink (the non-final, crash-safety copy) failed a write. The recording is
    /// unaffected: temp writes are skipped silently from here on, and the final file is written
    /// normally. Surfaced only once, the first time it happens; see
    /// [`ReplayFileRecorder::temp_sink_failed`].
    #[allow(missing_docs)]
    TempSink { explanation: Cow<'static, str> },

    /// Some other error occurred. The stream is no longer functional.
    #[allow(missing_docs)]
    Other { explanation: Cow<'static, str> }
}

impl Display for ReplayFileWriteError {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        match self {
            ReplayFileWriteError::BadInput { explanation } => f.write_fmt(format_args!("Bad input: {explanation}")),
            ReplayFileWriteError::StreamClosed => f.write_str("The stream has closed"),
            ReplayFileWriteError::Poisoned => f.write_str("Failed to write due to being in an error state"),
            ReplayFileWriteError::TempSink { explanation } => f.write_str(explanation),
            ReplayFileWriteError::Other { explanation } => f.write_str(explanation)
        }
    }
}

/// A null sink
///
/// Useful if you do not want a temporary buffer, for example
#[derive(Copy, Clone, PartialEq, Debug)]
pub struct NullReplayFileSink;

impl ReplayFileSink for NullReplayFileSink {
    fn write_bytes(&mut self, _: &[u8]) -> Result<(), ReplayFileWriteError> {
        Ok(())
    }
    fn truncate(&mut self, _: u64) -> Result<(), ReplayFileWriteError> {
        Ok(())
    }
    fn write_packet_data(&mut self, instructions: &[PacketWriteCommand<'_>]) -> Result<usize, ReplayFileWriteError> {
        let mut len = 0usize;
        for i in instructions {
            len = len.saturating_add(i.bytes().len());
        }
        Ok(len)
    }
    fn overwrite_header(&mut self, _: &ReplayHeaderBytes) -> Result<(), ReplayFileWriteError> {
        Ok(())
    }
}

/// Object-safe wrapper for [`ReplayFileRecorder`]
///
/// See its documentation for what these functions do.
#[expect(missing_docs)]
pub trait ReplayFileRecorderFns: core::any::Any + 'static + Send {
    fn is_closed(&self) -> bool;
    fn close(&mut self) -> Result<(), ReplayFileWriteError>;
    fn next_frame(&mut self, timestamp_millis: TimestampMillis) -> Result<(), ReplayFileWriteError>;
    fn set_bookmark_table(&mut self, table: BookmarkTable) -> Result<(), ReplayFileWriteError>;
    fn insert_keyframe(&mut self, state: ByteVec, timestamp_millis: TimestampMillis) -> Result<(), ReplayFileWriteError>;
    fn insert_keyframe_full(&mut self, state: ByteVec, timestamp_millis: TimestampMillis) -> Result<(), ReplayFileWriteError>;
    fn set_input(&mut self, input_buffer: InputBuffer) -> Result<(), ReplayFileWriteError>;
    fn reset_console(&mut self) -> Result<(), ReplayFileWriteError>;
    fn write_memory(&mut self, address: UnsignedInteger, data: ByteVec) -> Result<(), ReplayFileWriteError>;
    fn set_speed(&mut self, speed: Speed) -> Result<(), ReplayFileWriteError>;
    fn load_save_state(&mut self, state: ByteVec) -> Result<(), ReplayFileWriteError>;
    fn serial_in(&mut self, data: ByteVec) -> Result<(), ReplayFileWriteError>;
    fn get_errors(&mut self) -> Vec<ReplayFileWriteError>;
    fn mark_start(&mut self, timer_offset: TimestampMillis) -> Result<(), ReplayFileWriteError>;
    fn mark_end(&mut self) -> Result<(), ReplayFileWriteError>;
    fn change_counter(&mut self, counter: String, delta: SignedInteger) -> Result<(), ReplayFileWriteError>;

    /// A state buffer the recorder has finished with, if any, for reuse by the next
    /// `insert_keyframe` (see `ReplayFileRecorder::take_recycled_state`).
    fn take_free_state_buffer(&mut self) -> Option<Vec<u8>> {
        None
    }
}

impl<Final: ReplayFileSink + 'static + Send, Temp: ReplayFileSink + 'static + Send> ReplayFileRecorderFns for ReplayFileRecorder<Final, Temp> {
    #[inline]
    fn is_closed(&self) -> bool {
        self.is_closed()
    }

    #[inline]
    fn take_free_state_buffer(&mut self) -> Option<Vec<u8>> {
        self.take_recycled_state()
    }

    #[inline]
    fn close(&mut self) -> Result<(), ReplayFileWriteError> {
        self.close().map_err(|e| e.2)?;
        Ok(())
    }

    #[inline]
    fn next_frame(&mut self, timestamp: TimestampMillis) -> Result<(), ReplayFileWriteError> {
        self.next_frame(timestamp)
    }

    #[inline]
    fn set_bookmark_table(&mut self, table: BookmarkTable) -> Result<(), ReplayFileWriteError> {
        self.set_bookmark_table(table)
    }

    #[inline]
    fn insert_keyframe(&mut self, state: ByteVec, timestamp: TimestampMillis) -> Result<(), ReplayFileWriteError> {
        self.insert_keyframe(state, timestamp)?;
        Ok(())
    }

    #[inline]
    fn insert_keyframe_full(&mut self, state: ByteVec, timestamp: TimestampMillis) -> Result<(), ReplayFileWriteError> {
        self.insert_keyframe_with(state, timestamp, KeyframeEncoding::Full)?;
        Ok(())
    }

    #[inline]
    fn set_input(&mut self, input_buffer: InputBuffer) -> Result<(), ReplayFileWriteError> {
        self.set_input(input_buffer)
    }

    #[inline]
    fn reset_console(&mut self) -> Result<(), ReplayFileWriteError> {
        self.reset_console()
    }

    #[inline]
    fn write_memory(&mut self, address: UnsignedInteger, data: ByteVec) -> Result<(), ReplayFileWriteError> {
        self.write_memory(address, data)
    }

    #[inline]
    fn set_speed(&mut self, speed: Speed) -> Result<(), ReplayFileWriteError> {
        self.set_speed(speed)
    }

    #[inline]
    fn load_save_state(&mut self, state: ByteVec) -> Result<(), ReplayFileWriteError> {
        self.load_save_state(state)
    }

    #[inline]
    fn serial_in(&mut self, data: ByteVec) -> Result<(), ReplayFileWriteError> {
        self.serial_in(data)
    }

    #[inline]
    fn get_errors(&mut self) -> Vec<ReplayFileWriteError> {
        // TODO
        Vec::new()
    }

    #[inline]
    fn mark_start(&mut self, timer_offset: TimestampMillis) -> Result<(), ReplayFileWriteError> {
        self.mark_start(timer_offset)
    }

    #[inline]
    fn mark_end(&mut self) -> Result<(), ReplayFileWriteError> {
        self.mark_end()
    }

    #[inline]
    fn change_counter(&mut self, counter: String, delta: SignedInteger) -> Result<(), ReplayFileWriteError> {
        self.change_counter(counter, delta)
    }
}

fn _ensure_replay_file_recorder_fns_is_dyn_compatible(_fns: &dyn ReplayFileRecorderFns) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use std::sync::{Arc, Mutex};

    fn settings(max_frames_per_blob: u64, minimum_uncompressed_bytes_per_blob: usize) -> ReplayFileRecorderSettings {
        ReplayFileRecorderSettings {
            minimum_uncompressed_bytes_per_blob,
            max_frames_per_blob,
            compression_level: DEFAULT_ZSTD_COMPRESSION_LEVEL_V4,
            mask_transient_buffers: true,
        }
    }

    /// Wraps a [`SharedSink`], injecting controllable failures for the B3 (per-sink failure
    /// handling) tests: the `n`-th call (1-based, counting `write_bytes`/`write_packet_data` calls
    /// together) fails exactly once with an injected error, and/or `flush` fails for as long as
    /// `set_fail_flush(true)` is in effect. `truncate`/`overwrite_header` always pass straight
    /// through to the wrapped sink.
    #[derive(Clone, Default, Debug)]
    struct FailingSink {
        inner: SharedSink,
        fail_write_at: Arc<Mutex<usize>>,
        write_calls: Arc<Mutex<usize>>,
        fail_flush: Arc<Mutex<bool>>,
    }

    impl FailingSink {
        fn new() -> Self {
            Self::default()
        }

        /// The `n`-th write call (1-based) fails; `0` (the default) never fails a write.
        fn fail_write_at(self, n: usize) -> Self {
            *self.fail_write_at.lock().unwrap() = n;
            self
        }

        fn set_fail_flush(&self, fail: bool) {
            *self.fail_flush.lock().unwrap() = fail;
        }

        fn snapshot(&self) -> Vec<u8> {
            self.inner.snapshot()
        }

        fn injected_error() -> ReplayFileWriteError {
            ReplayFileWriteError::Other { explanation: Cow::Borrowed("injected write failure") }
        }

        /// Bumps the call counter and reports whether THIS call is the one that should fail.
        fn bump_and_check(&self) -> bool {
            let mut calls = self.write_calls.lock().unwrap();
            *calls += 1;
            let at = *self.fail_write_at.lock().unwrap();
            at != 0 && *calls == at
        }
    }

    impl ReplayFileSink for FailingSink {
        fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), ReplayFileWriteError> {
            if self.bump_and_check() {
                return Err(Self::injected_error());
            }
            self.inner.write_bytes(bytes)
        }
        fn truncate(&mut self, size: u64) -> Result<(), ReplayFileWriteError> {
            self.inner.truncate(size)
        }
        fn overwrite_header(&mut self, header_data: &ReplayHeaderBytes) -> Result<(), ReplayFileWriteError> {
            self.inner.overwrite_header(header_data)
        }
        fn write_packet_data(&mut self, instructions: &[PacketWriteCommand<'_>]) -> Result<usize, ReplayFileWriteError> {
            if self.bump_and_check() {
                return Err(Self::injected_error());
            }
            self.inner.write_packet_data(instructions)
        }
        fn flush(&mut self) -> Result<(), ReplayFileWriteError> {
            if *self.fail_flush.lock().unwrap() {
                return Err(ReplayFileWriteError::Other { explanation: Cow::Borrowed("injected flush failure") });
            }
            Ok(())
        }
    }

    /// Every layout the recorder can produce must play back exactly (sequentially and with
    /// forward/backward/cross-blob seeks), from the temp file as well as the closed file.
    #[test]
    fn recorded_files_play_back_exactly() {
        let cases = [
            ("frame cap 45", settings(45, usize::MAX)),
            ("frame cap 50 (splits at the rewritten keyframe)", settings(50, usize::MAX)),
            ("byte cap 12 KiB", settings(0, 12 * 1024)),
            ("both caps", settings(60, 9 * 1024)),
            ("no cap (single blob / all top-level)", settings(0, usize::MAX)),
            ("one keyframe per blob", settings(1, usize::MAX)),
        ];

        for (name, settings) in cases {
            let (temp, closed) = record_script(settings);
            check_script_replay(&temp, &format!("{name}: temp layout"));
            check_script_replay(&closed, &format!("{name}: closed layout"));
        }
    }

    /// Link-cable traffic (`SerialIn`) is an ordinary per-frame packet: written just before the
    /// frame's `NextFrame`, it comes back in that position from every layout (top level, inside a
    /// blob, after a seek), an empty payload writes nothing, the file is format v7, and resuming
    /// from such a file re-feeds the packets into the new file.
    #[test]
    fn serial_in_packets_round_trip_and_survive_a_resume() {
        use crate::replay_file::playback::ReplayFilePlayer;
        use crate::replay_file::record::build_resumed_recorder;
        use crate::replay_file::REPLAY_VERSION;

        let traffic = |frame: u64| -> Vec<u8> {
            // Something that changes per frame; frame 5 sends nothing at all.
            if frame == 5 { Vec::new() } else { (0..(frame as u8 % 7 + 1)).map(|i| i.wrapping_mul(31).wrapping_add(frame as u8)).collect() }
        };

        for (name, layout) in [("one blob per 3 frames", settings(3, 1)), ("top level", settings(0, usize::MAX))] {
            let mut recorder = ReplayFileRecorder::new_with_metadata(
                make_metadata(), ByteVec::new(), layout, 0u64.into(), ib(&[0]), Speed::default(), bv(&state_for(0)), Vec::new(), Vec::new()
            ).unwrap();
            let mut running = 0u64;
            for frame in 1..=12u64 {
                if frame % 4 == 0 {
                    recorder.set_input(ib(&[frame as u8])).unwrap();
                }
                recorder.serial_in(bv(&traffic(frame))).unwrap();
                running += 16;
                recorder.next_frame(running.into()).unwrap();
                if frame % 6 == 0 {
                    recorder.insert_keyframe(bv(&state_for(frame)), running.into()).unwrap();
                }
            }
            let (closed, _) = recorder.close().map_err(|e| e.2).unwrap();
            assert_eq!(u32::from_le_bytes(closed[4..8].try_into().unwrap()), REPLAY_VERSION, "{name}");
            assert_eq!(REPLAY_VERSION, 7);

            let mut player = ReplayFilePlayer::new(&closed, false).unwrap();
            player.go_to_keyframe(0).unwrap();
            let mut frame = 0u64;
            let mut serial_seen = 0;
            let mut pending_serial: Option<Vec<u8>> = None;
            while let Some(packet) = player.next_packet().unwrap() {
                match packet {
                    Packet::SerialIn { data } => {
                        assert!(pending_serial.is_none(), "{name}: two SerialIn packets in frame {}", frame + 1);
                        pending_serial = Some(data.to_vec());
                    }
                    Packet::NextFrame { .. } => {
                        frame += 1;
                        let expected = traffic(frame);
                        if expected.is_empty() {
                            assert!(pending_serial.is_none(), "{name}: frame {frame} should carry no SerialIn");
                        }
                        else {
                            assert_eq!(pending_serial.take(), Some(expected), "{name}: frame {frame}");
                            serial_seen += 1;
                        }
                    }
                    _ => {}
                }
            }
            assert_eq!(frame, 12, "{name}");
            assert_eq!(serial_seen, 11, "{name}");

            // After a seek to the mid-file keyframe the packets of frame 7 (the first one after it)
            // come back with their SerialIn before their NextFrame.
            player.go_to_keyframe(6).unwrap();
            assert!(matches!(player.next_packet().unwrap(), Some(Packet::Keyframe { .. })), "{name}");
            let mut got = Vec::new();
            loop {
                match player.next_packet().unwrap() {
                    Some(Packet::NextFrame { .. }) => break,
                    Some(Packet::SerialIn { data }) => got.push(data.to_vec()),
                    Some(_) => {}
                    None => panic!("{name}: ended before frame 7"),
                }
            }
            assert_eq!(got, alloc::vec![traffic(7)], "{name}: frame 7 after a seek");

            // Resume from frame 9: the frames re-fed after the boundary keyframe keep their traffic.
            let mut source = ReplayFilePlayer::new(&closed, false).unwrap();
            source.set_keyframe_states_wanted(true);
            let (resumed, info) = build_resumed_recorder(&mut source, Some(9), settings(0, usize::MAX), ResumeCropPolicy::PreserveStartDropEnd, None, Vec::new(), Vec::new()).unwrap();
            assert_eq!(info.elapsed_frames, 9, "{name}");
            let mut resumed = resumed;
            resumed.serial_in(bv(&[0xEE])).unwrap();
            resumed.next_frame((running + 16).into()).unwrap();
            let (resumed_bytes, _) = resumed.close().map_err(|e| e.2).unwrap();
            let mut player = ReplayFilePlayer::new(&resumed_bytes, false).unwrap();
            player.go_to_keyframe(0).unwrap();
            let mut per_frame: Vec<Option<Vec<u8>>> = Vec::new();
            let mut pending = None;
            while let Some(packet) = player.next_packet().unwrap() {
                match packet {
                    Packet::SerialIn { data } => pending = Some(data.to_vec()),
                    Packet::NextFrame { .. } => per_frame.push(pending.take()),
                    _ => {}
                }
            }
            assert_eq!(per_frame.len(), 10, "{name}: 9 resumed frames plus one new");
            // Frames 7..=9 were re-fed after the boundary keyframe at frame 6; the earlier ones sit
            // in blobs copied verbatim (or were re-fed too), either way with their traffic intact.
            for frame in 1..=9u64 {
                let expected = traffic(frame);
                assert_eq!(per_frame[frame as usize - 1].clone().unwrap_or_default(), expected, "{name}: resumed frame {frame}");
            }
            assert_eq!(per_frame[9], Some(alloc::vec![0xEE]), "{name}: the new frame");
        }
    }

    /// The buffer a keyframe state arrives in is handed back once the recorder has diffed the
    /// next keyframe against it, so the producer can fill it again instead of allocating.
    #[test]
    fn displaced_keyframe_buffers_are_recycled() {
        let mut recorder = ReplayFileRecorder::new_with_metadata(
            make_metadata(),
            ByteVec::new(),
            settings(0, usize::MAX),
            0u64.into(),
            ib(&[0]),
            Speed::default(),
            bv(&state_for(0)),
            Vec::new(),
            Vec::new(),
        )
        .unwrap();

        // The initial state is the first diff base; nothing has been displaced yet.
        assert!(recorder.take_recycled_state().is_none());

        let first: Vec<u8> = state_for(1);
        let first_ptr = first.as_ptr();
        recorder.next_frame(16u64.into()).unwrap();
        recorder.insert_keyframe(ByteVec::Heap(first), 16u64.into()).unwrap();
        // The initial state was an inline/heap ByteVec made by the test helper; whatever came
        // back, `first` itself is now the diff base and must not be offered yet.
        let _ = recorder.take_recycled_state();

        recorder.next_frame(32u64.into()).unwrap();
        recorder.insert_keyframe(ByteVec::Heap(state_for(2)), 32u64.into()).unwrap();
        let recycled = recorder.take_recycled_state().expect("the displaced buffer is offered back");
        assert_eq!(recycled.as_ptr(), first_ptr, "the very allocation that was displaced comes back");
        assert!(recorder.take_recycled_state().is_none(), "offered once only");

        recorder.close().unwrap();
    }

    #[test]
    fn keyframes_are_region_deltas_except_restarts_and_fallbacks() {
        let (temp, closed) = record_script(settings(45, usize::MAX));

        // 200 frames / 45 per blob: restarts at 0, 45, 90, 135, 180.
        let stats = file_stats(&closed);
        assert_eq!(stats.blobs, 5);
        assert_eq!(stats.top_level_packets, 5);
        assert_eq!(stats.keyframes.len(), keyframe_frames().len());
        assert_eq!(stats.count(StoredKeyframeKind::V3Delta), 0);

        for frame in [0, 45, 90, 135, 180] {
            assert_eq!(stats.kind_at(frame), StoredKeyframeKind::Full, "restart at {frame}");
        }
        // Fallbacks: the rewritten state (and the keyframe after it, which shares nothing with it
        // either), and the length changes at both ends of LONGER_RANGE.
        assert_eq!(stats.kind_at(REWRITTEN_FRAME), StoredKeyframeKind::Full);
        assert_eq!(stats.kind_at(REWRITTEN_FRAME + KEYFRAME_INTERVAL), StoredKeyframeKind::Full);
        assert_eq!(stats.kind_at(LONGER_RANGE.start), StoredKeyframeKind::Full);
        assert_eq!(stats.kind_at(LONGER_RANGE.end), StoredKeyframeKind::Full);
        // ...and the chain continues with deltas right after each fallback.
        for frame in [REWRITTEN_FRAME + 2 * KEYFRAME_INTERVAL, LONGER_RANGE.start + 5, LONGER_RANGE.end + 5, 5, 50, 195, 200] {
            assert_eq!(stats.kind_at(frame), StoredKeyframeKind::RegionDelta, "delta at {frame}");
        }
        assert_eq!(stats.count(StoredKeyframeKind::Full), 5 + 4);

        // The temp layout holds the same keyframes, with the last chain uncompressed at top level.
        let temp_stats = file_stats(&temp);
        assert_eq!(temp_stats.blobs, 4);
        assert_eq!(temp_stats.keyframes, stats.keyframes.iter().map(|&(f, k, _)| (f, k, f < 180)).collect::<Vec<_>>());
    }

    #[test]
    fn byte_cap_counts_actual_buffered_bytes() {
        // Each keyframe delta is a few hundred bytes; with 4 KiB states the v3 estimator would have
        // rolled an 8 KiB cap over every second keyframe (41 keyframes -> ~20 blobs). Only full
        // keyframes (restarts and the 4 fallbacks) weigh anything now.
        let (_, closed) = record_script(settings(0, 8 * 1024));
        let stats = file_stats(&closed);
        assert!(stats.blobs >= 2 && stats.blobs <= 8, "blobs = {}", stats.blobs);
        assert!(stats.count(StoredKeyframeKind::RegionDelta) > 30);
    }

    #[test]
    fn v4_output_is_smaller_than_the_v3_fixture() {
        // Same script, same number of blobs (4: restarts at 0, 55, 110, 165) as the v3 fixture.
        let (_, closed) = record_script(settings(55, usize::MAX));
        assert_eq!(file_stats(&closed).blobs, 4);
        assert!(closed.len() < V3_SMALL_CLOSED.len(), "{} >= {}", closed.len(), V3_SMALL_CLOSED.len());
        let version = crate::replay_file::ReplayHeaderRaw::from_bytes(closed[..2048].try_into().unwrap()).replay_version;
        assert_eq!(version, crate::replay_file::REPLAY_VERSION);
    }

    /// With masks on, a GBA chain must reconstruct every keyframe exactly outside the m4a PCM
    /// buffer and with the chain restart's bytes inside it; with masks off, exactly everywhere.
    #[test]
    fn transient_buffer_masks_are_applied_per_chain() {
        use crate::keyframe_masks::transient_ranges;
        use crate::replay_file::playback::ReplayFilePlayer;
        use crate::replay_file::ReplayConsoleType;

        const KEYFRAMES: u64 = 9;
        const CHAIN: u64 = 4; // restarts at frames 0, 4, 8

        // Synthetic mGBA states with an m4a SoundInfo at 0x03006380; every keyframe rewrites the
        // whole PCM buffer and a few other bytes.
        let base = pseudo_random_bytes(0x6BA, 0x61000 + 1024);
        let sound_info = 0x19000 + 0x6380;
        let state_at = |frame: u64| -> Vec<u8> {
            let mut s = base.clone();
            s[0..4].copy_from_slice(&0x0100_000Au32.to_le_bytes());
            s[0x19000 + 0x7FF0..0x19000 + 0x7FF4].copy_from_slice(&0x0300_6380u32.to_le_bytes());
            s[sound_info..sound_info + 4].copy_from_slice(&0x6873_6D53u32.to_le_bytes());
            let pcm = pseudo_random_bytes(1000 + frame, 0xC60);
            s[sound_info + 0x350..sound_info + 0x350 + 0xC60].copy_from_slice(&pcm);
            s[100 + frame as usize * 8] = frame as u8;
            s[0x40000 + frame as usize] = !(frame as u8);
            s
        };
        let metadata = ReplayFileMetadata { console_type: ReplayConsoleType::GameBoyAdvance, ..make_metadata() };
        let range = transient_ranges(ReplayConsoleType::GameBoyAdvance, &state_at(0));
        assert_eq!(range.len(), 1);
        let range = range[0].clone();

        for masks in [true, false] {
            let chain_settings = ReplayFileRecorderSettings { mask_transient_buffers: masks, ..settings(CHAIN, usize::MAX) };
            let mut recorder = ReplayFileRecorder::new_with_metadata(
                metadata.clone(), ByteVec::new(), chain_settings, 0u64.into(), ib(&[0]), Speed::default(), bv(&state_at(0)), Vec::<u8>::new(), Vec::<u8>::new()
            ).unwrap();
            for frame in 1..=KEYFRAMES {
                recorder.next_frame((frame * 16).into()).unwrap();
                recorder.insert_keyframe(bv(&state_at(frame)), (frame * 16).into()).unwrap();
            }
            let (bytes, _) = recorder.close().unwrap();

            let mut player = ReplayFilePlayer::new(&bytes, false).unwrap();
            for frame in (0..=KEYFRAMES).rev() {
                player.go_to_keyframe(frame).unwrap();
                let Some(Packet::Keyframe { state, .. }) = player.next_packet().unwrap() else { panic!("no keyframe at {frame}") };
                let recorded = state_at(frame);
                let restart = state_at(frame / CHAIN * CHAIN);

                assert_eq!(&state[..range.start], &recorded[..range.start], "masks={masks} frame {frame}: before the range");
                assert_eq!(&state[range.end..], &recorded[range.end..], "masks={masks} frame {frame}: after the range");
                let expected_inside = if masks { &restart[range.clone()] } else { &recorded[range.clone()] };
                assert_eq!(&state[range.clone()], expected_inside, "masks={masks} frame {frame}: inside the range");
            }

            // Masking removes the PCM buffer from every delta, so the file is much smaller.
            if masks {
                let unmasked = {
                    let unmasked_settings = ReplayFileRecorderSettings { mask_transient_buffers: false, ..settings(CHAIN, usize::MAX) };
                    let mut r = ReplayFileRecorder::new_with_metadata(
                        metadata.clone(), ByteVec::new(), unmasked_settings, 0u64.into(), ib(&[0]), Speed::default(), bv(&state_at(0)), Vec::<u8>::new(), Vec::<u8>::new()
                    ).unwrap();
                    for frame in 1..=KEYFRAMES {
                        r.next_frame((frame * 16).into()).unwrap();
                        r.insert_keyframe(bv(&state_at(frame)), (frame * 16).into()).unwrap();
                    }
                    r.close().unwrap().0.len()
                };
                assert!(bytes.len() < unmasked - 5 * 0xC60, "masked {} vs unmasked {unmasked}", bytes.len());
            }
        }
    }

    /// A recording that is never closed must recover the bookmark table as of the moment it
    /// stopped from its temp file, and as of its last completed blob from its final file.
    #[test]
    fn bookmark_tables_are_recovered_at_every_point() {
        use crate::replay_file::playback::{BookmarkTableSource, ReplayFilePlayer};

        for (name, settings) in [("frame cap 45", settings(45, usize::MAX)), ("one keyframe per blob", settings(1, usize::MAX)), ("no cap", settings(0, usize::MAX))] {
            let temp = SharedSink::default();
            let final_sink = SharedSink::default();
            let mut recorder = ReplayFileRecorder::new_with_metadata(
                make_metadata(), ByteVec::new(), settings, 0u64.into(), ib(&[0]), Speed::default(), bv(&state_for(0)), final_sink.clone(), temp.clone()
            ).unwrap();

            run_script_observed(&mut recorder, &mut |frame| {
                let expected = script_table_through(frame);
                let player = ReplayFilePlayer::new(temp.snapshot(), false).unwrap();
                assert_eq!(*player.bookmark_table(), expected, "{name}: temp file after frame {frame}");
                let source = if expected.is_empty() && frame < 8 { BookmarkTableSource::None } else { BookmarkTableSource::StreamSnapshot };
                assert_eq!(player.bookmark_table_source(), source, "{name}: temp file after frame {frame}");

                let final_bytes = final_sink.snapshot();
                if let Ok(player) = ReplayFilePlayer::new(&final_bytes, false) {
                    let last_blob_end = player.all_uncompressed_packets().iter().rev().find_map(|p| match p {
                        Packet::CompressedBlob { elapsed_frames_end, .. } => Some(*elapsed_frames_end),
                        _ => None
                    }).unwrap();
                    assert_eq!(*player.bookmark_table(), script_table_through(last_blob_end), "{name}: final file after frame {frame}");
                }
            });

            recorder.close().unwrap();
        }
    }

    #[test]
    fn closed_files_end_with_the_bookmark_section() {
        use crate::replay_file::bookmark_section::{decode_bookmark_section, BookmarkSectionError};
        use crate::replay_file::playback::{BookmarkTableSource, ReplayFilePlayer};

        let (_, closed) = record_script(settings(45, usize::MAX));
        let header = ReplayHeaderRaw::from_bytes(closed[..2048].try_into().unwrap());
        let stream_end = header.packet_stream_end().expect("a closed file records its stream end") as usize;
        let stats = file_stats(&closed);
        assert_eq!(stats.trailing_bytes, closed.len() - stream_end);
        assert_eq!(decode_bookmark_section(&closed[stream_end..]), Ok(script_table_through(TOTAL_FRAMES)));

        // Losing the section (a crash while closing) falls back to the stream.
        let player = ReplayFilePlayer::new(&closed[..stream_end], false).unwrap();
        assert_eq!(player.bookmark_table_source(), BookmarkTableSource::StreamSnapshot);
        assert_eq!(player.bookmark_section_error(), Some(&BookmarkSectionError::Truncated));
        assert_eq!(*player.bookmark_table(), script_table_through(TOTAL_FRAMES));
        check_script_replay(&closed[..stream_end], "section cut off");

        let mut damaged = closed.clone();
        let last = damaged.len() - 1;
        damaged[last] ^= 0x40;
        let player = ReplayFilePlayer::new(&damaged, false).unwrap();
        assert_eq!(player.bookmark_table_source(), BookmarkTableSource::StreamSnapshot);
        assert_eq!(player.bookmark_section_error(), Some(&BookmarkSectionError::HashMismatch));
        assert_eq!(*player.bookmark_table(), script_table_through(TOTAL_FRAMES));

        // A file cut inside its packets is refused, or read as far as possible when allowed.
        let cut = &closed[..stream_end - 100];
        assert!(ReplayFilePlayer::new(cut, false).is_err());
        let player = ReplayFilePlayer::new(cut, true).unwrap();
        assert!(player.stream_truncated());
        assert_eq!(player.bookmark_section_error(), None, "a truncated stream has no section to check");
    }

    #[test]
    fn recordings_without_bookmarks_write_no_snapshots() {
        use crate::replay_file::playback::{BookmarkTableSource, ReplayFilePlayer};

        let mut recorder = ReplayFileRecorder::new_with_metadata(
            make_metadata(), ByteVec::new(), settings(5, usize::MAX), 0u64.into(), ib(&[0]), Speed::default(), bv(&state_for(0)), Vec::<u8>::new(), Vec::<u8>::new()
        ).unwrap();
        for frame in 1..=20u64 {
            recorder.set_bookmark_table(BookmarkTable::new()).unwrap();
            recorder.next_frame((frame * 16).into()).unwrap();
            if frame % 5 == 0 {
                recorder.insert_keyframe(bv(&state_for(frame)), (frame * 16).into()).unwrap();
            }
        }
        let (closed, _) = recorder.close().unwrap();
        assert_eq!(file_stats(&closed).bookmark_snapshots, 0);

        let player = ReplayFilePlayer::new(&closed, false).unwrap();
        assert_eq!(player.bookmark_table_source(), BookmarkTableSource::Section);
        assert!(player.bookmark_table().is_empty());
    }

    /// A keyframe bookmark forces a full keyframe, sometimes on a frame that already has a
    /// scheduled one; a seek to that frame must land on the full keyframe and apply no deltas, in
    /// both the temp (top-level) and closed (blob) layouts.
    #[test]
    fn forced_full_keyframes_are_seeked_to_without_folds() {
        use crate::replay_file::playback::ReplayFilePlayer;

        let alternate = pseudo_random_bytes(77, STATE_LEN);
        let temp = SharedSink::default();
        let mut recorder = ReplayFileRecorder::new_with_metadata(
            make_metadata(), ByteVec::new(), settings(0, usize::MAX), 0u64.into(), ib(&[0]), Speed::default(), bv(&state_for(0)), Vec::<u8>::new(), temp.clone()
        ).unwrap();
        for frame in 1..=30u64 {
            recorder.next_frame((frame * 16).into()).unwrap();
            if frame % 5 == 0 {
                recorder.insert_keyframe(bv(&state_for(frame)), (frame * 16).into()).unwrap();
            }
            if frame == 12 {
                recorder.insert_keyframe_with(bv(&state_for(frame)), (frame * 16).into(), KeyframeEncoding::Full).unwrap();
            }
            if frame == 15 {
                // Same frame as the scheduled keyframe; a different state tells the two apart.
                recorder.insert_keyframe_with(bv(&alternate), (frame * 16).into(), KeyframeEncoding::Full).unwrap();
            }
        }
        let temp_bytes = temp.snapshot();
        let (closed, _) = recorder.close().unwrap();

        let stats = file_stats(&closed);
        assert_eq!(stats.keyframes.iter().filter(|k| k.0 == 15).map(|k| k.1).collect::<Vec<_>>(), [StoredKeyframeKind::RegionDelta, StoredKeyframeKind::Full]);
        assert_eq!(stats.kind_at(12), StoredKeyframeKind::Full);
        // The alternate state shares nothing with the next scheduled state (so 20 falls back to full);
        // the chain then continues with deltas.
        assert_eq!(stats.kind_at(20), StoredKeyframeKind::Full);
        assert_eq!(stats.kind_at(25), StoredKeyframeKind::RegionDelta);

        for (layout, bytes) in [("temp", &temp_bytes), ("closed", &closed)] {
            let mut player = ReplayFilePlayer::new(bytes, false).unwrap();
            assert!(player.keyframe_is_stored_full(12).unwrap(), "{layout}");
            assert!(player.keyframe_is_stored_full(15).unwrap(), "{layout}");
            assert!(!player.keyframe_is_stored_full(10).unwrap(), "{layout}");

            for (frame, expected) in [(15u64, alternate.clone()), (12, state_for(12)), (25, state_for(25)), (15, alternate.clone())] {
                // Park the chain somewhere unrelated first, so the seek cannot fold incrementally.
                player.go_to_keyframe(0).unwrap();
                player.next_packet().unwrap();

                let folds = player.chain_folds();
                player.go_to_keyframe(frame).unwrap();
                match player.next_packet().unwrap() {
                    Some(Packet::Keyframe { state, metadata }) => {
                        assert_eq!(metadata.elapsed_frames, frame);
                        assert_eq!(state.as_slice(), expected.as_slice(), "{layout}: state at {frame}");
                    }
                    other => panic!("{layout}: expected keyframe {frame}, got {other:?}")
                }
                let applied = player.chain_folds() - folds;
                if frame == 25 {
                    assert!(applied > 0, "{layout}: a delta keyframe is folded");
                } else {
                    assert_eq!(applied, 0, "{layout}: a seek to the full keyframe at {frame} folds nothing");
                }
            }

            // A sequential walk still passes both keyframes of frame 15.
            player.go_to_keyframe(0).unwrap();
            let mut at_15 = 0;
            while let Some(packet) = player.next_packet().unwrap() {
                if matches!(packet, Packet::Keyframe { metadata, .. } if metadata.elapsed_frames == 15) {
                    at_15 += 1;
                }
            }
            assert_eq!(at_15, 2, "{layout}");
        }
    }

    #[test]
    fn identical_consecutive_states_produce_empty_deltas() {
        let state = bv(&pseudo_random_bytes(4, 1000));
        let mut recorder = ReplayFileRecorder::new_with_metadata(
            make_metadata(), ByteVec::new(), settings(0, usize::MAX), 0u64.into(), ib(&[0]), Speed::default(), state.clone(), Vec::<u8>::new(), Vec::<u8>::new()
        ).unwrap();
        recorder.next_frame(16.into()).unwrap();
        recorder.insert_keyframe(state.clone(), 16.into()).unwrap();
        recorder.next_frame(32.into()).unwrap();
        recorder.insert_keyframe(state.clone(), 32.into()).unwrap();
        let (closed, _) = recorder.close().unwrap();

        let stats = file_stats(&closed);
        assert_eq!(stats.keyframes.iter().map(|k| k.1).collect::<Vec<_>>(), [StoredKeyframeKind::Full, StoredKeyframeKind::RegionDelta, StoredKeyframeKind::RegionDelta]);

        let mut player = crate::replay_file::playback::ReplayFilePlayer::new(&closed, false).unwrap();
        for frame in [2u64, 0, 1] {
            player.go_to_keyframe(frame).unwrap();
            match player.next_packet().unwrap() {
                Some(Packet::Keyframe { state: s, .. }) => assert_eq!(s, &state),
                other => panic!("{other:?}"),
            }
        }
    }

    /// A keyframe timestamp going backwards is bad input, not a crash: `insert_keyframe` must
    /// return `Err(BadInput)` and leave the recorder fully usable afterwards.
    #[test]
    fn keyframe_timestamp_backwards_is_bad_input_not_a_panic() {
        use crate::replay_file::playback::ReplayFilePlayer;

        let mut recorder = ReplayFileRecorder::new_with_metadata(
            make_metadata(), ByteVec::new(), settings(0, usize::MAX), 0u64.into(), ib(&[0]), Speed::default(), bv(&state_for(0)), Vec::<u8>::new(), Vec::<u8>::new()
        ).unwrap();

        recorder.next_frame(16u64.into()).unwrap();
        let err = recorder.insert_keyframe(bv(&state_for(1)), 8u64.into()).unwrap_err();
        assert!(matches!(err, ReplayFileWriteError::BadInput { .. }), "{err:?}");
        assert!(!recorder.is_closed(), "a bad-input error must not close or poison the recorder");

        // The recorder keeps working: a later (non-backwards) keyframe and close() still succeed.
        recorder.next_frame(32u64.into()).unwrap();
        recorder.insert_keyframe(bv(&state_for(2)), 32u64.into()).unwrap();
        let (closed, _) = recorder.close().unwrap();

        let player = ReplayFilePlayer::new(&closed, false).unwrap();
        assert_eq!(player.get_total_frames(), 2);
    }

    /// `apply_masks` overwrites the transient (masked) ranges of a state in place; when
    /// `insert_keyframe_with` falls back to storing a full (undiffed) keyframe after having
    /// masked, it must restore exactly what was there before -- full keyframes are documented to
    /// reconstruct bit-exactly, so masking must never leak into one. This verifies that guarantee
    /// directly: restoring what `apply_masks` returns must exactly undo its effect, for a
    /// wholesale-rewritten frame (so masking has a real, non-trivial effect: the two states'
    /// masked ranges genuinely differ).
    ///
    /// This does not go through `ReplayFileRecorder`/`insert_keyframe`, because doing so cannot
    /// actually reach the full-keyframe-fallback branch while masking has a real effect: masking
    /// only ever removes bytes from the region-diff's consideration, so it can only ever shrink
    /// the encoded delta, never make it larger than the full state -- and the masked range is a
    /// small, fixed-size fraction of a real GBA or NDS state either way, so no amount of
    /// "everything else differs" content can make up for what masking saves. (Confirmed
    /// empirically too: driving this exact before/after pair through the recorder stores it as a
    /// `RegionDeltaKeyframe`, not a full one.) The normal, always-happens-in-practice case --
    /// masking applied, a delta chosen -- is covered by `transient_buffer_masks_are_applied_per_chain`.
    #[test]
    fn fallback_full_keyframes_are_exact_when_masked() {
        use crate::replay_file::ReplayConsoleType;

        let base = pseudo_random_bytes(0x6BA, 0x61000 + 1024);
        let sound_info = 0x19000 + 0x6380;

        let mut prev = base.clone();
        prev[0..4].copy_from_slice(&0x0100_000Au32.to_le_bytes());
        prev[0x19000 + 0x7FF0..0x19000 + 0x7FF4].copy_from_slice(&0x0300_6380u32.to_le_bytes());
        prev[sound_info..sound_info + 4].copy_from_slice(&0x6873_6D53u32.to_le_bytes());
        prev[sound_info + 0x350..sound_info + 0x350 + 0xC60].copy_from_slice(&pseudo_random_bytes(1, 0xC60));

        // A wholesale rewrite, except for the bytes that make it recognisable as the same layout
        // (magic, the m4a SoundInfo pointer, its ident) -- so the masked (PCM buffer) range is the
        // same range as `prev`'s, and its content genuinely differs (so masking has a real effect).
        let mut cur = prev.clone();
        for b in cur.iter_mut() {
            *b = b.wrapping_add(1);
        }
        cur[0..4].copy_from_slice(&prev[0..4]);
        cur[0x19000 + 0x7FF0..0x19000 + 0x7FF4].copy_from_slice(&prev[0x19000 + 0x7FF0..0x19000 + 0x7FF4]);
        cur[sound_info..sound_info + 4].copy_from_slice(&prev[sound_info..sound_info + 4]);

        let original = cur.clone();
        let masked = crate::keyframe_masks::apply_masks(ReplayConsoleType::GameBoyAdvance, &prev, &mut cur);
        assert!(!masked.is_empty(), "the PCM buffer must have actually been recognised and masked");
        assert_ne!(cur, original, "masking must have had a real effect (the PCM buffers genuinely differ)");

        // Exactly what insert_keyframe_with's full-keyframe fallback does with `masked`.
        for (range, original_bytes) in &masked {
            cur[range.clone()].copy_from_slice(original_bytes);
        }
        assert_eq!(cur, original, "restoring what apply_masks overwrote must exactly undo it");
    }

    /// If the final sink's blob write fails, the blob (and its keyframes) is retained rather than
    /// lost: the failing call returns `Err`, but a later call that reaches `next_blob` again (here,
    /// simply retrying the same keyframe) succeeds once the sink stops failing, and `close()`
    /// produces a file that plays back exactly.
    #[test]
    fn final_sink_failure_keeps_the_blob_and_retries() {
        use crate::replay_file::playback::ReplayFilePlayer;

        // max_frames_per_blob = 1: inserting keyframe #2 (frame 1) forces blob #1 (holding just
        // keyframe #1, frame 0) to flush. That flush is the final sink's 3rd write (after the
        // header and the empty patch), which is where the one injected failure lands.
        let final_sink = FailingSink::new().fail_write_at(3);
        let temp = SharedSink::default();
        let mut recorder = ReplayFileRecorder::new_with_metadata(
            make_metadata(), ByteVec::new(), settings(1, usize::MAX), 0u64.into(), ib(&[0]), Speed::default(), bv(&state_for(0)), final_sink.clone(), temp.clone()
        ).unwrap();

        recorder.next_frame(16u64.into()).unwrap();
        let err = recorder.insert_keyframe(bv(&state_for(1)), 16u64.into()).unwrap_err();
        assert!(matches!(err, ReplayFileWriteError::Other { .. }), "{err:?}");
        assert!(!recorder.is_closed());
        assert!(recorder.is_poisoned(), "the final sink failed and has not yet retried successfully");

        // Retry: current_blob_keyframes still holds keyframe #1 untouched (it was never cleared,
        // since the failed next_blob() returned before insert_keyframe_with got that far), so
        // re-inserting the exact same keyframe flushes the SAME blob -- and this time the sink's
        // one injected failure has already been consumed, so it succeeds.
        recorder.insert_keyframe(bv(&state_for(1)), 16u64.into()).unwrap();
        assert!(!recorder.is_poisoned());

        recorder.next_frame(32u64.into()).unwrap();
        recorder.insert_keyframe(bv(&state_for(2)), 32u64.into()).unwrap();

        let (final_sink, _temp) = recorder.close().unwrap();
        let closed = final_sink.snapshot();

        let mut player = ReplayFilePlayer::new(&closed, false).unwrap();
        assert_eq!(player.get_total_frames(), 2);
        for frame in [0u64, 1, 2] {
            player.go_to_keyframe(frame).unwrap();
            match player.next_packet().unwrap() {
                Some(Packet::Keyframe { state, metadata }) => {
                    assert_eq!(metadata.elapsed_frames, frame);
                    assert_eq!(state.as_slice(), state_for(frame).as_slice(), "frame {frame}");
                }
                other => panic!("expected keyframe {frame}, got {other:?}"),
            }
        }
    }

    /// A temp-sink failure is surfaced exactly once (as `TempSink`), recorded on the recorder, and
    /// otherwise has no effect: the recording continues normally and the final file is unaffected.
    #[test]
    fn temp_sink_failure_does_not_stop_the_recording() {
        use crate::replay_file::playback::ReplayFilePlayer;

        // Temp write calls so far once construction finishes: #1 header, #2 (empty) patch, #3 the
        // frame-0 keyframe. #4 is the first `next_frame`, which is where the one injected failure
        // lands.
        let final_sink = SharedSink::default();
        let temp = FailingSink::new().fail_write_at(4);
        let mut recorder = ReplayFileRecorder::new_with_metadata(
            make_metadata(), ByteVec::new(), settings(0, usize::MAX), 0u64.into(), ib(&[0]), Speed::default(), bv(&state_for(0)), final_sink.clone(), temp.clone()
        ).unwrap();
        assert!(recorder.temp_sink_failed().is_none());

        let err = recorder.next_frame(16u64.into()).unwrap_err();
        assert!(matches!(err, ReplayFileWriteError::TempSink { .. }), "{err:?}");
        assert!(recorder.temp_sink_failed().is_some());
        assert!(!recorder.is_poisoned(), "a temp failure must not poison the final sink");

        // Later operations succeed normally: the temp mirror is silently abandoned, but the
        // recording (backed by the final sink) is unaffected.
        recorder.insert_keyframe(bv(&state_for(1)), 16u64.into()).unwrap();
        recorder.next_frame(32u64.into()).unwrap();
        recorder.insert_keyframe(bv(&state_for(2)), 32u64.into()).unwrap();
        assert!(recorder.temp_sink_failed().is_some());

        let (closed, _temp) = recorder.close().unwrap();
        let closed = closed.snapshot();

        let mut player = ReplayFilePlayer::new(&closed, false).unwrap();
        assert_eq!(player.get_total_frames(), 2);
        for frame in [0u64, 1, 2] {
            player.go_to_keyframe(frame).unwrap();
            match player.next_packet().unwrap() {
                Some(Packet::Keyframe { state, metadata }) => {
                    assert_eq!(metadata.elapsed_frames, frame);
                    assert_eq!(state.as_slice(), state_for(frame).as_slice(), "frame {frame}");
                }
                other => panic!("expected keyframe {frame}, got {other:?}"),
            }
        }
    }

    /// A flush error (previously silently swallowed for a buffered file sink) must reach the
    /// caller instead of being treated as success.
    #[test]
    fn flush_errors_reach_close() {
        let final_sink = FailingSink::new();
        final_sink.set_fail_flush(true);
        let temp = SharedSink::default();
        let mut recorder = ReplayFileRecorder::new_with_metadata(
            make_metadata(), ByteVec::new(), settings(0, usize::MAX), 0u64.into(), ib(&[0]), Speed::default(), bv(&state_for(0)), final_sink.clone(), temp.clone()
        ).unwrap();

        recorder.next_frame(16u64.into()).unwrap();
        recorder.insert_keyframe(bv(&state_for(1)), 16u64.into()).unwrap();

        // No blob-size/frame cap is reached yet, so close() is what first flushes the in-progress
        // blob: the write itself succeeds, but the sink's flush() fails, and that must abort the
        // close (not be swallowed as `let _ = self.flush()` used to).
        let (_final_sink, _temp, error) = recorder.close().unwrap_err();
        assert!(matches!(error, ReplayFileWriteError::Other { .. }), "{error:?}");
    }
}
