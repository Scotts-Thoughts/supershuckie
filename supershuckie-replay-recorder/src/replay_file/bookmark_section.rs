//! The bookmark section at the end of a closed replay (format v5).
//!
//! ```text
//! packet_stream_end + 0x00  [4]            magic "SSBM"
//!                   + 0x04  u32            section format (1)
//!                   + 0x08  u64            payload length
//!                   + 0x10  [32]           blake3 hash of the payload
//!                   + 0x30  [payload len]  BookmarkTable encoding
//! ```
//!
//! The section must end exactly at the end of the file. Anything that does not check out is
//! treated as missing: the player then falls back to the newest `BookmarkTable` snapshot in the
//! packet stream (see `ReplayFilePlayer::bookmark_table`), and the replay itself stays playable.
//!
//! [`write_bookmark_section`] rewrites the section of a replay on disk, upgrading a v3/v4 file to v5
//! in place the first time.

use alloc::borrow::Cow;
use alloc::vec::Vec;
use core::fmt::{Display, Formatter};

use crate::bookmarks::BookmarkTable;
use crate::util::blake3_hash;

/// Magic at the start of the section.
pub const BOOKMARK_SECTION_MAGIC: [u8; 4] = *b"SSBM";

/// Section layout version this build reads and writes.
pub const BOOKMARK_SECTION_FORMAT: u32 = 1;

/// Length of the section's fixed header (magic, format, payload length, hash).
pub const BOOKMARK_SECTION_HEADER_LEN: usize = 0x30;

/// Why a bookmark section could not be read.
#[derive(Clone, PartialEq, Debug)]
pub enum BookmarkSectionError {
    /// The file ends before a complete section header.
    Truncated,
    /// The section does not start with [`BOOKMARK_SECTION_MAGIC`].
    BadMagic,
    /// The section was written in a newer layout.
    UnsupportedFormat {
        /// The layout version found.
        format: u32
    },
    /// The payload length does not match the bytes up to the end of the file.
    LengthMismatch {
        /// Payload length recorded in the section header.
        recorded: u64,
        /// Bytes actually present after the section header.
        present: u64
    },
    /// The payload does not match its hash.
    HashMismatch,
    /// The payload is not a valid bookmark table.
    BadPayload {
        /// What went wrong.
        explanation: Cow<'static, str>
    }
}

impl Display for BookmarkSectionError {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Truncated => f.write_str("the bookmark section is cut short"),
            Self::BadMagic => f.write_str("the bookmark section has an unknown signature"),
            Self::UnsupportedFormat { format } => write!(f, "the bookmark section uses layout {format}, which this version cannot read"),
            Self::LengthMismatch { recorded, present } => write!(f, "the bookmark section records {recorded} bytes of bookmarks but {present} are present"),
            Self::HashMismatch => f.write_str("the bookmark section is damaged (checksum mismatch)"),
            Self::BadPayload { explanation } => write!(f, "the bookmark section cannot be parsed: {explanation}")
        }
    }
}

/// Serialize `table` as a complete section.
pub fn encode_bookmark_section(table: &BookmarkTable) -> Vec<u8> {
    let payload = table.encode();
    let mut section = Vec::with_capacity(BOOKMARK_SECTION_HEADER_LEN + payload.len());
    section.extend_from_slice(&BOOKMARK_SECTION_MAGIC);
    section.extend_from_slice(&BOOKMARK_SECTION_FORMAT.to_le_bytes());
    section.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    section.extend_from_slice(&blake3_hash(&payload));
    section.extend_from_slice(&payload);
    section
}

