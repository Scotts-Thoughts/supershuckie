//! Replay bookmarks (format v5).
//!
//! A replay's bookmarks are one [`BookmarkTable`]. It is stored twice in a v5 file: as the
//! editable bookmark section after the packet stream (see
//! [`replay_file::bookmark_section`](crate::replay_file::bookmark_section)), and as
//! [`Packet::BookmarkTable`](crate::Packet::BookmarkTable) snapshots inside the stream, which a
//! file that was never closed is recovered from.
//!
//! Every change replaces the whole table; there are no per-bookmark packets.

use alloc::borrow::Cow;
use alloc::string::String;
use alloc::vec::Vec;

use crate::{BookmarkMetadata, ByteVec, PacketIO, PacketInstructionsVec, PacketReadError, PacketWriteCommand, TimestampMillis, UnsignedInteger};

/// How many frames a keyframe bookmark sits after its keyframe.
///
/// A seek loads a keyframe at least this many frames before the frame it shows (see
/// `SuperShuckieCore::POST_LOAD_FRAMES`, which must equal this), so a keyframe bookmark at
/// `in_frame` is reached by loading exactly the keyframe at `in_frame - KEYFRAME_BOOKMARK_LEAD_FRAMES`.
pub const KEYFRAME_BOOKMARK_LEAD_FRAMES: UnsignedInteger = 3;

/// Encoding version of [`BookmarkTable`] this build writes. Readers refuse a table whose format is
/// newer than this; fields added without breaking older readers are appended to the records instead
/// (readers ignore trailing bytes in a record).
pub const BOOKMARK_TABLE_FORMAT: u32 = 1;

const FLAG_HAS_OUT: UnsignedInteger = 1 << 0;
const FLAG_KEYFRAME: UnsignedInteger = 1 << 1;

/// A named point or range in a replay.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Bookmark {
    /// Unique within the replay and never reused (see [`BookmarkTable::allocate_id`]).
    pub id: UnsignedInteger,

    /// Name shown to the user.
    pub name: String,

    /// Type of the bookmark; 0 is untyped. Resolved against the user's settings first and
    /// [`BookmarkTable::types`] second.
    pub type_id: UnsignedInteger,

    /// Frame the bookmark starts on: the frame counter's value while that frame is on screen.
    pub in_frame: UnsignedInteger,

    /// Replay time at `in_frame`.
    pub in_millis: TimestampMillis,

    /// Frame and replay time the bookmark ends on, for a range. The frame is never before `in_frame`.
    pub out: Option<(UnsignedInteger, TimestampMillis)>,

    /// Whether a keyframe was placed so that this bookmark is reached without re-emulation: the
    /// replay has a keyframe at [`Self::keyframe_frame`].
    pub keyframe: bool
}

impl Bookmark {
    /// The out frame, if this is a range.
    pub fn out_frame(&self) -> Option<UnsignedInteger> {
        self.out.map(|(frame, _)| frame)
    }

    /// The frame of the keyframe this bookmark is anchored to, if it is a keyframe bookmark.
    pub fn keyframe_frame(&self) -> Option<UnsignedInteger> {
        self.keyframe.then(|| self.in_frame.saturating_sub(KEYFRAME_BOOKMARK_LEAD_FRAMES))
    }

    fn encode(&self) -> Vec<u8> {
        let flags = if self.out.is_some() { FLAG_HAS_OUT } else { 0 } | if self.keyframe { FLAG_KEYFRAME } else { 0 };
        let (out_frame, out_millis) = self.out.unwrap_or((0, TimestampMillis(0)));

        let mut bytes = Vec::new();
        append(&mut bytes, &self.id);
        append(&mut bytes, &self.name);
        append(&mut bytes, &self.type_id);
        append(&mut bytes, &self.in_frame);
        append(&mut bytes, &self.in_millis);
        append(&mut bytes, &flags);
        append(&mut bytes, &out_frame);
        append(&mut bytes, &out_millis);
        bytes
    }

