//! Replay file player functionality
//!
//! See [`ReplayFilePlayer`].

use alloc::borrow::Cow;
use alloc::format;
use alloc::vec::Vec;
use alloc::string::String;
use alloc::borrow::ToOwned;
use core::mem::transmute;
use alloc::sync::Arc;
use alloc::collections::BTreeMap;
use alloc::vec;
use crate::replay_file::bookmark_section::{decode_bookmark_section, BookmarkSectionError};
use crate::replay_file::{ReplayFileMetadata, ReplayHeaderBytes, ReplayHeaderRaw, REPLAY_VERSION_BOOKMARK_SECTION};
use crate::{apply_diff_in_place, apply_region_diff_in_place, BookmarkMetadata, BookmarkTable, ByteVec, KeyframeMetadata, Packet, PacketIO, PacketReadError, TimestampMillis, UnsignedInteger};
use crate::util::{decompress_data, launder_reference};

type KeyframeMap<'a> = BTreeMap<UnsignedInteger, Vec<&'a KeyframeMetadata>>;
type BookmarkMap<'a> = BTreeMap<String, Vec<&'a BookmarkMetadata>>;

/// Object that iterates through packets in a replay file.
pub struct ReplayFilePlayer {
    replay_file_metadata: ReplayFileMetadata,
    header_raw: ReplayHeaderRaw,
    patch_data: Option<Vec<u8>>,
    all_uncompressed_packets: Arc<Vec<Packet>>,
    keyframes: KeyframeMap<'static>,
    legacy_bookmarks: BookmarkMap<'static>,

    bookmark_table: BookmarkTable,
    bookmark_table_source: BookmarkTableSource,
    bookmark_section_error: Option<BookmarkSectionError>,
    stream_truncated: bool,

    total_frame_count: UnsignedInteger,
    total_millis: TimestampMillis,

    #[cfg(feature = "std")]
    compressed_blobs_decompressing: BTreeMap<usize, Option<DecompressionWorker>>,
    compressed_blobs_finished: BTreeMap<usize, Option<Arc<Vec<Packet>>>>,
    compressed_blob_uncompressed_packet_indices: Vec<usize>,
    cleanup_enabled: bool,

    next_uncompressed_packet_index: usize,
    next_compressed_packet_index: Option<usize>,

    /// The keyframe chain the cursor is currently on; see [`ChainState`].
    chain: ChainState,

    /// The most recently materialised delta keyframe, handed out by [`Self::next_packet`] as a
    /// [`Packet::Keyframe`]. Overwritten by the next delta the cursor passes.
    materialized: Option<Packet>,

    /// Whether [`Packet::Keyframe`]s handed out for delta keyframes carry a copy of the state
    /// (see [`Self::set_keyframe_states_wanted`]).
    keyframe_states_wanted: bool,

    #[cfg(feature = "std")]
    threading: bool
}

/// Where [`ReplayFilePlayer::bookmark_table`] came from.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum BookmarkTableSource {
    /// The bookmark section of a closed v5 file.
    Section,
    /// The newest `BookmarkTable` snapshot in the packet stream (a v5 file that was never closed, or
    /// whose section is damaged).
    StreamSnapshot,
    /// `Bookmark` packets of a pre-v5 file, converted to untyped points.
    Legacy,
    /// The replay has no bookmarks.
    None
}

/// Which packet list a chain position refers to.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum ListId {
    /// The top-level (uncompressed) packet list.
    TopLevel,
    /// The decompressed packet list of the blob at this top-level index.
    Blob(usize),
}

/// The state of the keyframe most recently passed by the cursor, and where it came from.
///
/// Delta keyframes ([`Packet::DeltaKeyframe`], [`Packet::RegionDeltaKeyframe`]) stay compact in
/// the packet lists and are materialised on demand by folding them into this running state.
///
/// Invariant: whenever the cursor sits just after a keyframe-class packet, `state` equals that
/// keyframe's full state. Both [`ReplayFilePlayer::next_packet`] and
/// [`ReplayFilePlayer::go_to_keyframe`] maintain it.
struct ChainState {
    /// The list `last_applied` indexes into.
    list: Option<ListId>,
    /// Index within that list of the last keyframe-class packet folded into `state`.
    last_applied: Option<usize>,
    state: Vec<u8>,
    /// Delta keyframes applied so far (see [`ReplayFilePlayer::chain_folds`]).
    folds: u64,
}

fn is_keyframe_class(packet: &Packet) -> bool {
    matches!(packet, Packet::Keyframe { .. } | Packet::DeltaKeyframe { .. } | Packet::RegionDeltaKeyframe { .. })
}

/// Keyframe metadata of any keyframe-class packet.
fn keyframe_metadata(packet: &Packet) -> Option<&KeyframeMetadata> {
    match packet {
        Packet::Keyframe { metadata, .. }
        | Packet::DeltaKeyframe { metadata, .. }
        | Packet::RegionDeltaKeyframe { metadata, .. } => Some(metadata),
        _ => None
    }
}

impl ChainState {
    const fn new() -> Self {
        Self { list: None, last_applied: None, state: Vec::new(), folds: 0 }
    }

    fn is_at(&self, list: ListId, index: usize) -> bool {
        self.list == Some(list) && self.last_applied == Some(index)
    }

    fn set_full(&mut self, list: ListId, index: usize, state: &[u8]) {
        self.state.clear();
        self.state.extend_from_slice(state);
        self.list = Some(list);
        self.last_applied = Some(index);
    }

    /// Check that the chain currently holds the state of the keyframe-class packet immediately
    /// preceding `index` in `list`.
    fn check_predecessor(&self, list: ListId, packets: &[Packet], index: usize) -> Result<(), ReplayFileReadError> {
        let valid = self.list == Some(list)
            && self.last_applied.is_some_and(|last| {
                last < index && !packets[last + 1..index].iter().any(is_keyframe_class)
            });

        if valid {
            Ok(())
        }
        else {
            Err(ReplayFileReadError::BrokenPacket { explanation: Cow::Borrowed("delta keyframe is not preceded by a keyframe in its chain") })
        }
    }

