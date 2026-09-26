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
use crate::replay_file::{ReplayConsoleType, ReplayFileMetadata, ReplayHeaderBytes, ReplayHeaderRaw, REPLAY_VERSION_BOOKMARK_SECTION};
use crate::util::decompress_data_with_prefix;
use crate::{apply_diff_in_place, apply_region_diff_in_place, BookmarkMetadata, BookmarkTable, ByteVec, KeyframeMetadata, Packet, PacketDiscriminator, PacketIO, PacketReadError, TimestampMillis, UnsignedInteger};
use crate::util::{decompress_data, launder_reference, region_diff_touches, BlobDecoder};
use crate::keyframe_masks::transient_ranges;

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
    /// Every blob's decoded bytes, keyed by top-level packet index; `None` until first needed (or
    /// after cleanup dropped it). See [`DecodedBlob`].
    decoded_blobs: BTreeMap<usize, Option<DecodedBlob>>,
    compressed_blob_uncompressed_packet_indices: Vec<usize>,
    cleanup_enabled: bool,

    next_uncompressed_packet_index: usize,
    /// While the cursor is inside the blob at `next_uncompressed_packet_index`: the byte offset
    /// (in the decompressed blob) of the next packet to hand out.
    next_blob_offset: Option<usize>,

    /// The keyframe chain the cursor is currently on; see [`ChainState`].
    chain: ChainState,
    /// The file's bytes, kept for 3DS files whose keyframe frames are read on demand.
    source: Option<Arc<dyn AsRef<[u8]> + Send + Sync>>,
    /// Set by a seek: the next stored keyframe handed out must be materialised even when
    /// keyframe states are not wanted (the seek's caller reads `current_keyframe_state`). A
    /// stored keyframe met in a plain sequential read is otherwise left alone: materialising a
    /// 170 MB 3DS state costs ~100 ms, which playback must not pay every 8 s; a later seek
    /// walks the chain from whatever is held.
    materialise_next: bool,
    /// `(frame, top-level packet index)` of every [`Packet::Thumbnail`], in frame order.
    thumbnails: Vec<(UnsignedInteger, usize)>,
    /// The packet most recently handed out by [`Self::next_packet`] when it was not a reference
    /// into the top-level list: a packet parsed out of a blob, or a materialised keyframe.
    scratch: Option<Packet>,
    /// See [`Self::set_keyframe_states_wanted`].
    keyframe_states_wanted: bool,
    /// See [`Self::current_keyframe_has_stale_transients`].
    keyframe_transients_stale: bool,

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
///
/// A position in the top-level list is a packet index; a position in a blob is a byte offset into
/// the blob's decompressed bytes (blobs are parsed on demand, never into a packet list).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum ListId {
    /// The top-level (uncompressed) packet list.
    TopLevel,
    /// The decompressed bytes of the blob at this top-level index.
    Blob(usize),
}

/// The state of the keyframe most recently passed by the cursor, and where it came from.
///
/// Delta keyframes ([`Packet::DeltaKeyframe`], [`Packet::RegionDeltaKeyframe`]) stay compact in
/// the file and are materialised on demand by folding them into this running state.
///
/// Invariant: whenever the cursor sits just after a keyframe-class packet, `state` equals that
/// keyframe's full state. Both [`ReplayFilePlayer::next_packet`] and
/// [`ReplayFilePlayer::go_to_keyframe`] maintain it.
struct ChainState {
    /// The list `last_applied` refers to.
    list: Option<ListId>,
    /// Position within that list (see [`ListId`]) of the last keyframe-class packet folded into
    /// `state`.
    last_applied: Option<usize>,
    state: Vec<u8>,
    /// Delta keyframes applied so far (see [`ReplayFilePlayer::chain_folds`]).
    folds: u64,
    /// Whether a region delta may change the state's length (3DS files only; see
    /// `region_diff_resizing`).
    resizing: bool,
    /// 3DS files (`StoredKeyframe`): the state after the most recent keyframe of level `<= i`,
    /// with its list position and decoded delta payload (the zstd prefix of the next level-`i`
    /// delta). Shared between levels when one keyframe set several of them.
    levels: [Option<LevelState>; 3],
    /// 3DS files: the materialised state of the last folded keyframe (`levels[2]`'s), which
    /// stands in for `state`.
    stored_state: Option<Arc<Vec<u8>>>,
}

/// A decoded [`Packet::Thumbnail`]: `(width, height, RGB565 pixels)` for each screen.
#[derive(Clone, Debug)]
pub struct ReplayThumbnail {
    /// The frame it was taken at.
    pub frame: UnsignedInteger,
    #[allow(missing_docs)]
    pub top: (u32, u32, Vec<u8>),
    #[allow(missing_docs)]
    pub bottom: (u32, u32, Vec<u8>),
}

/// See `ChainState::levels`.
#[derive(Clone)]
struct LevelState {
    position: usize,
    state: Arc<Vec<u8>>,
    payload: Arc<Vec<u8>>,
}

fn is_keyframe_class(packet: &Packet) -> bool {
    matches!(packet, Packet::Keyframe { .. } | Packet::DeltaKeyframe { .. } | Packet::RegionDeltaKeyframe { .. } | Packet::StoredKeyframe { .. })
}

/// Keyframe metadata of any keyframe-class packet.
fn keyframe_metadata(packet: &Packet) -> Option<&KeyframeMetadata> {
    match packet {
        Packet::Keyframe { metadata, .. }
        | Packet::DeltaKeyframe { metadata, .. }
        | Packet::RegionDeltaKeyframe { metadata, .. }
        | Packet::StoredKeyframe { metadata, .. } => Some(metadata),
        _ => None
    }
}

impl ChainState {
    const fn new(resizing: bool) -> Self {
        Self { list: None, last_applied: None, state: Vec::new(), folds: 0, resizing, levels: [None, None, None], stored_state: None }
    }

    /// The materialised state of the last folded keyframe.
    fn current(&self) -> &[u8] {
        match &self.stored_state {
            Some(state) => state.as_slice(),
            None => self.state.as_slice()
        }
    }

    fn is_at(&self, list: ListId, position: usize) -> bool {
        self.list == Some(list) && self.last_applied == Some(position)
    }

    fn set_full(&mut self, list: ListId, position: usize, state: &[u8]) {
        self.stored_state = None;
        self.state.clear();
        self.state.extend_from_slice(state);
        self.list = Some(list);
        self.last_applied = Some(position);
    }

    /// Whether the chain currently holds the state of the keyframe-class packet immediately
    /// preceding `position` in `list`. `keyframe_between(a, b)` tells whether any keyframe-class
    /// packet lies strictly between positions `a` and `b` of that list.
    fn holds_predecessor_of(&self, list: ListId, position: usize, keyframe_between: impl FnOnce(usize, usize) -> bool) -> bool {
        self.list == Some(list)
            && self.last_applied.is_some_and(|last| last < position && !keyframe_between(last, position))
    }