/// Parse a section; `section` runs from `packet_stream_end` to the end of the file.
pub fn decode_bookmark_section(section: &[u8]) -> Result<BookmarkTable, BookmarkSectionError> {
    let Some((header, payload)) = section.split_at_checked(BOOKMARK_SECTION_HEADER_LEN) else {
        return Err(BookmarkSectionError::Truncated)
    };

    if header[0x00..0x04] != BOOKMARK_SECTION_MAGIC {
        return Err(BookmarkSectionError::BadMagic)
    }

    let format = u32::from_le_bytes(header[0x04..0x08].try_into().expect("4 bytes"));
    if format != BOOKMARK_SECTION_FORMAT {
        return Err(BookmarkSectionError::UnsupportedFormat { format })
    }

    let recorded = u64::from_le_bytes(header[0x08..0x10].try_into().expect("8 bytes"));
    let present = payload.len() as u64;
    if recorded != present {
        return Err(BookmarkSectionError::LengthMismatch { recorded, present })
    }

    if header[0x10..0x30] != blake3_hash(payload) {
        return Err(BookmarkSectionError::HashMismatch)
    }

    BookmarkTable::decode(payload).map_err(|e| BookmarkSectionError::BadPayload {
        explanation: match e {
            crate::PacketReadError::NotEnoughData => Cow::Borrowed("not enough data"),
            crate::PacketReadError::ParseFail { explanation } => explanation
        }
    })
}

#[cfg(feature = "std")]
pub use file::*;

#[cfg(feature = "std")]
mod file {
    use super::encode_bookmark_section;
    use crate::bookmarks::BookmarkTable;
    use crate::replay_file::{ReplayHeaderBytes, ReplayHeaderRaw, REPLAY_VERSION, REPLAY_VERSION_BOOKMARK_SECTION};
    use alloc::format;
    use alloc::string::String;
    use std::fmt::{Display, Formatter};
    use std::fs::OpenOptions;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::path::Path;

    /// Oldest format a replay can be upgraded to v5 from in place. A v2 header's packets would be
    /// misparsed under a v5 header (v2 keyframe metadata has no counters).
    pub const BOOKMARK_UPGRADE_MINIMUM_VERSION: u32 = 3;

    /// Result of a successful [`write_bookmark_section`].
    #[derive(Clone, Debug)]
    pub struct BookmarkSectionWriteOutcome {
        /// The header now on disk; pass it as `expected_header` to the next write.
        pub header: ReplayHeaderBytes,
        /// The version the file was upgraded from, if this write upgraded it to v5.
        pub upgraded_from: Option<u32>
    }

    /// Why [`write_bookmark_section`] did not write.
    #[derive(Clone, Debug, PartialEq)]
    pub enum BookmarkSectionWriteError {
        /// Reading or writing the file failed.
        Io {
            /// What went wrong.
            explanation: String
        },
        /// The file on disk is not the replay that was loaded (its header differs).
        HeaderChanged,
        /// The replay is too old to be upgraded in place; convert it first.
        NeedsConversion {
            /// The replay's format version.
            version: u32
        },
        /// The file is not a readable replay, or its packet stream is out of bounds.
        InvalidReplay {
            /// What went wrong.
            explanation: String
        }
    }