    /// Fold the packet at `index` of `list` into the chain (a no-op for non-keyframe packets).
    fn fold(&mut self, list: ListId, packets: &[Packet], index: usize) -> Result<(), ReplayFileReadError> {
        match &packets[index] {
            Packet::Keyframe { state, .. } => {
                self.set_full(list, index, state.as_slice());
            },

            Packet::DeltaKeyframe { diff, .. } => {
                self.check_predecessor(list, packets, index)?;
                if !apply_diff_in_place(&mut self.state, diff.as_slice()) {
                    return Err(ReplayFileReadError::BrokenPacket { explanation: Cow::Borrowed("delta keyframe failed to apply") });
                }
                self.last_applied = Some(index);
                self.folds += 1;
            },

            Packet::RegionDeltaKeyframe { state_len, control, data, .. } => {
                self.check_predecessor(list, packets, index)?;
                if *state_len != self.state.len() as UnsignedInteger {
                    return Err(ReplayFileReadError::BrokenPacket { explanation: Cow::Owned(format!("region delta keyframe expects a {state_len}-byte state but the chain holds {} bytes", self.state.len())) });
                }
                if !apply_region_diff_in_place(self.state.as_mut_slice(), control.as_slice(), data.as_slice()) {
                    return Err(ReplayFileReadError::BrokenPacket { explanation: Cow::Borrowed("region delta keyframe is malformed") });
                }
                self.last_applied = Some(index);
                self.folds += 1;
            },

            _ => {}
        }

        Ok(())
    }
}