    /// Fold `packet`, which sits at `position` of `list`, into the chain (a no-op for
    /// non-keyframe packets). `predecessor_held` is [`Self::holds_predecessor_of`] for that
    /// position; a delta whose predecessor is not held cannot be applied.
    fn fold(&mut self, list: ListId, position: usize, packet: &Packet, predecessor_held: bool, source: Option<&[u8]>) -> Result<(), ReplayFileReadError> {
        match packet {
            Packet::Keyframe { state, .. } => {
                self.set_full(list, position, state.as_slice());
            },

            Packet::StoredKeyframe { level, state_len, uncompressed_len, frame_len, frame_offset, .. } => {
                let level = usize::from(*level);
                if level > 2 {
                    return Err(ReplayFileReadError::BrokenPacket { explanation: Cow::Borrowed("3DS keyframe has an unknown level") });
                }
                let Some(source) = source else {
                    return Err(ReplayFileReadError::Other { explanation: Cow::Borrowed("3DS keyframe data is not available: the replay file is not kept open") });
                };
                let broken = |what: &'static str| ReplayFileReadError::BrokenPacket { explanation: Cow::Borrowed(what) };
                let start = usize::try_from(*frame_offset).map_err(|_| broken("3DS keyframe offset does not fit"))?;
                let len = usize::try_from(*frame_len).map_err(|_| broken("3DS keyframe length does not fit"))?;
                let frame = start.checked_add(len).and_then(|end| source.get(start..end)).ok_or_else(|| broken("3DS keyframe frame is outside the file"))?;
                let uncompressed = usize::try_from(*uncompressed_len).map_err(|_| broken("3DS keyframe uncompressed length does not fit"))?;
                let wanted = usize::try_from(*state_len).map_err(|_| broken("3DS keyframe state length does not fit"))?;
                let decode_error = |e: Cow<'static, str>| ReplayFileReadError::BrokenPacket { explanation: Cow::Owned(format!("3DS keyframe failed to decode: {e}")) };

                if level == 0 {
                    let state = decompress_data_with_prefix(frame, uncompressed, &[]).map_err(decode_error)?;
                    if state.len() != wanted {
                        return Err(broken("3DS full keyframe length disagrees with its state length"));
                    }
                    let state = Arc::new(state);
                    let payload = Arc::new(Vec::new());
                    for m in 0..3 {
                        self.levels[m] = Some(LevelState { position, state: state.clone(), payload: payload.clone() });
                    }
                }
                else {
                    if !predecessor_held {
                        return Err(broken("3DS delta keyframe's reference keyframe is not in its chain"));
                    }
                    let reference = self.levels[level].clone().ok_or_else(|| broken("3DS delta keyframe has no reference keyframe"))?;
                    let payload = decompress_data_with_prefix(frame, uncompressed, &reference.payload).map_err(decode_error)?;
                    let (control_len, rest) = payload.split_at_checked(8).ok_or_else(|| broken("3DS delta payload is too short"))?;
                    let control_len = usize::try_from(u64::from_le_bytes(control_len.try_into().expect("8 bytes"))).map_err(|_| broken("3DS delta control length does not fit"))?;
                    let (control, data) = rest.split_at_checked(control_len).ok_or_else(|| broken("3DS delta control stream is truncated"))?;
                    let mut state: Vec<u8> = (*reference.state).clone();
                    if wanted > state.len().saturating_mul(2).max(1 << 20) {
                        return Err(broken("3DS delta keyframe state length is implausible"));
                    }
                    state.resize(wanted, 0);
                    if !apply_region_diff_in_place(state.as_mut_slice(), control, data) {
                        return Err(broken("3DS delta keyframe failed to apply"));
                    }
                    let state = Arc::new(state);
                    let payload = Arc::new(payload);
                    for m in level..3 {
                        self.levels[m] = Some(LevelState { position, state: state.clone(), payload: payload.clone() });
                    }
                }
                self.stored_state = self.levels[2].as_ref().map(|l| l.state.clone());
                self.list = Some(list);
                self.last_applied = Some(position);
                self.folds += 1;
            },

            Packet::DeltaKeyframe { diff, .. } => {
                if !predecessor_held {
                    return Err(ReplayFileReadError::BrokenPacket { explanation: Cow::Borrowed("delta keyframe is not preceded by a keyframe in its chain") });
                }
                if !apply_diff_in_place(&mut self.state, diff.as_slice()) {
                    return Err(ReplayFileReadError::BrokenPacket { explanation: Cow::Borrowed("delta keyframe failed to apply") });
                }
                self.last_applied = Some(position);
                self.folds += 1;
            },

            Packet::RegionDeltaKeyframe { state_len, control, data, .. } => {
                if !predecessor_held {
                    return Err(ReplayFileReadError::BrokenPacket { explanation: Cow::Borrowed("delta keyframe is not preceded by a keyframe in its chain") });
                }
                // A 3DS keyframe may be a few bytes longer or shorter than the one before it
                // (see `region_diff_resizing`); the chain follows the keyframe's length. A
                // fixed-size console never takes this path, so its files decode as before.
                let wanted = usize::try_from(*state_len).map_err(|_| ReplayFileReadError::BrokenPacket { explanation: Cow::Owned(format!("region delta keyframe state length {state_len} does not fit")) })?;
                if wanted != self.state.len() {
                    if !self.resizing || wanted > self.state.len().saturating_mul(2).max(1 << 20) {
                        return Err(ReplayFileReadError::BrokenPacket { explanation: Cow::Owned(format!("region delta keyframe expects a {state_len}-byte state but the chain holds {} bytes", self.state.len())) });
                    }
                    self.state.resize(wanted, 0);
                }
                if !apply_region_diff_in_place(self.state.as_mut_slice(), control.as_slice(), data.as_slice()) {
                    return Err(ReplayFileReadError::BrokenPacket { explanation: Cow::Borrowed("region delta keyframe is malformed") });
                }
                self.last_applied = Some(position);
                self.folds += 1;
            },

            _ => {}
        }

        Ok(())
    }
}

/// How many compressed bytes an on-demand decode feeds to zstd per step. Blobs decompress about
/// 8-10x, so a step yields a few MiB, which bounds how far past a seek target a partial decode
/// runs.
const DECODE_STEP_COMPRESSED_BYTES: usize = 256 * 1024;

/// A blob's decompressed bytes, decoded only as far as a reader has needed so far.
///
/// Packets are parsed out of `raw` on demand: the cursor, and a seek's walk along the keyframe
/// chain, never build a packet list for the whole blob. A blob with a keyframe offset table
/// (format v6) can therefore be entered at any keyframe having decoded only the bytes before it;
/// a blob without one is decoded in full the first time its keyframes are needed and scanned once
/// to build the same table.
pub(crate) struct DecodedBlob {
    /// Decompressed bytes so far (`len()` grows as decoding proceeds, never beyond `size`).
    raw: Vec<u8>,
    /// The blob's full decompressed size.
    size: usize,
    /// Decoder for the rest of the blob; `None` once `raw` is complete.
    decoder: Option<BlobDecoder>,
    /// Byte offset in `raw` of every keyframe-class packet, ascending; `None` until scanned (only
    /// a blob written without a table needs scanning).
    keyframe_offsets: Option<Vec<usize>>,
}