    fn decode(mut record: &[u8], version: u32) -> Result<Self, PacketReadError> {
        let from = &mut record;
        let id = UnsignedInteger::read_all(from, version)?;
        let name = String::read_all(from, version)?;
        let type_id = UnsignedInteger::read_all(from, version)?;
        let in_frame = UnsignedInteger::read_all(from, version)?;
        let in_millis = TimestampMillis::read_all(from, version)?;
        let flags = UnsignedInteger::read_all(from, version)?;
        let out_frame = UnsignedInteger::read_all(from, version)?;
        let out_millis = TimestampMillis::read_all(from, version)?;

        // Anything after the known fields belongs to a newer writer and is ignored.
        Ok(Self {
            id,
            name,
            type_id,
            in_frame,
            in_millis,
            out: (flags & FLAG_HAS_OUT != 0).then_some((out_frame, out_millis)),
            keyframe: flags & FLAG_KEYFRAME != 0
        })
    }
}

/// A bookmark type as recorded in a replay: a snapshot of the user's type at the time the replay's
/// bookmarks were last saved, so the replay still shows its colors where that type is unknown.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct BookmarkTypeRecord {
    /// Non-zero id, shared with the user's settings.
    pub id: UnsignedInteger,

    /// Name of the type.
    pub name: String,

    /// Color as `0xRRGGBB`.
    pub color: u32
}

impl BookmarkTypeRecord {
    fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        append(&mut bytes, &self.id);
        append(&mut bytes, &self.name);
        append(&mut bytes, &self.color);
        bytes
    }

    fn decode(mut record: &[u8], version: u32) -> Result<Self, PacketReadError> {
        let from = &mut record;
        Ok(Self {
            id: UnsignedInteger::read_all(from, version)?,
            name: String::read_all(from, version)?,
            color: u32::read_all(from, version)?
        })
    }
}

/// Every bookmark of a replay, plus the types they use.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BookmarkTable {
    /// The id the next new bookmark gets.
    pub next_id: UnsignedInteger,

    /// A record for every type referenced by [`Self::bookmarks`] (see [`Self::prune_types`]).
    pub types: Vec<BookmarkTypeRecord>,

    /// Bookmarks sorted by `(in_frame, id)` (see [`Self::sort`]).
    pub bookmarks: Vec<Bookmark>
}

impl Default for BookmarkTable {
    fn default() -> Self {
        Self::new()
    }
}

impl BookmarkTable {
    /// An empty table.
    pub const fn new() -> Self {
        Self { next_id: 1, types: Vec::new(), bookmarks: Vec::new() }
    }

    /// Whether the table holds no bookmarks.
    pub fn is_empty(&self) -> bool {
        self.bookmarks.is_empty()
    }

    /// Reserve a new bookmark id.
    pub fn allocate_id(&mut self) -> UnsignedInteger {
        let id = self.next_id.max(1);
        self.next_id = id + 1;
        id
    }

    /// Insert a bookmark, keeping the table sorted. An id of 0 is replaced with a new one; a bookmark
    /// with an id that already exists replaces it. Returns the id.
    pub fn insert(&mut self, mut bookmark: Bookmark) -> UnsignedInteger {
        if bookmark.id == 0 {
            bookmark.id = self.allocate_id();
        }
        else if bookmark.id >= self.next_id {
            self.next_id = bookmark.id + 1;
        }

        let id = bookmark.id;
        self.bookmarks.retain(|b| b.id != id);
        let at = self.bookmarks.partition_point(|b| (b.in_frame, b.id) < (bookmark.in_frame, id));
        self.bookmarks.insert(at, bookmark);
        id
    }

    /// The bookmark with this id.
    pub fn get(&self, id: UnsignedInteger) -> Option<&Bookmark> {
        self.bookmarks.iter().find(|b| b.id == id)
    }

    /// The bookmark with this id, mutably. Call [`Self::sort`] after changing its `in_frame`.
    pub fn get_mut(&mut self, id: UnsignedInteger) -> Option<&mut Bookmark> {
        self.bookmarks.iter_mut().find(|b| b.id == id)
    }

    /// Remove the bookmark with this id.
    pub fn remove(&mut self, id: UnsignedInteger) -> Option<Bookmark> {
        let index = self.bookmarks.iter().position(|b| b.id == id)?;
        Some(self.bookmarks.remove(index))
    }

