use tinyvec::TinyVec;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::{Display, Formatter};
use core::num::NonZeroU16;
use core::cmp::Ordering;

mod io;
pub use io::*;

#[allow(missing_docs)]
pub type InputBuffer = TinyVec<[u8; 16]>;
#[allow(missing_docs)]
pub type UnsignedInteger = u64;
#[allow(missing_docs)]
pub type SignedInteger = i64;

#[derive(Copy, Clone, PartialEq, Debug, Default, Ord, Eq)]
#[repr(transparent)]
#[allow(missing_docs)]
pub struct TimestampMillis(pub UnsignedInteger);

impl PartialOrd for TimestampMillis {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.0.cmp(&other.0))
    }
}

impl core::fmt::Display for TimestampMillis {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(&self.0, f)
    }
}

impl From<u64> for TimestampMillis {
    #[inline]
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl PacketIO<'_> for TimestampMillis {
    fn write_packet_instructions(&self) -> PacketInstructionsVec<'_> {
        self.0.write_packet_instructions()
    }
    fn read_all(from: &mut &[u8], version: u32) -> Result<Self, PacketReadError> {
        Ok(Self(UnsignedInteger::read_all(from, version)?))
    }
}

#[allow(missing_docs)]
pub type ByteVec = TinyVec<[u8; 16]>;

/// Describes an individual packet.
#[derive(Clone, PartialEq, Debug)]
pub enum Packet {
    /// Do nothing
    NoOp,

    /// Run emulator for one frame.
    /// 
    /// `timestamp_delta` is the time passed since the last frame.
    #[allow(missing_docs)]
    NextFrame { timestamp_delta: TimestampMillis },

    /// Write RAM to the given address
    /// 
    /// How the address is interpreted is emulator-specific
    #[allow(missing_docs)]
    WriteMemory { address: UnsignedInteger, data: ByteVec },

    /// Set the current input.
    #[allow(missing_docs)]
    ChangeInput { data: InputBuffer },

    /// Set the current speed.
    #[allow(missing_docs)]
    ChangeSpeed { speed: Speed },

    /// Hard reset the console.
    ResetConsole,

    /// Load a save state.
    #[allow(missing_docs)]
    LoadSaveState { state: ByteVec },

    /// Describes a named point in the replay (format v2-v4; v5 writes [`Packet::BookmarkTable`]).
    #[allow(missing_docs)]
    Bookmark { metadata: BookmarkMetadata },

    /// Every bookmark of the replay as of this point in the stream (format v5).
    ///
    /// Written whenever the bookmarks change while recording and after the first keyframe of every
    /// blob once any were written, so a file that was never closed recovers its newest table from
    /// its uncompressed tail or its last blob. A closed file's bookmark section takes precedence.
    #[allow(missing_docs)]
    BookmarkTable { table: crate::BookmarkTable },

    /// Adds a keyframe so the replay can be scanned faster.
    #[allow(missing_docs)]
    Keyframe {
        metadata: KeyframeMetadata,
        state: ByteVec
    },

    /// Adds a diffed keyframe so the replay can be scanned faster (format v2/v3 encoding).
    ///
    /// The player materialises these into regular keyframes on demand using
    /// [`apply_diff`](crate::util::apply_diff); readers never see one. New files use
    /// [`Packet::RegionDeltaKeyframe`] instead.
    #[allow(missing_docs)]
    DeltaKeyframe {
        metadata: KeyframeMetadata,
        diff: Vec<UnsignedInteger>
    },

    /// A keyframe stored as a [region diff](crate::util::region_diff) against the previous
    /// keyframe in the same chain (top-level stream or blob); format v4.
    ///
    /// The player materialises it on demand (see `playback::ChainState`), so readers only ever
    /// see [`Packet::Keyframe`]. `state_len` is the length of the materialised state and lets the
    /// reader validate the delta before applying it.
    #[allow(missing_docs)]
    RegionDeltaKeyframe {
        metadata: KeyframeMetadata,
        state_len: UnsignedInteger,
        control: ByteVec,
        data: ByteVec
    },

    /// Describes a compressed blob of memory.
    ///
    /// `keyframe_offsets`, when not empty, is parallel to `keyframes`: the byte offset in the
    /// decompressed blob of each keyframe-class packet (format v6, written with the
    /// `IndexedCompressedBlob` discriminator). It lets a reader decompress a blob only as far as
    /// the keyframe it seeks and parse just the keyframe packets on the way there. Empty for blobs
    /// written before v6 (or copied verbatim from such a file); the reader then scans the whole
    /// decompressed blob once to build the same table.
    #[allow(missing_docs)]
    CompressedBlob {
        keyframes: Vec<KeyframeMetadata>,
        keyframe_offsets: Vec<UnsignedInteger>,
        bookmarks: Vec<BookmarkMetadata>,
        compressed_data: ByteVec,
        uncompressed_size: UnsignedInteger,
        timestamp_start: TimestampMillis,
        timestamp_end: TimestampMillis,
        elapsed_frames_start: UnsignedInteger,
        elapsed_frames_end: UnsignedInteger
    },