impl DecodedBlob {
    /// A blob with nothing decoded yet. `table` is the blob packet's own offset table, already
    /// validated by [`validate_offset_table`] (an invalid one is passed as `None` and rebuilt by
    /// scanning).
    fn begin(compressed: &[u8], size: usize, table: Option<Vec<usize>>) -> Result<Self, ReplayFileReadError> {
        let decoder = BlobDecoder::new(compressed, size)
            .map_err(|e| ReplayFileReadError::Other { explanation: Cow::Owned(format!("Decompression error: {e}")) })?;
        let mut raw = Vec::new();
        if raw.try_reserve_exact(size).is_err() {
            return Err(ReplayFileReadError::Other { explanation: Cow::Borrowed("failed to allocate RAM to decompress compressed blob") });
        }
        Ok(Self { raw, size, decoder: Some(decoder), keyframe_offsets: table })
    }

    /// A blob decoded in full (used by background workers and whole-blob readers).
    fn complete(header: &ReplayHeaderRaw, compressed: &[u8], size: usize, table: Option<Vec<usize>>) -> Result<Self, ReplayFileReadError> {
        let raw = decompress_data(compressed, size)
            .map_err(|e| ReplayFileReadError::Other { explanation: Cow::Owned(format!("Decompression error: {e}")) })?;
        let mut blob = Self { raw, size, decoder: None, keyframe_offsets: table };
        blob.offsets(compressed, header.replay_version)?;
        Ok(blob)
    }

    fn is_complete(&self) -> bool {
        self.raw.len() >= self.size
    }

    /// Decode until at least `upto` bytes (clamped to the blob size) are available.
    fn ensure_decoded(&mut self, compressed: &[u8], upto: usize) -> Result<(), ReplayFileReadError> {
        let upto = upto.min(self.size);
        while self.raw.len() < upto {
            let Some(decoder) = self.decoder.as_mut() else {
                return Err(ReplayFileReadError::BrokenPacket { explanation: Cow::Owned(format!("compressed blob decompressed to {} bytes but claims {}", self.raw.len(), self.size)) });
            };
            let finished = decoder.step(compressed, &mut self.raw, self.size, DECODE_STEP_COMPRESSED_BYTES)
                .map_err(|e| ReplayFileReadError::Other { explanation: Cow::Owned(format!("Decompression error: {e}")) })?;
            if finished {
                self.decoder = None;
                if self.raw.len() != self.size {
                    return Err(ReplayFileReadError::BrokenPacket { explanation: Cow::Owned(format!("compressed blob decompressed to {} bytes but claims {}", self.raw.len(), self.size)) });
                }
            }
        }
        Ok(())
    }

    fn ensure_complete(&mut self, compressed: &[u8]) -> Result<(), ReplayFileReadError> {
        self.ensure_decoded(compressed, self.size)
    }

    /// The keyframe offset table, scanning the (fully decoded) blob for it if it has none.
    fn offsets(&mut self, compressed: &[u8], version: u32) -> Result<&[usize], ReplayFileReadError> {
        if self.keyframe_offsets.is_none() {
            self.ensure_complete(compressed)?;
            let mut offsets = Vec::new();
            let mut b = self.raw.as_slice();
            while !b.is_empty() {
                let offset = self.size - b.len();
                let packet = Packet::read_all(&mut b, version)
                    .map_err(|e| ReplayFileReadError::BrokenPacket { explanation: Cow::Owned(format!("Failed to read packet - {e:?}")) })?;
                if is_keyframe_class(&packet) {
                    offsets.push(offset);
                }
            }
            self.keyframe_offsets = Some(offsets);
        }
        self.ensure_decoded(compressed, 1)?;
        let offsets = self.keyframe_offsets.as_deref().expect("just scanned");
        if offsets.first() != Some(&0) || self.raw.first() != Some(&(PacketDiscriminator::Keyframe as u8)) {
            return Err(ReplayFileReadError::InvalidReplayFile { explanation: Cow::Borrowed("first packet in a blob was not a keyframe") });
        }
        Ok(offsets)
    }

    /// Parse the packet at byte `offset`, decoding more of the blob as needed. Returns the packet
    /// and the offset of the one after it.
    fn parse_at(&mut self, compressed: &[u8], version: u32, offset: usize) -> Result<(Packet, usize), ReplayFileReadError> {
        if offset >= self.size {
            return Err(ReplayFileReadError::BrokenPacket { explanation: Cow::Borrowed("packet offset is past the end of its blob") });
        }
        loop {
            self.ensure_decoded(compressed, offset + 1)?;
            let mut b = &self.raw[offset..];
            match Packet::read_all(&mut b, version) {
                Ok(packet) => return Ok((packet, self.raw.len() - b.len())),
                // Anything can fail on a packet cut short by the decode frontier (a length prefix
                // read as garbage included), so only a complete blob makes an error final.
                Err(_) if !self.is_complete() => {
                    let decoded = self.raw.len();
                    self.ensure_decoded(compressed, decoded + 1)?;
                },
                Err(e) => return Err(ReplayFileReadError::BrokenPacket { explanation: Cow::Owned(format!("Failed to read packet - {e:?}")) }),
            }
        }
    }

    /// Whether the packet at `offset` is a full [`Packet::Keyframe`] (by its discriminator byte;
    /// decodes up to it if needed).
    fn is_full_keyframe_at(&mut self, compressed: &[u8], offset: usize) -> Result<bool, ReplayFileReadError> {
        self.ensure_decoded(compressed, offset + 1)?;
        Ok(self.raw.get(offset) == Some(&(PacketDiscriminator::Keyframe as u8)))
    }

    /// Whether a keyframe-class packet lies strictly between byte offsets `a` and `b`. Requires
    /// the offset table (see [`Self::offsets`]).
    fn keyframe_between(&self, a: usize, b: usize) -> bool {
        let offsets = self.keyframe_offsets.as_deref().expect("offset table needed before folding");
        let first_after_a = offsets.partition_point(|&o| o <= a);
        offsets.get(first_after_a).is_some_and(|&o| o < b)
    }
}

/// A blob packet's own keyframe offset table as `usize`s, if it is usable: one entry per keyframe,
/// starting at 0, strictly ascending and inside the blob. Anything else is treated as absent (the
/// player then scans the blob, which is slower but never wrong).
fn validate_offset_table(offsets: &[UnsignedInteger], keyframes: usize, size: UnsignedInteger) -> Option<Vec<usize>> {
    if offsets.is_empty() || offsets.len() != keyframes || offsets[0] != 0 {
        return None;
    }
    let mut table = Vec::with_capacity(offsets.len());
    let mut previous = None;
    for &offset in offsets {
        if offset >= size || previous.is_some_and(|p| offset <= p) {
            return None;
        }
        previous = Some(offset);
        table.push(usize::try_from(offset).ok()?);
    }
    Some(table)
}

/// The usable offset table of a blob packet (see [`validate_offset_table`]).
fn packet_offset_table(packet: &Packet) -> Option<Vec<usize>> {
    match packet {
        Packet::CompressedBlob { keyframe_offsets, keyframes, uncompressed_size, .. } => validate_offset_table(keyframe_offsets, keyframes.len(), *uncompressed_size),
        _ => None
    }
}

