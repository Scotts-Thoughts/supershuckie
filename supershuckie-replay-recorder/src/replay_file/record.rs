//! Replay file recording functionality.
//!
//! See [`ReplayFileRecorder`] and [`NonBlockingReplayFileRecorder`].

use crate::replay_file::{ReplayFileMetadata, ReplayHeaderBytes, ReplayHeaderRaw};
use crate::{BookmarkMetadata, ByteVec, Counter, InputBuffer, KeyframeMetadata, Packet, PacketIO, PacketWriteCommand, SignedInteger, Speed, TimestampMillis, UnsignedInteger};
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
use std::collections::BTreeMap;
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
    current_blob_bookmarks: Vec<BookmarkMetadata>,
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

    poisoned: bool
}

struct SinkTuple<Final: ReplayFileSink, Temp: ReplayFileSink> {
    final_sink: Final,
    temp_sink: Temp
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
    /// This bounds the length of a delta chain, i.e. how many deltas a cold seek may have to apply.
    ///
    /// Default is [`DEFAULT_MAX_FRAMES_PER_BLOB`] (15 minutes at 60 fps).
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

/// Default maximum frames per blob (15 minutes at 60 fps).
pub const DEFAULT_MAX_FRAMES_PER_BLOB: u64 = 54_000;

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

        temp_sink.write_bytes(metadata_bytes.as_slice())?;
        final_sink.write_bytes(metadata_bytes.as_slice())?;