    /// Modifies a counter
    #[allow(missing_docs)]
    IncrementCounter {
        name: String,
        delta: SignedInteger
    },

    /// A keyframe of a Nintendo 3DS file (format v8), whose bytes stay in the file until needed.
    ///
    /// The packet is followed in the stream by `frame_len` bytes: one zstd frame that decodes to
    /// `uncompressed_len` bytes, which for `level` 0 is the whole state (`state_len` bytes) and
    /// for levels 1 and 2 a [region diff](crate::util::region_diff_resizing) (`u64` control
    /// length, control, data) against the state after the most recent keyframe of a level at
    /// most `level`, compressed with that keyframe's own decoded payload as zstd prefix. The
    /// reader skips the frame when parsing and notes where it is (`frame_offset`, which is not
    /// stored), so a 4 GB file does not have to be in memory; see `replay-3ds-spec.md` §7.
    #[allow(missing_docs)]
    StoredKeyframe {
        metadata: KeyframeMetadata,
        level: u8,
        state_len: UnsignedInteger,
        uncompressed_len: UnsignedInteger,
        frame_len: UnsignedInteger,
        frame_offset: UnsignedInteger
    },

    /// A small picture of both screens at `elapsed_frames` (Nintendo 3DS files, once a second):
    /// what the timeline shows while it is dragged, so a drag never has to seek (a seek costs up
    /// to a second there). `top`/`bottom` are zstd frames of RGB565 pixels, row-major.
    #[allow(missing_docs)]
    Thumbnail {
        elapsed_frames: UnsignedInteger,
        top_width: UnsignedInteger,
        top_height: UnsignedInteger,
        bottom_width: UnsignedInteger,
        bottom_height: UnsignedInteger,
        top: ByteVec,
        bottom: ByteVec
    },

    /// Everything the console received over its link cable during the frame that the next
    /// [`Packet::NextFrame`] closes (format v7), so that a replay of a linked game reproduces the
    /// transfer without the other console. The bytes are console-specific (see the core's
    /// `emulator::link` module); a frame with nothing received writes no packet. At most
    /// [`MAX_SERIAL_IN_BYTES`] bytes.
    #[allow(missing_docs)]
    SerialIn { data: ByteVec }
}

/// Longest `SerialIn` payload accepted when reading (a frame of link traffic is a few kilobytes
/// at most; anything larger is a corrupt or hostile file).
pub const MAX_SERIAL_IN_BYTES: usize = 64 * 1024;

/// Speed value that uses a fixed point number.
#[derive(Copy, Clone, Debug, PartialEq)]
#[repr(transparent)]
pub struct Speed {
    /// A fixed point number that, when divided by 256, will yield the speed value.
    pub speed_over_256: NonZeroU16
}

impl Speed {
    /// Get the speed value from a multiplier.
    pub const fn from_multiplier_float(multiplier: f64) -> Self {
        Self {
            speed_over_256: match NonZeroU16::new((multiplier * 256.0) as u16) {
                Some(n) => n,
                None => NonZeroU16::new(1).expect("1 is not 0")
            }
        }
    }
    /// Convert the speed value into a multiplier.
    pub const fn into_multiplier_float(self) -> f64 {
        (self.speed_over_256.get() as f64) / 256.0
    }
}

impl Default for Speed {
    fn default() -> Self {
        Self::from_multiplier_float(1.0)
    }
}

impl Display for Speed {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        Display::fmt(&self.into_multiplier_float(), f)
    }
}

/// Payload for keyframes, not including save state data
#[derive(Clone, PartialEq, Debug, Default)]
pub struct KeyframeMetadata {
    /// Current input
    pub input: InputBuffer,

    /// Current speed
    pub speed: Speed,

    /// Number of elapsed frames
    pub elapsed_frames: UnsignedInteger,

    /// Total elapsed milliseconds
    pub elapsed_millis: TimestampMillis,

    /// Counters thus far
    pub counters: Vec<Counter>
}

/// Payload for bookmarks
#[derive(Clone, PartialEq, Debug, Default)]
pub struct BookmarkMetadata {
    /// Name of the bookmark
    pub name: String,

    /// Number of elapsed frames
    pub elapsed_frames: UnsignedInteger,

    /// Total elapsed milliseconds
    pub elapsed_millis: TimestampMillis
}

/// Counter data
#[derive(Clone, PartialEq, Debug, Default)]
pub struct Counter {
    /// Name of the counter.
    pub name: String,

    /// Value of the counter
    pub value: SignedInteger
}