impl ReplayFilePlayer {
    /// Try to read a buffer.
    ///
    /// If `allow_some_corruption`, then the parser will break early if it detects a corrupted
    /// packet and there is still some sort of usable stream. Otherwise, it will return `Err`.
    pub fn new<B: AsRef<[u8]>>(data: B, allow_some_corruption: bool) -> Result<ReplayFilePlayer, ReplayFileReadError> {
        // A 3DS file's keyframes are read from the file on demand, so its bytes must outlive
        // the parse; this constructor cannot keep a borrowed `data`, so it copies them (fine for
        // tests and small files; the app opens 3DS files with [`Self::new_shared`]).
        if data.as_ref().get(0x8..0xC).is_some_and(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")) == ReplayConsoleType::Nintendo3DS as u32) {
            let shared: Arc<dyn AsRef<[u8]> + Send + Sync> = Arc::new(data.as_ref().to_vec());
            return Self::new_shared(shared, allow_some_corruption);
        }
        Self::build(data.as_ref(), None, allow_some_corruption)
    }

    /// [`Self::new`] on bytes the player keeps (a memory-mapped file, say): what a 3DS file needs,
    /// since its keyframes stay in the file and are read as they are seeked to.
    pub fn new_shared(source: Arc<dyn AsRef<[u8]> + Send + Sync>, allow_some_corruption: bool) -> Result<ReplayFilePlayer, ReplayFileReadError> {
        let bytes: &[u8] = (*source).as_ref();
        // SAFETY: `source` is an `Arc` that `build` stores in the player, so the bytes outlive
        // every use made of this borrow during construction.
        let bytes: &[u8] = unsafe { core::slice::from_raw_parts(bytes.as_ptr(), bytes.len()) };
        Self::build(bytes, Some(source), allow_some_corruption)
    }