        temp_sink.write_bytes(patch_data.as_slice())?;
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
            current_blob_bookmarks: Vec::new(),
            current_blob_offset: u64::try_from(current_blob_offset).expect("failed to read"),
            poisoned: false,
            header: metadata,
            last_state_to_diff: None,
            recycled_state: None,
            counters: BTreeMap::new(),
            sink: Some(SinkTuple {
                final_sink, temp_sink
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

        self.do_with_poison(|this| {
            let write_instructions = blob.write_packet_instructions();
            let offset = this.current_blob_offset;

            let (final_sink, temp_sink) = this.get_sinks();

            let written = final_sink.write_packet_data(&write_instructions)?;
            let written = u64::try_from(written).expect("failing to convert written blob size from usize to u64");

            // Keep the temp file identical to the final file during the verbatim-copy phase: there
            // is no in-progress region yet, so truncating to the current offset is a no-op that
            // simply guards against any stray trailing bytes before we re-append the blob.
            temp_sink.truncate(offset)?;
            temp_sink.write_packet_data(&write_instructions)?;

            this.current_blob_offset = offset.checked_add(written).expect("overflowed adding current_blob_offset");
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
        self.current_blob_bookmarks.clear();
    }

    /// Returns `true` if the stream was closed.
    #[inline]
    pub fn is_closed(&self) -> bool {
        self.sink.is_none()
    }

    /// Close the replay file recorder.
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
        let flush_result = self.next_blob();

        let Some(SinkTuple { final_sink, temp_sink }) = self.sink.take() else {
            unreachable!();
        };

        self.poisoned = true;

        match flush_result {
            Ok(()) => Ok((final_sink, temp_sink)),
            Err(e) => Err((final_sink, temp_sink, e)),
        }
    }

    /// Returns true if an unrecoverable error occurred.
    pub const fn is_poisoned(&self) -> bool {
        self.poisoned
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

    /// Add a bookmark.
    pub fn add_bookmark<S: Into<String>>(&mut self, name: S) -> Result<(), ReplayFileWriteError> {
        self.assert_not_closed()?;
        let bookmark_data = BookmarkMetadata {
            name: name.into(),
            elapsed_frames: self.elapsed_frames,
            elapsed_millis: self.elapsed_millis
        };

        self.current_blob_bookmarks.push(bookmark_data.clone());
        self.write_packet_data(&Packet::Bookmark {
            metadata: bookmark_data
        })
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
        assert!(self.elapsed_millis <= elapsed_millis, "Bad timestamp given (time went backwards!!!); expected {} (current) <= {elapsed_millis} (last)", self.elapsed_millis);
        self.assert_not_closed()?;

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

        let mut state = state;
        let delta = match self.last_state_to_diff.as_ref() {
            Some(previous) if previous.len() == state.len() => {
                if self.settings.mask_transient_buffers {
                    apply_masks(self.header.console_type.get_or_default(), previous.as_slice(), state.as_mut_slice());
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
            self.write_packet_data(&Packet::Keyframe {
                metadata,
                state: state.clone()
            })?;
        }

        // The next delta is taken against the state exactly as the player will reconstruct it
        // (masked, if it was masked).
        let displaced = self.last_state_to_diff.replace(state);
        self.recycle_state(displaced);

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

    fn next_blob(&mut self) -> Result<(), ReplayFileWriteError> {
        self.do_with_poison(|this| {
            // Nothing buffered (e.g. closing immediately after a blob split): a blob with no
            // keyframes is invalid and would panic below, so there is simply nothing to flush.
            if this.current_blob_keyframes.is_empty() {
                return Ok(());
            }

            let uncompressed_size = this.current_blob.len();
            let compressed = crate::compress_data(this.current_blob.as_slice(), this.settings.compression_level)
                .map_err(|e| ReplayFileWriteError::Other { explanation: Cow::Owned(format!("next_blob failed to compress: {e}")) })?;

            this.current_blob.clear();

            let keyframes_len = this.current_blob_keyframes.len();

            let first_keyframe =  this.current_blob_keyframes.first().expect("no keyframes in blob?");

            let compressed_blob = Packet::CompressedBlob {
                elapsed_frames_start: first_keyframe.elapsed_frames,
                elapsed_frames_end: this.elapsed_frames,
                timestamp_start: first_keyframe.elapsed_millis,
                timestamp_end: this.elapsed_millis,

                keyframes: core::mem::take(&mut this.current_blob_keyframes),
                bookmarks: core::mem::take(&mut this.current_blob_bookmarks),
                compressed_data: ByteVec::Heap(compressed),
                uncompressed_size: u64::try_from(uncompressed_size).expect("failed to convert uncompressed_size from usize to u64"),
            };

            this.current_blob_keyframes.reserve(keyframes_len + 1024);

            let write_instructions = compressed_blob.write_packet_instructions();

            let current_blob_offset_old = this.current_blob_offset;

            let (final_sink, temporary_sink) = this.get_sinks();

            let written = final_sink.write_packet_data(&write_instructions)?;
            let written = u64::try_from(written).expect("failing to convert written packet data from usize to u64");
            temporary_sink.truncate(current_blob_offset_old)?;
            temporary_sink.write_packet_data(&write_instructions)?;

            this.current_blob_offset = current_blob_offset_old.checked_add(written).expect("overflowed adding current_blob_offset");

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

    fn write_packet_data<'a, P: PacketIO<'a>>(&mut self, what: &'a P) -> Result<(), ReplayFileWriteError> {
        self.do_with_poison(|this| {
            this.write_packet_unchecked(what)?;
            Ok(())
        })
    }

    fn write_packet_unchecked<'a, P: PacketIO<'a>>(&mut self, what: &'a P) -> Result<(), ReplayFileWriteError> {
        let instructions = what.write_packet_instructions();
        self.current_blob.write_packet_data(&instructions)?;
        self.sink.as_mut().expect("write_packet_data on None sink").temp_sink.write_packet_data(&instructions)?;
        Ok(())
    }

    fn get_sinks(&mut self) -> (&mut Final, &mut Temp) {
        let sink = self.sink.as_mut().expect("can't get sinks (already closed?)");

        (&mut sink.final_sink, &mut sink.temp_sink)
    }

    fn do_with_poison<T, F: FnOnce(&mut Self) -> Result<T, ReplayFileWriteError>>(&mut self, f: F) -> Result<T, ReplayFileWriteError> {
        self.assert_not_closed()?;
        if self.poisoned {
            return Err(ReplayFileWriteError::Poisoned)
        }
        self.poisoned = true;
        let result = f(self)?;
        self.poisoned = false;
        Ok(result)
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
        self.header.crop_start = true;
        self.header.crop_timer_offset = timer_offset;
        self.sync_header()
    }

    /// Mark the current position as the end.
    pub fn mark_end(&mut self) -> Result<(), ReplayFileWriteError> {
        self.header.crop_end_frame = self.elapsed_frames;
        self.header.crop_end_millis = self.elapsed_millis;
        self.header.crop_end = true;
        self.sync_header()
    }

    fn sync_header(&mut self) -> Result<(), ReplayFileWriteError> {
        let header_bytes = *self.header.as_bytes();
        self.do_with_poison(|f| {
            let (final_sink, temp_sink) = f.get_sinks();
            temp_sink.overwrite_header(&header_bytes)?;
            final_sink.overwrite_header(&header_bytes)?;
            Ok(())
        })
    }

    /// Modify the counter.
    pub fn change_counter(&mut self, name: String, delta: SignedInteger) -> Result<(), ReplayFileWriteError> {
        self.do_with_poison(|f| {
            if let Some(v) = f.counters.get_mut(&name) {
                *v = v.wrapping_add(delta);
            }
            else {
                f.counters.insert(name.clone(), delta);
            }

            f.write_packet_unchecked(&Packet::IncrementCounter { name, delta })?;
            Ok(())
        })
    }
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
        self.flush()?;
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
        let _ = self.flush();

        let this = self.get_mut();
        this.set_len(size)?;
        this.seek(SeekFrom::End(0))?;
        Ok(())
    }

    fn overwrite_header(&mut self, header_data: &ReplayHeaderBytes) -> Result<(), ReplayFileWriteError> {
        let _ = self.flush();
        let this = self.get_mut();
        this.overwrite_header(header_data)
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

    /// The stream was broken by a previous error. The stream is no longer functional.
    Poisoned,

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
    fn add_bookmark(&mut self, name: String) -> Result<(), ReplayFileWriteError>;
    fn insert_keyframe(&mut self, state: ByteVec, timestamp_millis: TimestampMillis) -> Result<(), ReplayFileWriteError>;
    fn set_input(&mut self, input_buffer: InputBuffer) -> Result<(), ReplayFileWriteError>;
    fn reset_console(&mut self) -> Result<(), ReplayFileWriteError>;
    fn write_memory(&mut self, address: UnsignedInteger, data: ByteVec) -> Result<(), ReplayFileWriteError>;
    fn set_speed(&mut self, speed: Speed) -> Result<(), ReplayFileWriteError>;
    fn load_save_state(&mut self, state: ByteVec) -> Result<(), ReplayFileWriteError>;
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
    fn add_bookmark(&mut self, name: String) -> Result<(), ReplayFileWriteError> {
        self.add_bookmark(name)
    }

    #[inline]
    fn insert_keyframe(&mut self, state: ByteVec, timestamp: TimestampMillis) -> Result<(), ReplayFileWriteError> {
        self.insert_keyframe(state, timestamp)?;
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

    fn settings(max_frames_per_blob: u64, minimum_uncompressed_bytes_per_blob: usize) -> ReplayFileRecorderSettings {
        ReplayFileRecorderSettings {
            minimum_uncompressed_bytes_per_blob,
            max_frames_per_blob,
            compression_level: DEFAULT_ZSTD_COMPRESSION_LEVEL_V4,
            mask_transient_buffers: true,
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
}