impl ReplayFilePlayer {
    /// Try to read a buffer.
    ///
    /// If `allow_some_corruption`, then the parser will break early if it detects a corrupted
    /// packet and there is still some sort of usable stream. Otherwise, it will return `Err`.
    pub fn new<B: AsRef<[u8]>>(data: B, allow_some_corruption: bool) -> Result<ReplayFilePlayer, ReplayFileReadError> {
        let buffer_bytes = data.as_ref();
        let Some(header_buffer) = buffer_bytes.get(..size_of::<ReplayHeaderBytes>()) else {
            return Err(ReplayFileReadError::InvalidReplayFile { explanation: Cow::Borrowed("cannot read header") });
        };

        let header_buffer_bytes: &ReplayHeaderBytes = header_buffer.try_into().expect("should be able to convert array");
        let header_raw = ReplayHeaderRaw::from_bytes(header_buffer_bytes);
        let replay_file_metadata = header_raw
            .parse()
            .map_err(|e| ReplayFileReadError::InvalidReplayFile { explanation: Cow::Owned(format!("Failed to read header: {e}")) })?;

        let patch_start = header_buffer_bytes.len();
        let patch_length = usize::try_from(header_raw.patch_data_length)
            .map_err(|_| ReplayFileReadError::InvalidReplayFile { explanation: Cow::Borrowed("Cannot read patch length (exceeds usize)") })?;
        let patch_end = patch_length.checked_add(patch_start)
            .ok_or_else(|| ReplayFileReadError::InvalidReplayFile { explanation: Cow::Borrowed("Cannot read patch end (overflowed usize)") })?;

        let patch_data;
        if patch_length > 0 {
            let patch_range = patch_start..patch_end;
            let patch_bytes = buffer_bytes
                .get(patch_range)
                .ok_or_else(|| ReplayFileReadError::InvalidReplayFile { explanation: Cow::Borrowed("Cannot read patch end (out-of-bounds)") })?;

            patch_data = Some(patch_bytes.to_owned());
        }
        else {
            patch_data = None;
        }

        // A closed v5 file records where its packets end; the bookmark section follows.
        let mut stream_truncated = false;
        let stream_end = match header_raw.packet_stream_end() {
            None => buffer_bytes.len(),
            Some(end) => match usize::try_from(end) {
                Ok(end) if end >= patch_end && end <= buffer_bytes.len() => end,
                Ok(end) if end >= patch_end && allow_some_corruption => {
                    stream_truncated = true;
                    buffer_bytes.len()
                },
                _ => return Err(ReplayFileReadError::InvalidReplayFile { explanation: Cow::Owned(format!("The packet stream ends out of bounds (byte {end} of a {}-byte file)", buffer_bytes.len())) })
            }
        };

        let mut replay_data = buffer_bytes.get(patch_end..stream_end)
            .ok_or_else(|| ReplayFileReadError::InvalidReplayFile { explanation: Cow::Borrowed("Cannot read replay data (out-of-bounds)") })?;

        let mut all_packets = Vec::new();

        while !replay_data.is_empty() {
            match Packet::read_all(&mut replay_data, header_raw.replay_version) {
                Ok(n) => all_packets.push(n),
                Err(_) if allow_some_corruption => {
                    stream_truncated = true;
                    break
                },
                Err(PacketReadError::NotEnoughData) => return Err(ReplayFileReadError::BrokenPacket { explanation: Cow::Borrowed("not enough data for a packet") }),
                Err(PacketReadError::ParseFail { explanation }) => return Err(ReplayFileReadError::BrokenPacket { explanation: Cow::Owned(format!("Parse failure: {explanation}")) })
            }
        }

        // Every top-level delta keyframe must be preceded (after any blob) by a full keyframe in the
        // top-level list, or it can never be materialised. Deltas inside blobs are checked when the
        // blob is decompressed (a blob always starts with a full keyframe).
        let mut have_top_level_keyframe = false;
        for (packet_index, packet) in all_packets.iter().enumerate() {
            match packet {
                Packet::Keyframe { .. } => have_top_level_keyframe = true,
                Packet::CompressedBlob { .. } => have_top_level_keyframe = false,
                Packet::DeltaKeyframe { .. } | Packet::RegionDeltaKeyframe { .. } if !have_top_level_keyframe => {
                    if allow_some_corruption {
                        all_packets.truncate(packet_index);
                        stream_truncated = true;
                        break;
                    }
                    return Err(ReplayFileReadError::BrokenPacket { explanation: Cow::Borrowed("Delta keyframe without a prior keyframe") })
                },
                _ => {}
            }
        }

        let all_packets = Arc::new(all_packets);

        let Some(first_packet) = all_packets.get(0) else {
            return Err(ReplayFileReadError::InvalidReplayFile { explanation: Cow::Borrowed("No packets detected in replay file") })
        };

        match first_packet {
            Packet::CompressedBlob { keyframes, .. } => {
                if keyframes.is_empty() {
                    return Err(ReplayFileReadError::InvalidReplayFile { explanation: Cow::Borrowed("Replay starts with a compressed blob with no keyframes") })
                }
            }
            Packet::Keyframe { .. } => {},
            _ => return Err(ReplayFileReadError::InvalidReplayFile { explanation: Cow::Borrowed("Replay does not start with a keyframe") })
        }

        let mut all_keyframes = KeyframeMap::new();
        let mut all_bookmarks = BookmarkMap::new();

        let mut total_frame_count: UnsignedInteger = 0;
        let mut total_millis: UnsignedInteger = 0;

        macro_rules! add_keyframe {
            ($metadata:expr) => {{
                total_frame_count = $metadata.elapsed_frames;
                match all_keyframes.get_mut(&$metadata.elapsed_frames) {
                    Some(n) => n.push($metadata),
                    None => { all_keyframes.insert( $metadata.elapsed_frames, vec![$metadata]); }
                }
            }};
        }

        macro_rules! add_bookmark {
            ($metadata:expr) => {
                match all_bookmarks.get_mut(&$metadata.name) {
                    Some(n) => n.push($metadata),
                    None => { all_bookmarks.insert($metadata.name.clone(), vec![$metadata]); }
                }
            };
        }

        #[cfg(feature = "std")]
        let mut compressed_blobs_decompressing = BTreeMap::new();
        let mut compressed_blobs_finished = BTreeMap::new();
        let mut compressed_blob_indices = Vec::new();

        for (packet_index, packet) in all_packets.iter().enumerate() {
            match packet {
                Packet::CompressedBlob {
                    keyframes,
                    bookmarks,
                    uncompressed_size,
                    timestamp_end,
                    elapsed_frames_end,
                    ..
                } => {
                    // Vec works with up to isize maximum elements
                    if isize::try_from(*uncompressed_size).is_err() {
                        return Err(ReplayFileReadError::Other { explanation: Cow::Borrowed("Replay has a compressed blob that decompressed beyond the current architectural limits") });
                    }

                    #[cfg(feature = "std")]
                    compressed_blobs_decompressing.insert(packet_index, None);
                    compressed_blobs_finished.insert(packet_index, None);
                    compressed_blob_indices.push(packet_index);

                    if keyframes.is_empty() {
                        return Err(ReplayFileReadError::InvalidReplayFile { explanation: Cow::Borrowed("Replay has a compressed blob with no keyframes") })
                    }

                    #[allow(unused)]
                    for i in keyframes {
                        add_keyframe!(i)
                    }
                    for i in bookmarks {
                        add_bookmark!(i)
                    }

                    total_frame_count = *elapsed_frames_end;
                    total_millis = timestamp_end.0;
                },
                Packet::Keyframe { metadata, .. }
                | Packet::DeltaKeyframe { metadata, .. }
                | Packet::RegionDeltaKeyframe { metadata, .. } => {
                    add_keyframe!(metadata);
                },
                Packet::NextFrame { timestamp_delta } => {
                    total_frame_count = total_frame_count.checked_add(1)
                        .ok_or_else(|| ReplayFileReadError::InvalidReplayFile { explanation: Cow::Borrowed("frame or time counter overflowed") })?;
                    total_millis = total_millis.checked_add(timestamp_delta.0)
                        .ok_or_else(|| ReplayFileReadError::InvalidReplayFile { explanation: Cow::Borrowed("frame or time counter overflowed") })?;
                }
                Packet::Bookmark { metadata } => {
                    add_bookmark!(metadata);
                    total_millis = metadata.elapsed_millis.0;
                },
                _ => {}
            }
        }

        if all_keyframes.get(&0).is_none() {
            return Err(ReplayFileReadError::InvalidReplayFile { explanation: Cow::Borrowed("Replay has no keyframe at index 0") })
        }

        // Bookmarks: the section of a closed v5 file, else the newest snapshot in the stream, else
        // the legacy bookmark packets.
        let mut bookmark_section_error = None;
        let mut resolved = None;

        if header_raw.packet_stream_end().is_some() && !stream_truncated {
            match decode_bookmark_section(&buffer_bytes[stream_end..]) {
                Ok(table) => resolved = Some((table, BookmarkTableSource::Section)),
                Err(e) => bookmark_section_error = Some(e)
            }
        }

        if resolved.is_none() && header_raw.replay_version >= REPLAY_VERSION_BOOKMARK_SECTION {
            resolved = newest_stream_snapshot(&header_raw, all_packets.as_slice()).map(|table| (table, BookmarkTableSource::StreamSnapshot));
        }

        if resolved.is_none() && !all_bookmarks.is_empty() {
            let table = BookmarkTable::from_legacy(all_bookmarks.values().flatten().copied());
            resolved = Some((table, BookmarkTableSource::Legacy));
        }

        let (bookmark_table, bookmark_table_source) = resolved.unwrap_or((BookmarkTable::new(), BookmarkTableSource::None));

        let player = ReplayFilePlayer {
            patch_data,
            replay_file_metadata,
            keyframes: unsafe { transmute::<KeyframeMap, KeyframeMap<'static>>(all_keyframes) },
            legacy_bookmarks: unsafe { transmute::<BookmarkMap, BookmarkMap<'static>>(all_bookmarks) },
            bookmark_table,
            bookmark_table_source,
            bookmark_section_error,
            stream_truncated,
            all_uncompressed_packets: all_packets,
            next_uncompressed_packet_index: 0usize,
            next_compressed_packet_index: None,
            compressed_blob_uncompressed_packet_indices: compressed_blob_indices,
            #[cfg(feature = "std")]
            compressed_blobs_decompressing,
            compressed_blobs_finished,
            total_frame_count,
            total_millis: TimestampMillis(total_millis),
            cleanup_enabled: true,
            header_raw: *header_raw,
            chain: ChainState::new(),
            materialized: None,
            keyframe_states_wanted: true,

            #[cfg(feature = "std")]
            threading: false
        };

        Ok(player)
    }

    /// Get the total frame count.
    pub fn get_total_frames(&self) -> UnsignedInteger {
        self.total_frame_count
    }

    /// Get the total milliseconds of the replay.
    pub fn get_total_milliseconds(&self) -> TimestampMillis {
        self.total_millis
    }

    /// Get the format version the file was written with.
    pub fn get_replay_version(&self) -> u32 {
        self.header_raw.replay_version
    }

    /// Enable decompression on a separate thread.
    ///
    /// The next compressed blob will be automatically decompressed in the background.
    ///
    /// This cannot be turned off once activated.
    ///
    /// The `std` feature is required to enable this.
    #[cfg(feature = "std")]
    pub fn enable_threading(&mut self) {
        self.threading = true;
    }

    /// Choose whether the [`Packet::Keyframe`] returned for a delta keyframe carries a copy of
    /// the reconstructed state (`true`, the default) or an empty `state` (`false`).
    ///
    /// Reconstructing the chain is cheap and always happens, but copying a multi-megabyte state
    /// for every keyframe the cursor passes is not; a consumer that only needs the state now and
    /// then (or never, when it does not resync) should turn this off and read
    /// [`Self::current_keyframe_state`] instead when it does.
    pub fn set_keyframe_states_wanted(&mut self, wanted: bool) {
        self.keyframe_states_wanted = wanted;
    }

    /// The full state of the keyframe-class packet most recently returned by
    /// [`Self::next_packet`], regardless of [`Self::set_keyframe_states_wanted`]. Empty before
    /// the first keyframe.
    pub fn current_keyframe_state(&self) -> &[u8] {
        &self.chain.state
    }

    /// Get a reference to a map of keyframes.
    ///
    /// The key is the frame count.
    pub fn all_keyframes(&self) -> &BTreeMap<UnsignedInteger, Vec<&KeyframeMetadata>> {
        &self.keyframes
    }

    /// The legacy (pre-v5) `Bookmark` packets of the replay, keyed by name.
    ///
    /// Use [`Self::bookmark_table`] instead; it includes these, converted.
    pub fn legacy_bookmarks(&self) -> &BTreeMap<String, Vec<&BookmarkMetadata>> {
        &self.legacy_bookmarks
    }

    /// The replay's bookmarks, from wherever they could be read (see
    /// [`Self::bookmark_table_source`]).
    pub fn bookmark_table(&self) -> &BookmarkTable {
        &self.bookmark_table
    }

    /// Where [`Self::bookmark_table`] came from.
    pub fn bookmark_table_source(&self) -> BookmarkTableSource {
        self.bookmark_table_source
    }

    /// Why the bookmark section of a closed v5 file could not be used, if it could not.
    pub fn bookmark_section_error(&self) -> Option<&BookmarkSectionError> {
        self.bookmark_section_error.as_ref()
    }

    /// Whether part of the file was dropped because it was damaged (only possible when the player
    /// was created with `allow_some_corruption`).
    pub fn stream_truncated(&self) -> bool {
        self.stream_truncated
    }

    /// The header exactly as it is in the file.
    pub fn raw_header_bytes(&self) -> ReplayHeaderBytes {
        *self.header_raw.as_bytes()
    }

    /// Total delta keyframes applied so far while materialising keyframes (a seek to a full
    /// keyframe applies none).
    pub fn chain_folds(&self) -> u64 {
        self.chain.folds
    }

    /// Get all top-level uncompressed packets.
    pub fn all_uncompressed_packets(&self) -> &[Packet] {
        self.all_uncompressed_packets.as_slice()
    }

    /// Get the replay metadata.
    pub fn get_replay_metadata(&self) -> &ReplayFileMetadata {
        &self.replay_file_metadata
    }

    /// Get the patch data, if any.
    pub fn get_patch_data(&self) -> Option<&[u8]> {
        self.patch_data.as_ref().map(|i| i.as_slice())
    }

    /// Go to the given keyframe.
    ///
    /// Afterwards the next call to [`Self::next_packet`] yields that keyframe as a
    /// [`Packet::Keyframe`] (materialised if it is stored as a delta).
    ///
    /// When several keyframes share the frame (a full keyframe forced for a keyframe bookmark right
    /// after a scheduled one), the last one in the stream is used.
    ///
    /// On failure, `Err` is returned.
    pub fn go_to_keyframe(&mut self, keyframe_frames_index: UnsignedInteger) -> Result<(), ReplaySeekError> {
        let (list, packets, target) = self.locate_keyframe(keyframe_frames_index)?;

        self.fast_forward_chain(list, packets.as_slice(), target).map_err(|error| ReplaySeekError::ReadError { error })?;

        match list {
            ListId::TopLevel => {
                self.next_uncompressed_packet_index = target;
                self.next_compressed_packet_index = None;
            },
            ListId::Blob(top_index) => {
                self.next_uncompressed_packet_index = top_index;
                self.next_compressed_packet_index = Some(target);
            }
        }

        Ok(())
    }

    /// Whether the keyframe [`Self::go_to_keyframe`] would use for this frame is stored as a full
    /// keyframe (so seeking to it applies no deltas). Decompresses its blob if needed; does not
    /// move the cursor.
    pub fn keyframe_is_stored_full(&mut self, keyframe_frames_index: UnsignedInteger) -> Result<bool, ReplaySeekError> {
        let (_, packets, target) = self.locate_keyframe(keyframe_frames_index)?;
        Ok(matches!(packets[target], Packet::Keyframe { .. }))
    }

    /// Find the list and index of the last keyframe-class packet on the given frame.
    fn locate_keyframe(&mut self, keyframe_frames_index: UnsignedInteger) -> Result<(ListId, Arc<Vec<Packet>>, usize), ReplaySeekError> {
        if self.keyframes.get(&keyframe_frames_index).is_none() {
            return Err(ReplaySeekError::NoSuchKeyframe {
                given: keyframe_frames_index,
                best: self.keyframes.keys().copied().filter(|i| *i <= keyframe_frames_index).max().expect("there is always a keyframe at frame index 0")
            })
        };

        let matches_frame = |packet: &Packet| keyframe_metadata(packet).is_some_and(|m| m.elapsed_frames == keyframe_frames_index);

        // Locate the last list holding the keyframe. Keyframes on one frame are never separated by
        // a NextFrame, so the scan stops at the first NextFrame or keyframe-less blob after a match.
        let mut location = None;
        for (top_index, packet) in self.all_uncompressed_packets.iter().enumerate() {
            match packet {
                Packet::CompressedBlob { keyframes, .. } => {
                    if keyframes.iter().any(|k| k.elapsed_frames == keyframe_frames_index) {
                        location = Some((ListId::Blob(top_index), None));
                    }
                    else if location.is_some() {
                        break;
                    }
                },
                p if matches_frame(p) => {
                    location = Some((ListId::TopLevel, Some(top_index)));
                },
                Packet::NextFrame { .. } if location.is_some() => break,
                _ => continue
            }
        }

        let Some((list, top_level_target)) = location else {
            return Err(ReplaySeekError::ReadError { error: ReplayFileReadError::Other { explanation: Cow::Borrowed("keyframe is in the index but not in the packet stream") } })
        };

        match list {
            ListId::TopLevel => Ok((list, self.all_uncompressed_packets.clone(), top_level_target.expect("top-level location has an index"))),
            ListId::Blob(top_index) => {
                self.decompress_immediately(top_index).map_err(|error| ReplaySeekError::ReadError { error })?;

                let packets = self.compressed_blobs_finished
                    .get(&top_index)
                    .expect("somehow did not find the blob we just found in compressed_blobs_finished...")
                    .clone()
                    .expect("somehow the blob we just decompressed is not decompressed");

                let Some(target) = packets.iter().rposition(matches_frame) else {
                    return Err(ReplaySeekError::ReadError { error: ReplayFileReadError::BrokenPacket { explanation: Cow::Borrowed("keyframe listed in the blob metadata is not in the blob") } })
                };

                Ok((list, packets, target))
            }
        }
    }

    /// Bring the chain to the keyframe-class packet just before `target` in `list`, so that
    /// materialising `target` itself only needs one more fold.
    ///
    /// Forward seeks within the chain the cursor is already on are incremental; anything else
    /// restarts from the nearest full keyframe at or before `target` (which is also where an
    /// incremental seek starts if such a keyframe lies between the chain position and the target,
    /// since a full keyframe resets the chain anyway).
    fn fast_forward_chain(&mut self, list: ListId, packets: &[Packet], target: usize) -> Result<(), ReplayFileReadError> {
        let Some(restart) = packets[..=target].iter().rposition(|p| matches!(p, Packet::Keyframe { .. })) else {
            return Err(ReplayFileReadError::BrokenPacket { explanation: Cow::Borrowed("delta keyframe with no full keyframe before it") })
        };

        let start = match self.chain.last_applied {
            Some(last) if self.chain.list == Some(list) && last <= target => (last + 1).max(restart),
            _ => restart
        };

        for index in start..target {
            self.chain.fold(list, packets, index)?;
        }

        Ok(())
    }

    /// Ensure the blob at `blob_packet_index` is decompressed and cached, using (and, if
    /// necessary, waiting for) a background worker if one is in flight for it.
    ///
    /// Never spins: a worker still running is waited for with a blocking `recv()`, and a worker
    /// that died before sending (see [`DecompressionWorker`]) is reaped and the blob is
    /// decompressed here instead.
    #[cfg(feature = "std")]
    fn decompress_immediately(&mut self, blob_packet_index: usize) -> Result<(), ReplayFileReadError> {
        if self.compressed_blobs_finished[&blob_packet_index].is_some() {
            return Ok(())
        }

        let worker = self.compressed_blobs_decompressing
            .get_mut(&blob_packet_index)
            .expect("compressed blob should be in working cache")
            .take();

        if let Some(worker) = worker {
            match worker.result.recv() {
                Ok(Ok(packets)) => {
                    *self.compressed_blobs_finished
                        .get_mut(&blob_packet_index)
                        .expect("compressed blob should be in finished cache") = Some(packets);
                    return Ok(());
                }
                Ok(Err(error)) => return Err(error),
                Err(std::sync::mpsc::RecvError) => {
                    // The worker died before sending (an early exit; a panic would abort the whole
                    // process under this app's release profile, but not under an unwinding build
                    // such as `cargo test`). Reap the thread and fall back to decompressing here.
                    let _ = worker.handle.join();
                }
            }
        }

        self.decompress_on_this_thread(blob_packet_index)
    }

    /// [`Self::decompress_immediately`] without a background worker: the `no_std` configuration
    /// never spawns a thread, so this is the only path.
    #[cfg(not(feature = "std"))]
    fn decompress_immediately(&mut self, blob_packet_index: usize) -> Result<(), ReplayFileReadError> {
        if self.compressed_blobs_finished[&blob_packet_index].is_some() {
            return Ok(())
        }

        self.decompress_on_this_thread(blob_packet_index)
    }

    /// Decompress the blob at `blob_packet_index` on the calling thread and cache the result.
    /// Assumes it is not already cached.
    fn decompress_on_this_thread(&mut self, blob_packet_index: usize) -> Result<(), ReplayFileReadError> {
        let Some(Packet::CompressedBlob { compressed_data, uncompressed_size, .. }) = self.all_uncompressed_packets.get(blob_packet_index) else {
            panic!("decompress_immediately on {blob_packet_index} failed because it's not a compressed blob packet...")
        };

        let packets = decompress_compressed_blob(
            &self.header_raw,
            compressed_data.as_slice(),
            usize::try_from(*uncompressed_size).expect("we checked uncompressed size converting earlier")
        )?;

        *self.compressed_blobs_finished
            .get_mut(&blob_packet_index)
            .expect("compressed blob should be in finished cache") = Some(packets);

        Ok(())
    }

    /// Get the next packet in the stream.
    ///
    /// Delta keyframes are materialised and handed out as [`Packet::Keyframe`]s; callers never see
    /// [`Packet::DeltaKeyframe`], [`Packet::RegionDeltaKeyframe`] or [`Packet::CompressedBlob`].
    ///
    /// If there is no packet, `Ok(None)` will be returned.
    pub fn next_packet(&mut self) -> Result<Option<&Packet>, ReplayFileReadError> {
        loop {
            let packet_index = self.next_uncompressed_packet_index;
            if packet_index >= self.all_uncompressed_packets.len() {
                return Ok(None)
            }

            self.hint_decompress_next_blob_and_cleanup();

            if !matches!(self.all_uncompressed_packets[packet_index], Packet::CompressedBlob { .. }) {
                self.next_uncompressed_packet_index += 1;
                let packets = self.all_uncompressed_packets.clone();
                return self.materialize(ListId::TopLevel, &packets, packet_index).map(Some);
            }

            self.decompress_immediately(packet_index)?;

            let packets = self.compressed_blobs_finished
                .get(&packet_index)
                .expect("compressed blob not found in finished cache")
                .clone()
                .expect("should be decompressed but wasn't for some reason???");

            let inner_index = self.next_compressed_packet_index.unwrap_or(0);
            if inner_index >= packets.len() {
                self.next_compressed_packet_index = None;
                self.next_uncompressed_packet_index += 1;
                continue;
            }

            self.next_compressed_packet_index = Some(inner_index + 1);
            return self.materialize(ListId::Blob(packet_index), &packets, inner_index).map(Some);
        }
    }

    /// Fold the packet at `index` of `list` into the chain and return what the caller should see:
    /// the packet itself, or a materialised [`Packet::Keyframe`] for a delta.
    fn materialize(&mut self, list: ListId, packets: &Arc<Vec<Packet>>, index: usize) -> Result<&Packet, ReplayFileReadError> {
        let packet = &packets[index];

        match packet {
            Packet::Keyframe { state, .. } => {
                self.chain.set_full(list, index, state.as_slice());
            },

            Packet::DeltaKeyframe { metadata, .. } | Packet::RegionDeltaKeyframe { metadata, .. } => {
                // A repeated seek to the same delta finds it already folded; otherwise fold it now.
                if !self.chain.is_at(list, index) {
                    self.chain.fold(list, packets.as_slice(), index)?;
                }

                let state = if self.keyframe_states_wanted {
                    ByteVec::Heap(self.chain.state.clone())
                }
                else {
                    ByteVec::new()
                };
                self.materialized = Some(Packet::Keyframe {
                    metadata: metadata.clone(),
                    state
                });

                return Ok(self.materialized.as_ref().expect("just set"));
            },

            _ => {}
        }

        // SAFETY: `packets` is the top-level list or a cached decompressed blob, both of which are
        // owned by `self` and never mutated after construction. The returned reference is bound to
        // the `&mut self` borrow, so the cache cannot be cleaned up (which only happens inside
        // `next_packet`) while the caller still holds it.
        Ok(unsafe { launder_reference(packet) })
    }

    /// Decompress all blobs.
    ///
    /// Decompressed blobs hold compact packet lists (delta keyframes are only materialised as the
    /// cursor passes them), so this costs roughly the compressed size of the file, not the sum of
    /// all keyframe states.
    pub fn decompress_all_blobs(&mut self) {
        self.cleanup_enabled = false;

        for (index, packet) in self.all_uncompressed_packets.clone().iter().enumerate() {
            if let Packet::CompressedBlob { .. } = packet {
                let _ = self.decompress_immediately(index);
            }
        }
    }

    fn hint_decompress_next_blob_and_cleanup(&mut self) {
        let current_frame_index = self.next_uncompressed_packet_index;

        if self.cleanup_enabled {
            let last_compressed_blob = self
                .compressed_blob_uncompressed_packet_indices
                .iter()
                .copied()
                .filter(|frame_index| *frame_index < current_frame_index)
                .last();

            if let Some(last_compressed_blob_packet_index) = last_compressed_blob {
                for i in 0..last_compressed_blob_packet_index {
                    self.compressed_blobs_finished.insert(i, None);
                    #[cfg(feature = "std")]
                    self.compressed_blobs_decompressing.insert(i, None);
                }
            }
        }

        #[cfg(feature = "std")]
        if self.threading {
            let next_compressed_blob = self
                .compressed_blob_uncompressed_packet_indices
                .iter()
                .copied()
                .filter(|frame_index| *frame_index > current_frame_index)
                .next();

            if let Some(next_compressed_blob_index) = next_compressed_blob {
                self.decompress_blob_threaded(next_compressed_blob_index);
            }
        }
    }

    /// Start (if not already running) or poll (without blocking) the background decompression of
    /// the blob at `blob_index`.
    #[cfg(feature = "std")]
    fn decompress_blob_threaded(&mut self, blob_index: usize) {
        if !self.threading {
            return;
        }

        if self.compressed_blobs_finished[&blob_index].is_some() {
            return;
        }

        let slot = self.compressed_blobs_decompressing
            .get_mut(&blob_index)
            .expect("compressed_blobs_decompressing exploded");

        let Some(worker) = slot else {
            // Not decompressing yet; start it.
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            let packets = self.all_uncompressed_packets.clone();
            let header = self.header_raw;

            let spawned = std::thread::Builder::new()
                .name("ReplayFilePlayer-decompression-thread".to_owned())
                .spawn(move || {
                    let Packet::CompressedBlob { uncompressed_size, compressed_data, .. } = packets
                        .get(blob_index)
                        .expect("failed to get packet") else {
                        panic!("compressed blob wasn't a compressed blob NOOOOO")
                    };
                    let result = decompress_compressed_blob(&header, compressed_data.as_slice(), usize::try_from(*uncompressed_size).expect("we checked this could be a usize!"));
                    // If the receiver side was dropped (e.g. a fresh spawn detached us, see the
                    // `slot.insert`/re-spawn path), the send just fails; there is nothing else to
                    // clean up here.
                    let _ = tx.send(result);
                });

            if let Ok(handle) = spawned {
                *slot = Some(DecompressionWorker { result: rx, handle });
            }
            // If spawning failed, leave the slot empty; a later hint or a direct
            // `decompress_immediately` will try again / fall back to the main thread.
            return;
        };

        match worker.result.try_recv() {
            Ok(Ok(packets)) => {
                self.compressed_blobs_finished.insert(blob_index, Some(packets));
                *slot = None;
            }
            Ok(Err(_)) => {
                // Decompression failed; drop the worker so a later demand retries on the main
                // thread, where the error is returned to the caller instead of silently dropped.
                *slot = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                // Still working; nothing to do.
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                // The worker died before sending. Reap the thread and clear the slot so the next
                // hint (or `decompress_immediately`) retries.
                if let Some(worker) = slot.take() {
                    let _ = worker.handle.join();
                }
            }
        }
    }
}

/// A background decompression of one compressed blob, started by
/// [`ReplayFilePlayer::enable_threading`].
///
/// `decompress_immediately` never spins on this: it blocks on [`Self::result`] (a blocking
/// `recv()`) rather than polling, and a worker that dies before sending (`RecvError`/
/// `TryRecvError::Disconnected`) is reaped with `handle.join()` and the blob is decompressed on
/// the calling thread instead, so a background failure can never wedge the caller at 100% CPU or
/// forever.
///
/// Note: under this app's release profile (`panic = "abort"`), a panic in the worker thread
/// aborts the whole process like any other panic, so the fallback below cannot occur from a panic
/// in a release build; it matters for unwinding builds (`cargo test`, a debug build) and for a
/// worker that exits early without panicking.
#[cfg(feature = "std")]
pub(crate) struct DecompressionWorker {
    result: std::sync::mpsc::Receiver<Result<Arc<Vec<Packet>>, ReplayFileReadError>>,
    handle: std::thread::JoinHandle<()>,
}

#[cfg(all(test, feature = "std"))]
impl ReplayFilePlayer {
    /// Test-only hook: inject a decompression worker for `blob_index`, replacing any existing one
    /// (which is simply detached — see [`ReplayFilePlayer::decompress_blob_threaded`]).
    pub(crate) fn inject_worker(&mut self, blob_index: usize, worker: DecompressionWorker) {
        self.compressed_blobs_decompressing.insert(blob_index, Some(worker));
    }
}

/// An error when seeking to a given a keyframe.
#[derive(Clone, PartialEq, Debug)]
pub enum ReplaySeekError {
    /// No keyframe at the given frame index.
    ///
    /// The keyframe before the given frame index is provided at `best`, instead.
    #[allow(missing_docs)]
    NoSuchKeyframe { given: UnsignedInteger, best: UnsignedInteger },

    /// An error occurred when seeking (usually a decompression error, or a delta keyframe that
    /// failed to apply).
    #[allow(missing_docs)]
    ReadError { error: ReplayFileReadError }
}

/// An error that occurred when reading
#[derive(Clone, PartialEq, Debug)]
#[allow(missing_docs)]
pub enum ReplayFileReadError {
    InvalidReplayFile { explanation: Cow<'static, str> },
    BrokenPacket { explanation: Cow<'static, str> },
    EndOfStream,
    Other { explanation: Cow<'static, str> }
}

/// The newest `BookmarkTable` snapshot of a stream: in the uncompressed tail, or else in the last
/// blob (the recorder repeats the table at the start of every blob once any was written, so older
/// blobs never hold a newer one).
fn newest_stream_snapshot(header: &ReplayHeaderRaw, packets: &[Packet]) -> Option<BookmarkTable> {
    for packet in packets.iter().rev() {
        match packet {
            Packet::BookmarkTable { table } => return Some(table.clone()),
            Packet::CompressedBlob { compressed_data, uncompressed_size, .. } => {
                let blob = decompress_compressed_blob(header, compressed_data.as_slice(), usize::try_from(*uncompressed_size).ok()?).ok()?;
                return blob.iter().rev().find_map(|p| match p {
                    Packet::BookmarkTable { table } => Some(table.clone()),
                    _ => None
                })
            },
            _ => {}
        }
    }
    None
}

/// Decompress a blob into its packet list.
///
/// Delta keyframes are left compact; they are materialised by the player's [`ChainState`] as the
/// cursor passes them. The first packet must be a full keyframe so every delta in the blob has a
/// base.
fn decompress_compressed_blob(header: &ReplayHeaderRaw, blob_data: &[u8], uncompressed_size: usize) -> Result<Arc<Vec<Packet>>, ReplayFileReadError> {
    let decompressed_data = decompress_data(blob_data, uncompressed_size)
        .map_err(|e| ReplayFileReadError::Other { explanation: Cow::Owned(format!("Decompression error: {e}")) })?;

    let mut b = decompressed_data.as_slice();
    let mut packets = Vec::new();

    while !b.is_empty() {
        packets.push(
            Packet::read_all(&mut b, header.replay_version).map_err(|i| ReplayFileReadError::BrokenPacket { explanation: Cow::Owned(format!("Failed to read packet - {i:?}")) })?
        )
    }

    if !matches!(packets.first(), Some(Packet::Keyframe { .. })) {
        return Err(ReplayFileReadError::InvalidReplayFile { explanation: Cow::Borrowed("first packet in a blob was not a keyframe") });
    }

    Ok(Arc::new(packets))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    /// A v3 file (both the crash-safe temp layout with a top-level delta tail and the closed
    /// all-blobs layout) must keep playing back exactly: identical materialised keyframe states,
    /// packet stream, totals and indexes.
    #[test]
    fn v3_fixture_plays_back_exactly() {
        check_script_replay(V3_SMALL, "v3-small (temp layout)");
        check_script_replay(V3_SMALL_CLOSED, "v3-small-closed");
    }

    #[test]
    fn region_deltas_materialise_at_top_level_and_after_seeks() {
        let s0 = pseudo_random_bytes(1, 1001);
        let mut s1 = s0.clone();
        s1[10..30].fill(0xAA);
        let mut s2 = s1.clone();
        s2[999..].fill(0x11);
        let s3 = pseudo_random_bytes(2, 1001); // rewritten: stored as a full keyframe

        let packets = [
            Packet::Keyframe { metadata: metadata_at(0), state: bv(&s0) },
            Packet::NextFrame { timestamp_delta: 16.into() },
            region_delta(1, &s0, &s1),
            Packet::NextFrame { timestamp_delta: 16.into() },
            region_delta(2, &s1, &s2),
            Packet::NextFrame { timestamp_delta: 16.into() },
            Packet::Keyframe { metadata: metadata_at(3), state: bv(&s3) },
            Packet::NextFrame { timestamp_delta: 16.into() },
            region_delta(4, &s3, &s0),
        ];
        let bytes = file_with_packets(&packets);
        let mut player = ReplayFilePlayer::new(&bytes, false).unwrap();
        assert_eq!(player.get_total_frames(), 4);
        assert_eq!(player.all_keyframes().keys().copied().collect::<Vec<_>>(), vec![0, 1, 2, 3, 4]);

        let expected = [(0u64, &s0), (1, &s1), (2, &s2), (3, &s3), (4, &s0)];
        let check = |player: &mut ReplayFilePlayer, frame: u64| {
            let state = expected.iter().find(|(f, _)| *f == frame).unwrap().1;
            match player.next_packet().unwrap() {
                Some(Packet::Keyframe { metadata, state: got }) => {
                    assert_eq!(metadata.elapsed_frames, frame);
                    assert_eq!(got.as_slice(), state.as_slice(), "state at frame {frame}");
                }
                other => panic!("expected keyframe {frame}, got {other:?}"),
            }
        };

        // Sequential.
        for frame in 0..=4 {
            check(&mut player, frame);
            if frame < 4 {
                assert!(matches!(player.next_packet().unwrap(), Some(Packet::NextFrame { .. })));
            }
        }
        assert!(player.next_packet().unwrap().is_none());

        // Backward, repeated, forward incremental, across the full-keyframe restart.
        for frame in [2u64, 2, 1, 4, 0, 3, 4, 2, 4, 1] {
            player.go_to_keyframe(frame).unwrap();
            check(&mut player, frame);
        }
    }

    /// When a blob closes between a scheduled keyframe and a forced full keyframe on the same frame,
    /// both blobs list that frame; the seek must use the later blob's full keyframe.
    #[test]
    fn duplicate_keyframes_resolve_to_the_last() {
        let s0 = pseudo_random_bytes(1, 1001);
        let mut s1 = s0.clone();
        s1[40..60].fill(0xEE);
        let s1_forced = pseudo_random_bytes(3, 1001);

        let top_level = [
            Packet::Keyframe { metadata: metadata_at(0), state: bv(&s0) },
            Packet::NextFrame { timestamp_delta: 16.into() },
            region_delta(1, &s0, &s1),
            Packet::Keyframe { metadata: metadata_at(1), state: bv(&s1_forced) },
            Packet::NextFrame { timestamp_delta: 16.into() },
            region_delta(2, &s1_forced, &s0),
        ];
        let blobs = [
            blob_of(&top_level[..3]),
            blob_of(&top_level[3..]),
        ];

        for (layout, bytes) in [("top level", file_with_packets(&top_level)), ("across blobs", file_with_packets(&blobs))] {
            let mut player = ReplayFilePlayer::new(&bytes, false).unwrap();
            assert_eq!(player.get_total_frames(), 2, "{layout}");
            assert_eq!(player.all_keyframes()[&1].len(), 2, "{layout}");
            assert!(player.keyframe_is_stored_full(1).unwrap(), "{layout}");

            let folds = player.chain_folds();
            player.go_to_keyframe(1).unwrap();
            match player.next_packet().unwrap() {
                Some(Packet::Keyframe { state, .. }) => assert_eq!(state.as_slice(), s1_forced.as_slice(), "{layout}"),
                other => panic!("{layout}: {other:?}")
            }
            assert_eq!(player.chain_folds(), folds, "{layout}: no delta applied");

            // The delta after it chains from the forced keyframe.
            player.go_to_keyframe(2).unwrap();
            match player.next_packet().unwrap() {
                Some(Packet::Keyframe { state, .. }) => assert_eq!(state.as_slice(), s0.as_slice(), "{layout}"),
                other => panic!("{layout}: {other:?}")
            }
        }
    }

    #[test]
    fn top_level_delta_without_a_keyframe_is_rejected_or_truncated() {
        // The fixture starts with a blob; a delta straight after it has no top-level base.
        let blob = v3_small_closed_first_blob();
        let s0 = pseudo_random_bytes(1, 64);
        let packets = [blob.clone(), region_delta(1, &s0, &s0)];
        let bytes = file_with_packets(&packets);

        assert!(matches!(ReplayFilePlayer::new(&bytes, false), Err(ReplayFileReadError::BrokenPacket { .. })));

        let player = ReplayFilePlayer::new(&bytes, true).unwrap();
        assert_eq!(player.all_uncompressed_packets().len(), 1, "the orphan delta is dropped");
        assert!(!player.all_keyframes().contains_key(&1));
    }

    fn v3_small_closed_first_blob() -> Packet {
        let player = ReplayFilePlayer::new(V3_SMALL_CLOSED, false).unwrap();
        player.all_uncompressed_packets()[0].clone()
    }

    #[test]
    fn malformed_deltas_fail_cleanly() {
        let s0 = pseudo_random_bytes(1, 64);
        let bad_control = Packet::RegionDeltaKeyframe { metadata: metadata_at(1), state_len: 64, control: bv(&[0, 30]), data: bv(&[0; 120]) };
        let bad_len = Packet::RegionDeltaKeyframe { metadata: metadata_at(2), state_len: 65, control: bv(&[]), data: bv(&[]) };
        let bad_v3 = Packet::DeltaKeyframe { metadata: metadata_at(3), diff: vec![(64u64 << 32) | 1] };

        for bad in [bad_control, bad_len, bad_v3] {
            let frame = keyframe_metadata(&bad).unwrap().elapsed_frames;
            let packets = [
                Packet::Keyframe { metadata: metadata_at(0), state: bv(&s0) },
                Packet::NextFrame { timestamp_delta: 16.into() },
                bad,
                Packet::NextFrame { timestamp_delta: 16.into() },
                Packet::Keyframe { metadata: metadata_at(frame + 1), state: bv(&s0) },
            ];
            let bytes = file_with_packets(&packets);
            let mut player = ReplayFilePlayer::new(&bytes, false).unwrap();

            // Sequential read fails at the delta...
            player.go_to_keyframe(0).unwrap();
            assert!(matches!(player.next_packet().unwrap(), Some(Packet::Keyframe { .. })));
            assert!(matches!(player.next_packet().unwrap(), Some(Packet::NextFrame { .. })));
            assert!(matches!(player.next_packet(), Err(ReplayFileReadError::BrokenPacket { .. })), "frame {frame}");

            // ...seeking to it fails at materialisation...
            player.go_to_keyframe(frame).unwrap();
            assert!(matches!(player.next_packet(), Err(ReplayFileReadError::BrokenPacket { .. })));

            // ...and the full keyframe after it is still reachable.
            player.go_to_keyframe(frame + 1).unwrap();
            assert!(matches!(player.next_packet().unwrap(), Some(Packet::Keyframe { metadata, .. }) if metadata.elapsed_frames == frame + 1));
        }
    }

    /// Threaded decompression must hand out exactly the same packets as decompressing on the
    /// calling thread: `enable_threading()` only changes *when* a blob's decompression work
    /// happens, never its result.
    #[cfg(feature = "std")]
    #[test]
    fn threaded_decompression_hands_out_the_same_packets() {
        fn walk(player: &mut ReplayFilePlayer) -> Vec<Packet> {
            player.go_to_keyframe(0).unwrap();
            let mut out = Vec::new();
            while let Some(packet) = player.next_packet().unwrap() {
                out.push(packet.clone());
            }
            out
        }

        let mut plain = ReplayFilePlayer::new(V3_SMALL_CLOSED, false).unwrap();
        let expected = walk(&mut plain);

        let mut threaded = ReplayFilePlayer::new(V3_SMALL_CLOSED, false).unwrap();
        threaded.enable_threading();
        let got = walk(&mut threaded);

        assert_eq!(got.len(), expected.len(), "packet count");
        for (i, (g, e)) in got.iter().zip(expected.iter()).enumerate() {
            assert_packet_eq(g, e, &format!("threaded packet {i}"));
        }
    }

    /// A worker whose thread drops the sender without sending (an early exit, or -- in an
    /// unwinding build -- a panic) must not wedge the reader: the fallback in
    /// `decompress_immediately` reaps it and decompresses on the calling thread instead.
    #[cfg(feature = "std")]
    #[test]
    fn dead_worker_falls_back_to_main_thread() {
        let mut player = ReplayFilePlayer::new(V3_SMALL_CLOSED, false).unwrap();
        player.enable_threading();

        let Packet::CompressedBlob { keyframes, .. } = &player.all_uncompressed_packets()[0] else {
            panic!("expected the first packet of the closed fixture to be a compressed blob")
        };
        let frame = keyframes[0].elapsed_frames;

        let (tx, rx) = std::sync::mpsc::sync_channel::<Result<Arc<Vec<Packet>>, ReplayFileReadError>>(1);
        // The thread does nothing but drop the sender: the channel disconnects without a value
        // ever being sent, simulating a worker that exited early (or, in an unwinding build,
        // panicked).
        let handle = std::thread::spawn(move || {
            drop(tx);
        });
        player.inject_worker(0, DecompressionWorker { result: rx, handle });

        // Seeking into that blob blocks on the dead worker, observes the disconnect, reaps the
        // thread, and falls back to decompressing on this thread -- it must still succeed rather
        // than spin or error out.
        player.go_to_keyframe(frame).unwrap();
        assert!(matches!(player.next_packet().unwrap(), Some(Packet::Keyframe { metadata, .. }) if metadata.elapsed_frames == frame));
    }
}