    fn build(buffer_bytes: &[u8], source: Option<Arc<dyn AsRef<[u8]> + Send + Sync>>, allow_some_corruption: bool) -> Result<ReplayFilePlayer, ReplayFileReadError> {
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

        let stream_len = replay_data.len();
        while !replay_data.is_empty() {
            let before = replay_data.len();
            match Packet::read_all(&mut replay_data, header_raw.replay_version) {
                Ok(mut n) => {
                    if let Packet::StoredKeyframe { frame_len, frame_offset, .. } = &mut n {
                        // The frame is the tail of what this packet consumed.
                        let consumed = before - replay_data.len();
                        *frame_offset = (patch_end + (stream_len - before) + consumed - *frame_len as usize) as UnsignedInteger;
                    }
                    all_packets.push(n)
                },
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
        // blob is decoded (a blob always starts with a full keyframe).
        let mut have_top_level_keyframe = false;
        for (packet_index, packet) in all_packets.iter().enumerate() {
            match packet {
                Packet::Keyframe { .. } | Packet::StoredKeyframe { level: 0, .. } => have_top_level_keyframe = true,
                Packet::CompressedBlob { .. } => have_top_level_keyframe = false,
                Packet::DeltaKeyframe { .. } | Packet::RegionDeltaKeyframe { .. } | Packet::StoredKeyframe { .. } if !have_top_level_keyframe => {
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
            Packet::Keyframe { .. } | Packet::StoredKeyframe { level: 0, .. } => {},
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

        let mut thumbnails = Vec::new();
        #[cfg(feature = "std")]
        let mut compressed_blobs_decompressing = BTreeMap::new();
        let mut decoded_blobs = BTreeMap::new();
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
                    decoded_blobs.insert(packet_index, None);
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
                | Packet::RegionDeltaKeyframe { metadata, .. }
                | Packet::StoredKeyframe { metadata, .. } => {
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
                Packet::Thumbnail { elapsed_frames, .. } => {
                    thumbnails.push((*elapsed_frames, packet_index));
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
            chain: ChainState::new(replay_file_metadata.console_type == ReplayConsoleType::Nintendo3DS),
            source,
            materialise_next: false,
            thumbnails,
            replay_file_metadata,
            keyframes: unsafe { transmute::<KeyframeMap, KeyframeMap<'static>>(all_keyframes) },
            legacy_bookmarks: unsafe { transmute::<BookmarkMap, BookmarkMap<'static>>(all_bookmarks) },
            bookmark_table,
            bookmark_table_source,
            bookmark_section_error,
            stream_truncated,
            all_uncompressed_packets: all_packets,
            next_uncompressed_packet_index: 0usize,
            next_blob_offset: None,
            compressed_blob_uncompressed_packet_indices: compressed_blob_indices,
            #[cfg(feature = "std")]
            compressed_blobs_decompressing,
            decoded_blobs,
            total_frame_count,
            total_millis: TimestampMillis(total_millis),
            cleanup_enabled: true,
            header_raw: *header_raw,
            scratch: None,
            keyframe_states_wanted: true,
            keyframe_transients_stale: false,

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
        self.chain.current()
    }

    /// Whether the keyframe-class packet most recently returned by [`Self::next_packet`] left
    /// the console's transient ranges ([`transient_ranges`]) as they were in the keyframe before
    /// it, which is what every delta keyframe of a replay recorded with
    /// `mask_transient_buffers` does: its regenerated output buffers are then a copy from the
    /// chain's restart keyframe, not the game's output at that moment, and an emulator loading
    /// the state must not show them (see `EmulatorCore::load_save_state_with_stale_output` in
    /// supershuckie-core). `false` for full keyframes and before the first keyframe.
    ///
    /// Judged from the delta itself, so a lossless file's deltas (which rewrite those buffers
    /// every keyframe the game draws) are not affected.
    pub fn current_keyframe_has_stale_transients(&self) -> bool {
        self.keyframe_transients_stale
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
    /// A seek into a blob decodes it only as far as the keyframe (given a v6 offset table) and
    /// parses only the keyframe packets on the way there.
    ///
    /// On failure, `Err` is returned.
    pub fn go_to_keyframe(&mut self, keyframe_frames_index: UnsignedInteger) -> Result<(), ReplaySeekError> {
        let (list, target) = self.locate_keyframe(keyframe_frames_index)?;

        self.fast_forward_chain(list, target).map_err(|error| ReplaySeekError::ReadError { error })?;
        self.materialise_next = true;

        match list {
            ListId::TopLevel => {
                self.next_uncompressed_packet_index = target;
                self.next_blob_offset = None;
            },
            ListId::Blob(top_index) => {
                self.next_uncompressed_packet_index = top_index;
                self.next_blob_offset = Some(target);
            }
        }

        Ok(())
    }

    /// Whether the keyframe [`Self::go_to_keyframe`] would use for this frame is stored as a full
    /// keyframe (so seeking to it applies no deltas). Decodes its blob up to it if needed; does not
    /// move the cursor.
    pub fn keyframe_is_stored_full(&mut self, keyframe_frames_index: UnsignedInteger) -> Result<bool, ReplaySeekError> {
        let (list, target) = self.locate_keyframe(keyframe_frames_index)?;
        match list {
            ListId::TopLevel => Ok(matches!(self.all_uncompressed_packets[target], Packet::Keyframe { .. } | Packet::StoredKeyframe { level: 0, .. })),
            ListId::Blob(top_index) => {
                let packets = self.all_uncompressed_packets.clone();
                let compressed = blob_compressed_data(&packets, top_index);
                self.blob_mut(top_index).is_full_keyframe_at(compressed, target).map_err(|error| ReplaySeekError::ReadError { error })
            }
        }
    }

    /// Find the list and position of the last keyframe-class packet on the given frame.
    fn locate_keyframe(&mut self, keyframe_frames_index: UnsignedInteger) -> Result<(ListId, usize), ReplaySeekError> {
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
                    if let Some(i) = keyframes.iter().rposition(|k| k.elapsed_frames == keyframe_frames_index) {
                        location = Some((ListId::Blob(top_index), i));
                    }
                    else if location.is_some() {
                        break;
                    }
                },
                p if matches_frame(p) => {
                    location = Some((ListId::TopLevel, top_index));
                },
                Packet::NextFrame { .. } if location.is_some() => break,
                _ => continue
            }
        }

        let Some((list, index)) = location else {
            return Err(ReplaySeekError::ReadError { error: ReplayFileReadError::Other { explanation: Cow::Borrowed("keyframe is in the index but not in the packet stream") } })
        };

        match list {
            ListId::TopLevel => Ok((list, index)),
            ListId::Blob(top_index) => {
                self.decompress_immediately(top_index).map_err(|error| ReplaySeekError::ReadError { error })?;

                let packets = self.all_uncompressed_packets.clone();
                let compressed = blob_compressed_data(&packets, top_index);
                let version = self.header_raw.replay_version;
                let offsets = self.blob_mut(top_index).offsets(compressed, version).map_err(|error| ReplaySeekError::ReadError { error })?;

                // `index` counts keyframes in the blob's metadata list, which the recorder writes in
                // step with the packets; a table that disagrees is caught when the packet is parsed.
                let Some(&target) = offsets.get(index) else {
                    return Err(ReplaySeekError::ReadError { error: ReplayFileReadError::BrokenPacket { explanation: Cow::Borrowed("keyframe listed in the blob metadata is not in the blob") } })
                };

                Ok((list, target))
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
    fn fast_forward_chain(&mut self, list: ListId, target: usize) -> Result<(), ReplayFileReadError> {
        match list {
            ListId::TopLevel => {
                let packets = self.all_uncompressed_packets.clone();
                if matches!(packets[target], Packet::StoredKeyframe { .. }) {
                    return self.fast_forward_stored_chain(&packets, target);
                }
                let Some(restart) = packets[..=target].iter().rposition(|p| matches!(p, Packet::Keyframe { .. })) else {
                    return Err(ReplayFileReadError::BrokenPacket { explanation: Cow::Borrowed("delta keyframe with no full keyframe before it") })
                };

                let start = match self.chain.last_applied {
                    Some(last) if self.chain.list == Some(list) && last <= target => (last + 1).max(restart),
                    _ => restart
                };

                for index in start..target {
                    self.fold_top_level(&packets, index)?;
                }
            },

            ListId::Blob(top_index) => {
                let packets = self.all_uncompressed_packets.clone();
                let compressed = blob_compressed_data(&packets, top_index);
                let version = self.header_raw.replay_version;

                // The keyframes at or before the target, oldest first.
                let offsets: Vec<usize> = {
                    let blob = self.blob_mut(top_index);
                    let offsets = blob.offsets(compressed, version)?;
                    let end = offsets.partition_point(|&o| o <= target);
                    offsets[..end].to_vec()
                };
                if offsets.last() != Some(&target) {
                    return Err(ReplayFileReadError::BrokenPacket { explanation: Cow::Borrowed("seek target is not a keyframe of its blob") });
                }

                // Restart from the last full keyframe at or before the target (read by discriminator
                // byte, which decodes the blob up to there).
                let mut restart = None;
                for (i, &offset) in offsets.iter().enumerate().rev() {
                    if self.blob_mut(top_index).is_full_keyframe_at(compressed, offset)? {
                        restart = Some(i);
                        break;
                    }
                }
                let Some(restart) = restart else {
                    return Err(ReplayFileReadError::BrokenPacket { explanation: Cow::Borrowed("delta keyframe with no full keyframe before it") })
                };

                let start = match self.chain.last_applied {
                    Some(last) if self.chain.list == Some(list) && last <= target => offsets.partition_point(|&o| o <= last).max(restart),
                    _ => restart
                };

                let end = offsets.len() - 1;
                for &offset in &offsets[start.min(end)..end] {
                    self.fold_in_blob(top_index, compressed, offset)?;
                }
            }
        }

        Ok(())
    }

    /// The 3DS chain to the keyframe just before `target`: the last level-0 keyframe at or
    /// before it, the level-1 keyframes after that, then the level-2 keyframes after the last of
    /// those. Whatever the chain already holds of that walk is kept (a level's state is
    /// current for the walk when the level below it is at the walk's own start).
    fn fast_forward_stored_chain(&mut self, packets: &[Packet], target: usize) -> Result<(), ReplayFileReadError> {
        let list = ListId::TopLevel;
        let level_of = |i: usize| match &packets[i] { Packet::StoredKeyframe { level, .. } => Some(*level), _ => None };
        let Some(restart) = (0..=target).rev().find(|&i| level_of(i) == Some(0)) else {
            return Err(ReplayFileReadError::BrokenPacket { explanation: Cow::Borrowed("3DS delta keyframe with no full keyframe before it") })
        };
        let level1: Vec<usize> = (restart + 1..target).filter(|&i| level_of(i) == Some(1)).collect();
        let last_level1 = level1.last().copied().unwrap_or(restart);
        let level2: Vec<usize> = (last_level1 + 1..target).filter(|&i| level_of(i) == Some(2)).collect();

        let held_at = |chain: &ChainState, level: usize, position: usize| chain.list == Some(list) && chain.levels[level].as_ref().is_some_and(|l| l.position == position);
        if !held_at(&self.chain, 0, restart) {
            self.fold_top_level(packets, restart)?;
        }
        // Walking a level's chain from its start (a backward seek, or a segment the chain never
        // walked) begins from the state of the level below: that is what its first delta
        // references.
        let start1 = level1.iter().rposition(|&p| held_at(&self.chain, 1, p)).map_or(0, |i| i + 1);
        if start1 == 0 {
            self.chain.levels[1] = self.chain.levels[0].clone();
        }
        for &p in &level1[start1..] {
            self.fold_top_level(packets, p)?;
        }
        let start2 = if held_at(&self.chain, 1, last_level1) {
            level2.iter().rposition(|&p| held_at(&self.chain, 2, p)).map_or(0, |i| i + 1)
        } else {
            0
        };
        if start2 == 0 {
            self.chain.levels[2] = self.chain.levels[1].clone();
        }
        for &p in &level2[start2..] {
            self.fold_top_level(packets, p)?;
        }
        Ok(())
    }

    /// Fold the top-level packet at `index` into the chain.
    fn fold_top_level(&mut self, packets: &[Packet], index: usize) -> Result<(), ReplayFileReadError> {
        let list = ListId::TopLevel;
        let packet = &packets[index];
        if !is_keyframe_class(packet) || self.chain.is_at(list, index) {
            // Nothing to fold, or a repeated seek to the same delta finds it already folded.
            return Ok(());
        }
        let held = match packet {
            // A 3DS delta's reference is the most recent keyframe of a level at most its own.
            Packet::StoredKeyframe { level, .. } if *level > 0 => {
                let reference = (0..index).rev().find(|&i| matches!(&packets[i], Packet::StoredKeyframe { level: l, .. } if l <= level));
                self.chain.list == Some(list) && self.chain.levels[usize::from(*level)].as_ref().is_some_and(|l| Some(l.position) == reference)
            },
            _ => self.chain.holds_predecessor_of(list, index, |a, b| packets[a + 1..b].iter().any(is_keyframe_class))
        };
        let source = self.source.clone();
        self.chain.fold(list, index, packet, held, source.as_deref().map(|s| s.as_ref()))
    }

    /// Parse the packet at byte `offset` of the blob at `top_index`, fold it into the chain if it
    /// is a keyframe, and return it with the offset of the packet after it.
    fn fold_in_blob(&mut self, top_index: usize, compressed: &[u8], offset: usize) -> Result<(Packet, usize), ReplayFileReadError> {
        let list = ListId::Blob(top_index);
        let version = self.header_raw.replay_version;
        let blob = self.decoded_blobs.get_mut(&top_index).expect("blob index").as_mut().expect("blob decoded");
        let (packet, next) = blob.parse_at(compressed, version, offset)?;

        if is_keyframe_class(&packet) {
            // The predecessor check below consults the offset table (a table-less blob builds it
            // by scanning, which decodes the rest of the blob first).
            blob.offsets(compressed, version)?;
            // A repeated seek to the same delta finds it already folded.
            if !self.chain.is_at(list, offset) {
                let held = self.chain.holds_predecessor_of(list, offset, |a, b| blob.keyframe_between(a, b));
                self.chain.fold(list, offset, &packet, held, None)?;
            }
        }

        Ok((packet, next))
    }

    fn blob_mut(&mut self, top_index: usize) -> &mut DecodedBlob {
        self.decoded_blobs.get_mut(&top_index).expect("blob index").as_mut().expect("blob decoded")
    }

    /// Ensure the blob at `blob_packet_index` has a [`DecodedBlob`] to read from, using (and, if
    /// necessary, waiting for) a background worker if one is in flight for it; otherwise the blob
    /// starts undecoded and is decoded on demand.
    ///
    /// Never spins: a worker still running is waited for with a blocking `recv()`, and a worker
    /// that died before sending (see [`DecompressionWorker`]) is reaped and the blob is
    /// decoded here instead.
    #[cfg(feature = "std")]
    fn decompress_immediately(&mut self, blob_packet_index: usize) -> Result<(), ReplayFileReadError> {
        if self.decoded_blobs[&blob_packet_index].is_some() {
            return Ok(())
        }

        let worker = self.compressed_blobs_decompressing
            .get_mut(&blob_packet_index)
            .expect("compressed blob should be in working cache")
            .take();

        if let Some(worker) = worker {
            match worker.result.recv() {
                Ok(Ok(blob)) => {
                    *self.decoded_blobs
                        .get_mut(&blob_packet_index)
                        .expect("compressed blob should be in finished cache") = Some(blob);
                    return Ok(());
                }
                Ok(Err(error)) => return Err(error),
                Err(std::sync::mpsc::RecvError) => {
                    // The worker died before sending (an early exit; a panic would abort the whole
                    // process under this app's release profile, but not under an unwinding build
                    // such as `cargo test`). Reap the thread and fall back to decoding here.
                    let _ = worker.handle.join();
                }
            }
        }

        self.begin_decoding(blob_packet_index)
    }

    /// [`Self::decompress_immediately`] without a background worker: the `no_std` configuration
    /// never spawns a thread, so this is the only path.
    #[cfg(not(feature = "std"))]
    fn decompress_immediately(&mut self, blob_packet_index: usize) -> Result<(), ReplayFileReadError> {
        if self.decoded_blobs[&blob_packet_index].is_some() {
            return Ok(())
        }

        self.begin_decoding(blob_packet_index)
    }

    /// Set up on-demand decoding of the blob at `blob_packet_index` (nothing is decoded yet).
    /// Assumes it is not already decoded.
    fn begin_decoding(&mut self, blob_packet_index: usize) -> Result<(), ReplayFileReadError> {
        let packet = &self.all_uncompressed_packets[blob_packet_index];
        let Packet::CompressedBlob { compressed_data, uncompressed_size, .. } = packet else {
            panic!("begin_decoding on {blob_packet_index} failed because it's not a compressed blob packet...")
        };

        let blob = DecodedBlob::begin(
            compressed_data.as_slice(),
            usize::try_from(*uncompressed_size).expect("we checked uncompressed size converting earlier"),
            packet_offset_table(packet)
        )?;

        *self.decoded_blobs
            .get_mut(&blob_packet_index)
            .expect("compressed blob should be in finished cache") = Some(blob);

        Ok(())
    }

    /// Whether the file carries timeline pictures (Nintendo 3DS files do).
    pub fn has_thumbnails(&self) -> bool {
        !self.thumbnails.is_empty()
    }

    /// The timeline picture nearest at or before `frame`, decoded.
    pub fn thumbnail_at_or_before(&self, frame: UnsignedInteger) -> Option<ReplayThumbnail> {
        let at = self.thumbnails.partition_point(|&(f, _)| f <= frame).checked_sub(1)?;
        let (thumb_frame, index) = self.thumbnails[at];
        let Packet::Thumbnail { top_width, top_height, bottom_width, bottom_height, top, bottom, .. } = &self.all_uncompressed_packets[index] else {
            return None;
        };
        let decode = |w: UnsignedInteger, h: UnsignedInteger, data: &ByteVec| -> Option<(u32, u32, Vec<u8>)> {
            let (w, h) = (u32::try_from(w).ok()?, u32::try_from(h).ok()?);
            let len = usize::try_from(u64::from(w).checked_mul(u64::from(h))?.checked_mul(2)?).ok()?;
            if len == 0 || len > 16 << 20 {
                return None;
            }
            Some((w, h, crate::util::decompress_data(data.as_slice(), len).ok()?))
        };
        Some(ReplayThumbnail {
            frame: thumb_frame,
            top: decode(*top_width, *top_height, top)?,
            bottom: decode(*bottom_width, *bottom_height, bottom)?
        })
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
                if matches!(packets[packet_index], Packet::StoredKeyframe { .. }) && !self.keyframe_states_wanted && !self.materialise_next {
                    // Sequential playback past a 3DS keyframe: not materialised (see
                    // `materialise_next`); handed out with an empty state.
                    let metadata = keyframe_metadata(&packets[packet_index]).expect("stored keyframe").clone();
                    self.scratch = Some(Packet::Keyframe { metadata, state: ByteVec::new() });
                    self.keyframe_transients_stale = false;
                    return Ok(self.scratch.as_ref());
                }
                self.materialise_next = false;
                self.fold_top_level(&packets, packet_index)?;
                return self.hand_out(ListId::TopLevel, packet_index, None, &packets).map(Some);
            }

            self.decompress_immediately(packet_index)?;

            let packets = self.all_uncompressed_packets.clone();
            let compressed = blob_compressed_data(&packets, packet_index);
            let offset = self.next_blob_offset.unwrap_or(0);
            if offset >= self.blob_mut(packet_index).size {
                self.next_blob_offset = None;
                self.next_uncompressed_packet_index += 1;
                continue;
            }

            let (packet, next) = self.fold_in_blob(packet_index, compressed, offset)?;
            self.next_blob_offset = Some(next);
            return self.hand_out(ListId::Blob(packet_index), offset, Some(packet), &packets).map(Some);
        }
    }

    /// What the caller should see for the packet at `position` of `list`, which has just been
    /// folded into the chain: the packet itself, or a materialised [`Packet::Keyframe`] for a
    /// delta. `owned` is the packet when it was parsed out of a blob; a top-level packet is
    /// referenced in place.
    fn hand_out(&mut self, list: ListId, position: usize, owned: Option<Packet>, packets: &Arc<Vec<Packet>>) -> Result<&Packet, ReplayFileReadError> {
        let is_delta = match owned.as_ref() {
            Some(packet) => matches!(packet, Packet::DeltaKeyframe { .. } | Packet::RegionDeltaKeyframe { .. } | Packet::StoredKeyframe { .. }),
            None => matches!(packets[position], Packet::DeltaKeyframe { .. } | Packet::RegionDeltaKeyframe { .. } | Packet::StoredKeyframe { .. }),
        };

        // (`position` indexes `packets` only for a top-level packet; in a blob it is a byte offset.)
        let packet = match owned.as_ref() {
            Some(packet) => packet,
            None => &packets[position]
        };
        if is_keyframe_class(packet) {
            self.keyframe_transients_stale = match packet {
                Packet::RegionDeltaKeyframe { control, .. } => {
                    let ranges = transient_ranges(self.replay_file_metadata.console_type, self.chain.current());
                    !ranges.is_empty() && ranges.iter().all(|r| !region_diff_touches(control.as_slice(), r.start, r.end))
                },
                _ => false
            };
        }

        if is_delta {
            debug_assert!(self.chain.is_at(list, position), "delta handed out without being folded");
            let metadata = match owned {
                Some(Packet::DeltaKeyframe { metadata, .. }) | Some(Packet::RegionDeltaKeyframe { metadata, .. }) | Some(Packet::StoredKeyframe { metadata, .. }) => metadata,
                _ => keyframe_metadata(&packets[position]).expect("checked above").clone()
            };
            let state = if self.keyframe_states_wanted {
                ByteVec::Heap(self.chain.current().to_vec())
            }
            else {
                ByteVec::new()
            };
            self.scratch = Some(Packet::Keyframe { metadata, state });
            return Ok(self.scratch.as_ref().expect("just set"));
        }

        match owned {
            Some(packet) => {
                self.scratch = Some(packet);
                Ok(self.scratch.as_ref().expect("just set"))
            },
            // SAFETY: `packets` is the top-level list, which is owned by `self` and never mutated
            // after construction. The returned reference is bound to the `&mut self` borrow.
            None => Ok(unsafe { launder_reference(&packets[position]) })
        }
    }

    /// Decompress all blobs.
    ///
    /// Decoded blobs hold their compact packet bytes (about the compressed size of the file times
    /// the compression ratio, not the sum of all keyframe states), so this costs roughly the
    /// decompressed size of every blob.
    pub fn decompress_all_blobs(&mut self) {
        self.cleanup_enabled = false;

        let packets = self.all_uncompressed_packets.clone();
        for (index, packet) in packets.iter().enumerate() {
            if let Packet::CompressedBlob { compressed_data, .. } = packet {
                if self.decompress_immediately(index).is_ok() {
                    let _ = self.blob_mut(index).ensure_complete(compressed_data.as_slice());
                }
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
                    if let Some(slot) = self.decoded_blobs.get_mut(&i) {
                        *slot = None;
                    }
                    #[cfg(feature = "std")]
                    if let Some(slot) = self.compressed_blobs_decompressing.get_mut(&i) {
                        *slot = None;
                    }
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

        if self.decoded_blobs[&blob_index].is_some() {
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
                    let packet = packets.get(blob_index).expect("failed to get packet");
                    let Packet::CompressedBlob { uncompressed_size, compressed_data, .. } = packet else {
                        panic!("compressed blob wasn't a compressed blob NOOOOO")
                    };
                    let result = DecodedBlob::complete(
                        &header,
                        compressed_data.as_slice(),
                        usize::try_from(*uncompressed_size).expect("we checked this could be a usize!"),
                        packet_offset_table(packet)
                    );
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
            Ok(Ok(blob)) => {
                self.decoded_blobs.insert(blob_index, Some(blob));
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

/// The compressed bytes of the blob at `top_index` of `packets`.
fn blob_compressed_data(packets: &[Packet], top_index: usize) -> &[u8] {
    match &packets[top_index] {
        Packet::CompressedBlob { compressed_data, .. } => compressed_data.as_slice(),
        _ => panic!("packet {top_index} is not a compressed blob")
    }
}

/// A background decompression of one compressed blob, started by
/// [`ReplayFilePlayer::enable_threading`].
///
/// `decompress_immediately` never spins on this: it blocks on [`Self::result`] (a blocking
/// `recv()`) rather than polling, and a worker that dies before sending (`RecvError`/
/// `TryRecvError::Disconnected`) is reaped with `handle.join()` and the blob is decoded on
/// the calling thread instead, so a background failure can never wedge the caller at 100% CPU or
/// forever.
///
/// Note: under this app's release profile (`panic = "abort"`), a panic in the worker thread
/// aborts the whole process like any other panic, so the fallback below cannot occur from a panic
/// in a release build; it matters for unwinding builds (`cargo test`, a debug build) and for a
/// worker that exits early without panicking.
#[cfg(feature = "std")]
pub(crate) struct DecompressionWorker {
    result: std::sync::mpsc::Receiver<Result<DecodedBlob, ReplayFileReadError>>,
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
                let raw = decompress_data(compressed_data.as_slice(), usize::try_from(*uncompressed_size).ok()?).ok()?;
                let mut newest = None;
                let mut b = raw.as_slice();
                while !b.is_empty() {
                    if let Packet::BookmarkTable { table } = Packet::read_all(&mut b, header.replay_version).ok()? {
                        newest = Some(table);
                    }
                }
                return newest;
            },
            _ => {}
        }
    }
    None
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

    /// The packets of a small chain: a full keyframe, then region deltas, with a frame between
    /// each (states are `size` bytes so a blob is many decode steps long when `size` is large).
    fn chain_packets(keyframes: u64, size: usize) -> (Vec<Packet>, Vec<Vec<u8>>) {
        let mut states = Vec::new();
        let mut packets = Vec::new();
        let mut previous: Option<Vec<u8>> = None;
        for frame in 0..keyframes {
            let mut state = match &previous {
                None => pseudo_random_bytes(7, size),
                Some(p) => p.clone(),
            };
            // Touch a different region each time so every delta is small but distinct.
            let at = (frame as usize * 97) % (size - 8);
            state[at..at + 8].copy_from_slice(&frame.to_le_bytes());
            match &previous {
                None => packets.push(Packet::Keyframe { metadata: metadata_at(frame), state: bv(&state) }),
                Some(p) => packets.push(region_delta(frame, p, &state)),
            }
            if frame + 1 < keyframes {
                packets.push(Packet::NextFrame { timestamp_delta: 16.into() });
            }
            states.push(state.clone());
            previous = Some(state);
        }
        (packets, states)
    }

    fn walk_keyframes(player: &mut ReplayFilePlayer, order: &[u64], states: &[Vec<u8>]) {
        for &frame in order {
            player.go_to_keyframe(frame).unwrap();
            match player.next_packet().unwrap() {
                Some(Packet::Keyframe { metadata, state }) => {
                    assert_eq!(metadata.elapsed_frames, frame);
                    assert_eq!(state.as_slice(), states[frame as usize].as_slice(), "state at frame {frame}");
                }
                other => panic!("expected keyframe {frame}, got {other:?}"),
            }
        }
    }

    /// A v6 blob (with its offset table) and the same blob written table-less (the plain
    /// discriminator) play back identically, sequentially and under seeks.
    #[test]
    fn indexed_and_plain_blobs_play_back_the_same() {
        let (packets, states) = chain_packets(12, 600);
        let indexed = blob_of(&packets);
        let Packet::CompressedBlob { keyframe_offsets, .. } = &indexed else { unreachable!() };
        assert_eq!(keyframe_offsets.len(), 12, "blob_of builds the table");
        let mut plain = indexed.clone();
        let Packet::CompressedBlob { keyframe_offsets, .. } = &mut plain else { unreachable!() };
        keyframe_offsets.clear();

        let indexed_bytes = file_with_packets(&[indexed]);
        let plain_bytes = file_with_packets(&[plain]);
        assert_eq!(indexed_bytes[..2048], plain_bytes[..2048], "same header");
        assert_eq!(indexed_bytes[2048], PacketDiscriminator::IndexedCompressedBlob as u8);
        assert_eq!(plain_bytes[2048], PacketDiscriminator::CompressedBlob as u8);

        let mut a = ReplayFilePlayer::new(&indexed_bytes, false).unwrap();
        let mut b = ReplayFilePlayer::new(&plain_bytes, false).unwrap();

        let mut walk_a = Vec::new();
        while let Some(packet) = a.next_packet().unwrap() {
            walk_a.push(packet.clone());
        }
        let mut walk_b = Vec::new();
        while let Some(packet) = b.next_packet().unwrap() {
            walk_b.push(packet.clone());
        }
        assert_eq!(walk_a.len(), walk_b.len());
        for (i, (x, y)) in walk_a.iter().zip(walk_b.iter()).enumerate() {
            assert_packet_eq(x, y, &format!("packet {i}"));
        }
        assert_eq!(walk_a.iter().filter(|p| matches!(p, Packet::Keyframe { .. })).count(), 12);

        let order = [5u64, 5, 2, 11, 0, 7, 8, 3, 11, 1];
        walk_keyframes(&mut a, &order, &states);
        walk_keyframes(&mut b, &order, &states);
        // Both walked the same chain the same way.
        assert_eq!(a.chain_folds(), b.chain_folds());
    }

    /// A seek into an indexed blob decodes only up to the target keyframe (plus the packet
    /// itself), not the whole blob; playing on decodes the rest incrementally.
    #[test]
    fn seeking_an_indexed_blob_decodes_a_prefix() {
        // 40 full keyframes of 300 KiB of incompressible bytes: a 12 MiB blob that takes many
        // decode steps (deltas would compress to almost nothing and fit in one).
        let states: Vec<Vec<u8>> = (0..40u64).map(|frame| pseudo_random_bytes(100 + frame, 300 * 1024)).collect();
        let mut packets = Vec::new();
        for (frame, state) in states.iter().enumerate() {
            packets.push(Packet::Keyframe { metadata: metadata_at(frame as u64), state: bv(state) });
            if frame + 1 < states.len() {
                packets.push(Packet::NextFrame { timestamp_delta: 16.into() });
            }
        }
        let bytes = file_with_packets(&[blob_of(&packets)]);
        let mut player = ReplayFilePlayer::new(&bytes, false).unwrap();

        player.go_to_keyframe(3).unwrap();
        let (decoded, size) = {
            let blob = player.decoded_blobs[&0].as_ref().unwrap();
            (blob.raw.len(), blob.size)
        };
        assert!(decoded < size, "seeking to keyframe 3 of 40 decoded {decoded} of {size} bytes");
        assert!(player.decoded_blobs[&0].as_ref().unwrap().decoder.is_some(), "decoder kept for the rest");

        walk_keyframes(&mut player, &[3, 4, 1, 39], &states);
        let blob = player.decoded_blobs[&0].as_ref().unwrap();
        assert_eq!(blob.raw.len(), blob.size, "the last keyframe needs the whole blob");
        assert!(blob.decoder.is_none(), "decoder released once complete");

        // Sequential playback from a cold start also decodes on demand.
        let mut player = ReplayFilePlayer::new(&bytes, false).unwrap();
        let mut keyframes = 0;
        while let Some(packet) = player.next_packet().unwrap() {
            if let Packet::Keyframe { metadata, state } = packet {
                assert_eq!(state.as_slice(), states[metadata.elapsed_frames as usize].as_slice());
                keyframes += 1;
            }
        }
        assert_eq!(keyframes, 40);
    }

    /// An offset table that does not describe the blob is ignored (the blob is scanned instead),
    /// or, when it points at plausible but wrong packets, reported as a broken packet -- never a
    /// panic or a wrong state.
    #[test]
    fn bad_offset_tables_are_ignored_or_reported() {
        let (packets, states) = chain_packets(6, 500);
        let good = blob_of(&packets);
        let Packet::CompressedBlob { keyframe_offsets: good_offsets, uncompressed_size, .. } = &good else { unreachable!() };
        let size = *uncompressed_size;

        let with_table = |offsets: Vec<u64>| {
            let mut blob = good.clone();
            let Packet::CompressedBlob { keyframe_offsets, .. } = &mut blob else { unreachable!() };
            *keyframe_offsets = offsets;
            file_with_packets(&[blob])
        };

        // Structurally invalid tables fall back to scanning and play back correctly.
        for offsets in [
            vec![0u64; 6],                       // not ascending
            vec![1, 2, 3, 4, 5, 6],              // does not start at 0
            good_offsets[..5].to_vec(),          // wrong length
            vec![0, 1, 2, 3, 4, size + 10],      // out of bounds
        ] {
            let bytes = with_table(offsets.clone());
            let mut player = ReplayFilePlayer::new(&bytes, false).unwrap();
            walk_keyframes(&mut player, &[4, 1, 5, 0], &states);
        }

        // A plausible table pointing at non-keyframe packets is a broken packet at seek time.
        let mut wrong = good_offsets.clone();
        wrong[2] += 1; // the NextFrame after keyframe 2... or mid-packet garbage
        let bytes = with_table(wrong);
        let mut player = ReplayFilePlayer::new(&bytes, false).unwrap();
        let result = player.go_to_keyframe(4);
        assert!(matches!(result, Err(ReplaySeekError::ReadError { .. })), "got {result:?}");
        // Keyframes before the bad entry are unaffected.
        walk_keyframes(&mut player, &[1, 0], &states);
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

        let (tx, rx) = std::sync::mpsc::sync_channel::<Result<DecodedBlob, ReplayFileReadError>>(1);
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