    /// Sort the bookmarks by `(in_frame, id)`.
    pub fn sort(&mut self) {
        self.bookmarks.sort_by_key(|b| (b.in_frame, b.id));
    }

    /// The recorded type with this id.
    pub fn type_record(&self, id: UnsignedInteger) -> Option<&BookmarkTypeRecord> {
        self.types.iter().find(|t| t.id == id)
    }

    /// Add or replace a type record.
    pub fn set_type_record(&mut self, record: BookmarkTypeRecord) {
        match self.types.iter_mut().find(|t| t.id == record.id) {
            Some(existing) => *existing = record,
            None => self.types.push(record)
        }
    }

    /// Drop type records no bookmark uses, and sort the rest by id.
    pub fn prune_types(&mut self) {
        let bookmarks = &self.bookmarks;
        self.types.retain(|t| t.id != 0 && bookmarks.iter().any(|b| b.type_id == t.id));
        self.types.sort_by_key(|t| t.id);
    }

    /// The table as it applies to a replay cut after `frame` (resume support): bookmarks starting
    /// after `frame` are dropped, and ranges ending after it lose their out frame.
    pub fn truncated_to(&self, frame: UnsignedInteger) -> BookmarkTable {
        let mut table = self.clone();
        table.bookmarks.retain(|b| b.in_frame <= frame);
        for bookmark in &mut table.bookmarks {
            if bookmark.out_frame().is_some_and(|out| out > frame) {
                bookmark.out = None;
            }
        }
        table.prune_types();
        table
    }

    /// Frames holding a keyframe that a keyframe bookmark relies on. Re-encoding a replay must keep
    /// the keyframes on these frames full.
    pub fn keyframe_anchor_frames(&self) -> impl Iterator<Item = UnsignedInteger> + '_ {
        self.bookmarks.iter().filter_map(Bookmark::keyframe_frame)
    }

    /// Convert the bookmarks of a pre-v5 replay: untyped points, numbered in `(frame, name)` order.
    pub fn from_legacy<'a, I: IntoIterator<Item = &'a BookmarkMetadata>>(legacy: I) -> BookmarkTable {
        let mut legacy: Vec<&BookmarkMetadata> = legacy.into_iter().collect();
        legacy.sort_by(|a, b| (a.elapsed_frames, &a.name).cmp(&(b.elapsed_frames, &b.name)));

        let mut table = BookmarkTable::new();
        for metadata in legacy {
            let id = table.allocate_id();
            table.bookmarks.push(Bookmark {
                id,
                name: metadata.name.clone(),
                type_id: 0,
                in_frame: metadata.elapsed_frames,
                in_millis: metadata.elapsed_millis,
                out: None,
                keyframe: false
            });
        }
        table
    }

    /// Serialize the table (the payload of both the bookmark section and a `BookmarkTable` packet).
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        append(&mut bytes, &BOOKMARK_TABLE_FORMAT);
        append(&mut bytes, &self.next_id);

        append(&mut bytes, &self.types.len());
        for record in &self.types {
            append(&mut bytes, &ByteVec::Heap(record.encode()));
        }

        append(&mut bytes, &self.bookmarks.len());
        for bookmark in &self.bookmarks {
            append(&mut bytes, &ByteVec::Heap(bookmark.encode()));
        }

        bytes
    }

    /// Parse a table written by [`Self::encode`]. Bytes after the table are ignored.
    pub fn decode(mut bytes: &[u8]) -> Result<BookmarkTable, PacketReadError> {
        // Tables have their own format number; the replay version does not affect their encoding.
        const VERSION: u32 = crate::replay_file::REPLAY_VERSION;

        let from = &mut bytes;
        let format = u32::read_all(from, VERSION)?;
        if format == 0 || format > BOOKMARK_TABLE_FORMAT {
            return Err(PacketReadError::ParseFail { explanation: Cow::Owned(alloc::format!("unsupported bookmark table format {format}")) });
        }

        let next_id = UnsignedInteger::read_all(from, VERSION)?;

        let type_count = usize::read_all(from, VERSION)?;
        let mut types = Vec::with_capacity(type_count.min(from.len()));
        for _ in 0..type_count {
            types.push(BookmarkTypeRecord::decode(ByteVec::read_all(from, VERSION)?.as_slice(), VERSION)?);
        }

        let bookmark_count = usize::read_all(from, VERSION)?;
        let mut bookmarks = Vec::with_capacity(bookmark_count.min(from.len()));
        for _ in 0..bookmark_count {
            bookmarks.push(Bookmark::decode(ByteVec::read_all(from, VERSION)?.as_slice(), VERSION)?);
        }

        let mut table = BookmarkTable { next_id, types, bookmarks };
        table.sort();
        let highest = table.bookmarks.iter().map(|b| b.id).max().unwrap_or(0);
        table.next_id = table.next_id.max(highest + 1).max(1);
        Ok(table)
    }
}