    impl Display for BookmarkSectionWriteError {
        fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Io { explanation } => f.write_str(explanation),
                Self::HeaderChanged => f.write_str("The replay file was changed on disk after it was loaded."),
                Self::NeedsConversion { version } => write!(f, "This replay uses format v{version}. Convert it to the current format before adding bookmarks."),
                Self::InvalidReplay { explanation } => write!(f, "This replay cannot hold bookmarks: {explanation}")
            }
        }
    }

    fn io(what: &str, error: std::io::Error) -> BookmarkSectionWriteError {
        BookmarkSectionWriteError::Io { explanation: format!("Failed to {what}: {error}") }
    }

    /// Replace the bookmark section of the replay at `path` with `table`.
    ///
    /// When `expected_header` is given, the write is refused unless the file's header is exactly
    /// that one (the header captured when the replay was loaded, or returned by the previous write),
    /// so a replay replaced on disk is never modified.
    ///
    /// A v3/v4 file, or a v5 file whose recording was never closed, first gets a v5 header whose
    /// `packet_stream_end` is the current file length. The header is written before the section,
    /// and the file is synced afterwards; if the process dies in between, the section is invalid
    /// and players fall back to the bookmarks in the packet stream.
    pub fn write_bookmark_section(path: &Path, expected_header: Option<&ReplayHeaderBytes>, table: &BookmarkTable) -> Result<BookmarkSectionWriteOutcome, BookmarkSectionWriteError> {
        let mut file = OpenOptions::new().read(true).write(true).open(path).map_err(|e| io("open the replay for writing", e))?;

        let mut header_bytes = [0u8; size_of::<ReplayHeaderBytes>()];
        file.read_exact(&mut header_bytes).map_err(|e| match e.kind() {
            std::io::ErrorKind::UnexpectedEof => BookmarkSectionWriteError::InvalidReplay { explanation: String::from("the file is too short to be a replay") },
            _ => io("read the replay header", e)
        })?;

        if let Some(expected) = expected_header && *expected != header_bytes {
            return Err(BookmarkSectionWriteError::HeaderChanged)
        }

        let header = ReplayHeaderRaw::from_bytes(&header_bytes);
        header.parse().map_err(|explanation| BookmarkSectionWriteError::InvalidReplay { explanation })?;

        let version = header.replay_version;
        if version < BOOKMARK_UPGRADE_MINIMUM_VERSION {
            return Err(BookmarkSectionWriteError::NeedsConversion { version })
        }

        let file_len = file.metadata().map_err(|e| io("read the replay's size", e))?.len();
        let packets_start = size_of::<ReplayHeaderBytes>() as u64 + header.patch_data_length;

        let (stream_end, new_header) = match header.packet_stream_end() {
            Some(end) => {
                if end < packets_start || end > file_len {
                    return Err(BookmarkSectionWriteError::InvalidReplay { explanation: format!("its packet stream ends at byte {end}, outside the file ({file_len} bytes)") })
                }
                (end, None)
            }
            None => {
                if file_len < packets_start {
                    return Err(BookmarkSectionWriteError::InvalidReplay { explanation: String::from("the file ends inside its patch data") })
                }
                let mut upgraded = *header;
                upgraded.replay_version = REPLAY_VERSION.max(REPLAY_VERSION_BOOKMARK_SECTION);
                upgraded.packet_stream_end = file_len;
                (file_len, Some(upgraded))
            }
        };

        if let Some(new_header) = new_header.as_ref() {
            file.seek(SeekFrom::Start(0)).map_err(|e| io("seek to the replay header", e))?;
            file.write_all(new_header.as_bytes()).map_err(|e| io("write the replay header", e))?;
        }

        let section = encode_bookmark_section(table);
        file.seek(SeekFrom::Start(stream_end)).map_err(|e| io("seek to the bookmark section", e))?;
        file.write_all(&section).map_err(|e| io("write the bookmark section", e))?;
        file.set_len(stream_end + section.len() as u64).map_err(|e| io("truncate the replay", e))?;
        file.sync_data().map_err(|e| io("flush the replay to disk", e))?;

        Ok(BookmarkSectionWriteOutcome {
            header: new_header.map(|h| *h.as_bytes()).unwrap_or(header_bytes),
            upgraded_from: (version < REPLAY_VERSION_BOOKMARK_SECTION).then_some(version)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bookmarks::{Bookmark, BookmarkTypeRecord};

    fn table() -> BookmarkTable {
        let mut table = BookmarkTable::new();
        table.insert(Bookmark { name: "a".into(), type_id: 4, in_frame: 10, in_millis: 160.into(), out: Some((20, 330.into())), ..Default::default() });
        table.set_type_record(BookmarkTypeRecord { id: 4, name: "t".into(), color: 0xFF0000 });
        table
    }

    #[test]
    fn valid_sections_read_back() {
        let section = encode_bookmark_section(&table());
        assert_eq!(section.len(), BOOKMARK_SECTION_HEADER_LEN + table().encode().len());
        assert_eq!(decode_bookmark_section(&section), Ok(table()));
    }

    #[cfg(feature = "std")]
    mod file_writes {
        use super::table;
        use crate::bookmarks::{Bookmark, BookmarkTable};
        use crate::replay_file::bookmark_section::{write_bookmark_section, BookmarkSectionWriteError};
        use crate::replay_file::playback::{BookmarkTableSource, ReplayFilePlayer};
        use crate::replay_file::record::ReplayFileRecorderSettings;
        use crate::replay_file::{ReplayHeaderBytes, ReplayHeaderRaw, REPLAY_VERSION};
        use crate::test_support::*;
        use alloc::format;
        use std::path::PathBuf;

        fn temp_file(name: &str, bytes: &[u8]) -> PathBuf {
            let dir = std::env::temp_dir().join(format!("supershuckie-bookmark-section-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        }

        fn header_of(bytes: &[u8]) -> ReplayHeaderBytes {
            bytes[..2048].try_into().unwrap()
        }

        #[test]
        fn rewrites_grow_and_shrink_a_closed_replay() {
            let (_, closed) = record_script(ReplayFileRecorderSettings { max_frames_per_blob: 45, ..Default::default() });
            let path = temp_file("closed.replay", &closed);
            let stream_end = ReplayHeaderRaw::from_bytes(&header_of(&closed)).packet_stream_end().unwrap();

            let mut bigger = script_table_through(TOTAL_FRAMES);
            for frame in (10..190).step_by(10) {
                bigger.insert(Bookmark { name: format!("extra {frame}"), in_frame: frame, in_millis: millis_at(frame).into(), ..Default::default() });
            }
            let outcome = write_bookmark_section(&path, Some(&header_of(&closed)), &bigger).unwrap();
            assert_eq!(outcome.upgraded_from, None);
            assert_eq!(outcome.header, header_of(&closed), "a closed v5 file keeps its header");

            let bytes = std::fs::read(&path).unwrap();
            assert!(bytes.len() > closed.len());
            assert_eq!(&bytes[..stream_end as usize], &closed[..stream_end as usize], "the packets are untouched");
            check_script_replay_with(&bytes, "grown section", BookmarkExpectation::Rewritten(&bigger));

            let empty = BookmarkTable::new();
            write_bookmark_section(&path, Some(&outcome.header), &empty).unwrap();
            let bytes = std::fs::read(&path).unwrap();
            assert!(bytes.len() < closed.len());
            let player = ReplayFilePlayer::new(&bytes, false).unwrap();
            assert_eq!(player.bookmark_table_source(), BookmarkTableSource::Section);
            assert!(player.bookmark_table().is_empty());

            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn v3_replays_are_upgraded_in_place() {
            for (name, fixture) in [("v3-small.replay", V3_SMALL), ("v3-small-closed.replay", V3_SMALL_CLOSED)] {
                let path = temp_file(name, fixture);

                let outcome = write_bookmark_section(&path, Some(&header_of(fixture)), &table()).unwrap();
                assert_eq!(outcome.upgraded_from, Some(3), "{name}");

                let bytes = std::fs::read(&path).unwrap();
                let header_bytes = header_of(&bytes);
                let header = ReplayHeaderRaw::from_bytes(&header_bytes);
                let version = header.replay_version;
                assert_eq!(version, REPLAY_VERSION, "{name}");
                assert_eq!(header.packet_stream_end(), Some(fixture.len() as u64), "{name}");
                assert_eq!(outcome.header, header_of(&bytes), "{name}");
                assert!(header.same_replay_as(ReplayHeaderRaw::from_bytes(&header_of(fixture))), "{name}");

                // The legacy packets still play back exactly; the section is now authoritative.
                check_script_replay_with(&bytes, name, BookmarkExpectation::Fixed(&table()));
                assert_eq!(ReplayFilePlayer::new(&bytes, false).unwrap().legacy_bookmarks().len(), 2, "{name}");

                // A second write needs the upgraded header, not the one loaded before the upgrade.
                assert_eq!(write_bookmark_section(&path, Some(&header_of(fixture)), &BookmarkTable::new()).unwrap_err(), BookmarkSectionWriteError::HeaderChanged, "{name}");
                write_bookmark_section(&path, Some(&outcome.header), &BookmarkTable::new()).unwrap();
                assert_eq!(std::fs::read(&path).unwrap().len(), fixture.len() + super::super::encode_bookmark_section(&BookmarkTable::new()).len(), "{name}");

                let _ = std::fs::remove_file(&path);
            }
        }

        #[test]
        fn old_changed_and_broken_replays_are_refused() {
            // v2 cannot be upgraded in place.
            let mut v2 = V3_SMALL_CLOSED.to_vec();
            v2[4..8].copy_from_slice(&2u32.to_le_bytes());
            let path = temp_file("v2.replay", &v2);
            assert_eq!(write_bookmark_section(&path, None, &table()).unwrap_err(), BookmarkSectionWriteError::NeedsConversion { version: 2 });
            assert_eq!(std::fs::read(&path).unwrap(), v2, "nothing was written");

            // A header that changed since loading.
            let path = temp_file("changed.replay", V3_SMALL_CLOSED);
            let mut loaded = header_of(V3_SMALL_CLOSED);
            loaded[0x0C] = 1;
            assert_eq!(write_bookmark_section(&path, Some(&loaded), &table()).unwrap_err(), BookmarkSectionWriteError::HeaderChanged);

            // A closed file cut inside its packets.
            let (_, closed) = record_script(ReplayFileRecorderSettings { max_frames_per_blob: 45, ..Default::default() });
            let stream_end = ReplayHeaderRaw::from_bytes(&header_of(&closed)).packet_stream_end().unwrap() as usize;
            let path = temp_file("cut.replay", &closed[..stream_end - 10]);
            assert!(matches!(write_bookmark_section(&path, None, &table()), Err(BookmarkSectionWriteError::InvalidReplay { .. })));

            // Not a replay.
            let path = temp_file("junk.replay", b"definitely not a replay");
            assert!(matches!(write_bookmark_section(&path, None, &table()), Err(BookmarkSectionWriteError::InvalidReplay { .. })));

            // Missing.
            assert!(matches!(write_bookmark_section(&path.with_extension("missing"), None, &table()), Err(BookmarkSectionWriteError::Io { .. })));
        }
    }

    #[test]
    fn every_corruption_is_detected() {
        let section = encode_bookmark_section(&table());

        assert_eq!(decode_bookmark_section(&[]), Err(BookmarkSectionError::Truncated));
        assert_eq!(decode_bookmark_section(&section[..BOOKMARK_SECTION_HEADER_LEN - 1]), Err(BookmarkSectionError::Truncated));

        let mut bad = section.clone();
        bad[0] = b'X';
        assert_eq!(decode_bookmark_section(&bad), Err(BookmarkSectionError::BadMagic));

        let mut bad = section.clone();
        bad[4] = 2;
        assert_eq!(decode_bookmark_section(&bad), Err(BookmarkSectionError::UnsupportedFormat { format: 2 }));

        let payload_len = (section.len() - BOOKMARK_SECTION_HEADER_LEN) as u64;
        assert_eq!(decode_bookmark_section(&section[..section.len() - 1]), Err(BookmarkSectionError::LengthMismatch { recorded: payload_len, present: payload_len - 1 }));
        let mut longer = section.clone();
        longer.push(0);
        assert!(matches!(decode_bookmark_section(&longer), Err(BookmarkSectionError::LengthMismatch { .. })));

        let mut bad = section.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert_eq!(decode_bookmark_section(&bad), Err(BookmarkSectionError::HashMismatch));

        // A payload that hashes correctly but is not a table.
        let mut bad = section[..BOOKMARK_SECTION_HEADER_LEN].to_vec();
        let junk = [0xFFu8; 3];
        bad[0x08..0x10].copy_from_slice(&(junk.len() as u64).to_le_bytes());
        bad[0x10..0x30].copy_from_slice(&blake3_hash(&junk));
        bad.extend_from_slice(&junk);
        assert!(matches!(decode_bookmark_section(&bad), Err(BookmarkSectionError::BadPayload { .. })));
    }
}