fn append<'a, T: PacketIO<'a>>(bytes: &mut Vec<u8>, value: &'a T) {
    for command in value.write_packet_instructions() {
        bytes.extend_from_slice(command.bytes());
    }
}

impl PacketIO<'_> for BookmarkTable {
    fn write_packet_instructions(&'_ self) -> PacketInstructionsVec<'_> {
        // Length-prefixed, so a reader can skip anything a newer writer appends after the table.
        let payload = self.encode();
        let mut instructions = PacketInstructionsVec::new();
        let mut length = Vec::new();
        append(&mut length, &payload.len());
        instructions.push(PacketWriteCommand::WriteVec { bytes: ByteVec::Heap(length) });
        instructions.push(PacketWriteCommand::WriteVec { bytes: ByteVec::Heap(payload) });
        instructions
    }

    fn read_all(from: &mut &[u8], version: u32) -> Result<Self, PacketReadError> {
        let payload = ByteVec::read_all(from, version)?;
        BookmarkTable::decode(payload.as_slice())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::string::ToString;

    fn sample() -> BookmarkTable {
        let mut table = BookmarkTable::new();
        table.insert(Bookmark { name: "Split 1".into(), type_id: 0x55, in_frame: 18_402, in_millis: 306_700.into(), keyframe: true, ..Default::default() });
        table.insert(Bookmark { name: "Bike skip".into(), type_id: 0x77, in_frame: 31_120, in_millis: 518_667.into(), out: Some((32_560, 542_667.into())), ..Default::default() });
        table.insert(Bookmark { name: "Death".into(), in_frame: 7, in_millis: 116.into(), ..Default::default() });
        table.set_type_record(BookmarkTypeRecord { id: 0x77, name: "Route".into(), color: 0x1B8577 });
        table.set_type_record(BookmarkTypeRecord { id: 0x55, name: "Split".into(), color: 0xB87B0C });
        table
    }

    #[test]
    fn insert_keeps_order_and_allocates_ids() {
        let table = sample();
        assert_eq!(table.bookmarks.iter().map(|b| (b.id, b.in_frame)).collect::<Vec<_>>(), vec![(3, 7), (1, 18_402), (2, 31_120)]);
        assert_eq!(table.next_id, 4);

        let mut table = table;
        table.insert(Bookmark { id: 10, name: "explicit".into(), in_frame: 7, ..Default::default() });
        assert_eq!(table.next_id, 11);
        assert_eq!(table.bookmarks[1].id, 10, "same frame sorts by id");

        // Replacing by id keeps one entry.
        table.insert(Bookmark { id: 10, name: "moved".into(), in_frame: 40_000, ..Default::default() });
        assert_eq!(table.bookmarks.len(), 4);
        assert_eq!(table.bookmarks.last().unwrap().name, "moved");
    }

    #[test]
    fn round_trips() {
        let table = sample();
        assert_eq!(BookmarkTable::decode(&table.encode()).unwrap(), table);

        let empty = BookmarkTable::new();
        assert_eq!(BookmarkTable::decode(&empty.encode()).unwrap(), empty);
    }

    #[test]
    fn trailing_record_bytes_and_unknown_flags_are_ignored() {
        let mut bookmark = Bookmark { id: 1, name: "x".into(), in_frame: 5, in_millis: 80.into(), keyframe: true, ..Default::default() };
        let mut record = bookmark.encode();
        // Set an unknown flag bit (the flags field follows id, name, type, in frame, in millis).
        let mut prefix = Vec::new();
        append(&mut prefix, &bookmark.id);
        append(&mut prefix, &bookmark.name);
        append(&mut prefix, &bookmark.type_id);
        append(&mut prefix, &bookmark.in_frame);
        append(&mut prefix, &bookmark.in_millis);
        let mut flags = Vec::new();
        append(&mut flags, &(FLAG_KEYFRAME | 1 << 40));
        record.splice(prefix.len()..prefix.len() + 2, flags);
        record.extend_from_slice(&[0xAA, 0xBB, 0xCC]);

        let mut type_record = BookmarkTypeRecord { id: 9, name: "t".into(), color: 0x123456 }.encode();
        type_record.push(0x42);

        let mut bytes = Vec::new();
        append(&mut bytes, &BOOKMARK_TABLE_FORMAT);
        append(&mut bytes, &2u64);
        append(&mut bytes, &1usize);
        append(&mut bytes, &ByteVec::Heap(type_record));
        append(&mut bytes, &1usize);
        append(&mut bytes, &ByteVec::Heap(record));
        bytes.extend_from_slice(b"appended by a newer writer");

        let table = BookmarkTable::decode(&bytes).unwrap();
        bookmark.type_id = 0;
        assert_eq!(table.bookmarks, vec![bookmark]);
        assert_eq!(table.types, vec![BookmarkTypeRecord { id: 9, name: "t".into(), color: 0x123456 }]);
    }

    #[test]
    fn newer_table_formats_are_refused() {
        let mut bytes = BookmarkTable::new().encode();
        bytes[..4].copy_from_slice(&(BOOKMARK_TABLE_FORMAT + 1).to_le_bytes());
        assert!(BookmarkTable::decode(&bytes).is_err());
    }

    #[test]
    fn next_id_never_falls_behind_the_ids_in_use() {
        let mut table = sample();
        table.next_id = 1;
        let decoded = BookmarkTable::decode(&table.encode()).unwrap();
        assert_eq!(decoded.next_id, 4);
    }

    #[test]
    fn truncation_drops_later_bookmarks_and_cuts_ranges() {
        let table = sample();
        let cut = table.truncated_to(32_000);
        assert_eq!(cut.bookmarks.len(), 3);
        assert_eq!(cut.get(2).unwrap().out, None, "the range crossing the cut loses its out frame");

        let cut = table.truncated_to(20_000);
        assert_eq!(cut.bookmarks.iter().map(|b| b.id).collect::<Vec<_>>(), vec![3, 1]);
        assert_eq!(cut.types.iter().map(|t| t.id).collect::<Vec<_>>(), vec![0x55], "unused type records are dropped");
        assert_eq!(cut.next_id, table.next_id, "ids are never reused");
    }

    #[test]
    fn keyframe_anchors() {
        let table = sample();
        assert_eq!(table.keyframe_anchor_frames().collect::<Vec<_>>(), vec![18_402 - KEYFRAME_BOOKMARK_LEAD_FRAMES]);
    }

    #[test]
    fn legacy_bookmarks_become_untyped_points() {
        let legacy = [
            BookmarkMetadata { name: "late".to_string(), elapsed_frames: 76, elapsed_millis: 1.into() },
            BookmarkMetadata { name: "checkpoint".to_string(), elapsed_frames: 165, elapsed_millis: 2.into() },
            BookmarkMetadata { name: "checkpoint".to_string(), elapsed_frames: 7, elapsed_millis: 3.into() },
        ];
        let table = BookmarkTable::from_legacy(&legacy);
        assert_eq!(
            table.bookmarks.iter().map(|b| (b.id, b.name.as_str(), b.in_frame, b.type_id, b.out, b.keyframe)).collect::<Vec<_>>(),
            vec![(1, "checkpoint", 7, 0, None, false), (2, "late", 76, 0, None, false), (3, "checkpoint", 165, 0, None, false)]
        );
        assert_eq!(table.next_id, 4);
    }
}
