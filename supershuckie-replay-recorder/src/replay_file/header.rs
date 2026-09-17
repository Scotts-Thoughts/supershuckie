use crate::util::{reinterpret_ref, MaybeEnum};
use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::String;
use core::ffi::CStr;
use num_enum::{IntoPrimitive, TryFromPrimitive};
use crate::replay_file::record::ResumeCropPolicy;
use crate::{TimestampMillis, UnsignedInteger};

/// Signature start (all replay headers must start with this)
pub const SIGNATURE_START: [u8; 4] = 0x4E49444Fu32.to_be_bytes();

/// Signature end (all replay headers must end with this)
pub const SIGNATURE_END: [u8; 4] = 0x52494E41u32.to_be_bytes();

/// Replay format version (minimum supported)
pub const REPLAY_VERSION_MINIMUM_SUPPORTED: u32 = 2;

/// Replay format version (written version)
///
/// * v2: original format.
/// * v3: `KeyframeMetadata` gained `counters`.
/// * v4: keyframe deltas are `RegionDeltaKeyframe` packets (run-length region diffs) and delta
///   chains run for minutes rather than ~2 minutes; no new header fields (packets self-describe),
///   but the delta chains are only affordable with a player that materialises deltas lazily, so
///   older builds refuse v4 files.
/// * v5: bookmarks are a [`BookmarkTable`](crate::BookmarkTable): the header gains
///   [`ReplayHeaderRaw::packet_stream_end`], a closed file ends with an editable bookmark section
///   (see [`bookmark_section`](crate::replay_file::bookmark_section)), and the stream carries
///   `BookmarkTable` snapshots instead of `Bookmark` packets. A v3/v4 file is upgraded in place the
///   first time its bookmarks are edited; its packets parse identically under a v5 header.
pub const REPLAY_VERSION: u32 = 5;

/// First format version with [`ReplayHeaderRaw::packet_stream_end`] and a bookmark section.
pub const REPLAY_VERSION_BOOKMARK_SECTION: u32 = 5;

/// Oldest format version whose packets are encoded the way this build writes them. Re-encoding a
/// file of this version or newer gains nothing (v5 only added bookmark storage, which a v4 file
/// gets in place on its first bookmark edit).
pub const REPLAY_VERSION_CURRENT_ENCODING: u32 = 4;

// Resume support: see replay_file::record::resume (build_resumed_recorder). A resumed file is an
// ordinary file of the current version. Blobs copied verbatim from the source keep the source's
// packet encoding (v3 DeltaKeyframes stay v3), which every current reader accepts.

/// Blake3 checksum
pub type ReplayHeaderBlake3Hash = [u8; 32];

/// Convert the hash to an uppercase ASCII string (uppercase) for displaying.
pub fn blake3_hash_to_ascii(hash: ReplayHeaderBlake3Hash) -> String {
    let mut ascii = String::with_capacity(64);

    for b in hash {
        let high = b >> 4;
        let low = b & 0xF;

        fn get_char(b: u8) -> char {
            if b <= 0x9 {
                (b'0' + b) as char
            }
            else {
                (b'A' + (b - 0xA)) as char
            }
        }

        ascii.push(get_char(high));
        ascii.push(get_char(low));
    }

    ascii
}

/// UTF-8 null-terminated 255 byte length string
pub type ReplayHeaderString = [u8; 256];

/// Raw replay header, mapping directly to the actual file.
#[derive(Copy, Clone, PartialEq, Debug)]
#[repr(C, packed(1))]
pub struct ReplayHeaderRaw {
    /// 0x000 - signature (must equal [`SIGNATURE_START`])
    pub signature_start: [u8; 4],

    /// 0x004 - replay format version
    pub replay_version: u32,

    /// 0x008 - type of the console
    pub console_type: MaybeEnum<ReplayConsoleType>,

    /// 0x00C - non-zero if crop_start_* and crop_timer_offset are valid
    ///
    /// This is a `u8`, not a `bool`, because [`Self::from_bytes`] transmutes raw file bytes: a
    /// `bool` field would be undefined behaviour for any byte value other than 0 or 1 (which a
    /// corrupt or hand-edited file cannot be assumed to avoid). Treat any non-zero value as set.
    pub crop_start: u8,

    /// 0x00D - non-zero if crop_end_* are valid (see [`Self::crop_start`] for why this is a `u8`)
    pub crop_end: u8,

    /// 0x00E - padding
    pub _padding_0: [u8; 2],

    /// 0x010 name of the emulator core, including version
    pub emulator_core_name: ReplayHeaderString,

    /// 0x110 patch data length
    pub patch_data_length: u64,

    /// 0x118 - patch format of the ROM
    pub patch_format: MaybeEnum<ReplayPatchFormat>,

    /// 0x11C - padding
    pub _padding_1: [u8; 4],

    /// 0x120 - blake3 hash of the unpatched ROM
    pub patch_target_checksum: ReplayHeaderBlake3Hash,

    /// 0x140 internal name of the ROM
    pub rom_name: ReplayHeaderString,

    /// 0x240 - filename of the ROM
    pub rom_filename: ReplayHeaderString,

    /// 0x340 - blake3 hash of the ROM (after all patches are applied, if any)
    pub rom_checksum: ReplayHeaderBlake3Hash,

    /// 0x360 - blake3 hash of the BIOS
    pub bios_checksum: ReplayHeaderBlake3Hash,

    /// 0x380 - crop range begin (frame index)
    pub crop_start_frame: UnsignedInteger,

    /// 0x388 - crop range begin (milliseconds)
    pub crop_start_millis: TimestampMillis,

    /// 0x390 - crop range end (frame index)
    pub crop_end_frame: UnsignedInteger,

    /// 0x398 - crop range end (milliseconds)
    pub crop_end_millis: TimestampMillis,

    /// 0x3A0 - crop timer offset
    pub crop_timer_offset: TimestampMillis,

    /// 0x3A8 - absolute offset where the packet stream ends and the bookmark section begins
    /// (format v5+). 0 means the packets run to the end of the file: a recording that was never
    /// closed, or a file older than v5. See [`Self::packet_stream_end`].
    pub packet_stream_end: u64,

    /// 0x3B0 - padding
    pub _padding_2: [u8; 0x7FC - 0x3B0],

    /// 0x7FC - signature (must equal [`SIGNATURE_END`])
    pub signature_end: [u8; 4],
}

/// Exactly enough bytes to hold [`ReplayHeaderRaw`] in binary form.
pub type ReplayHeaderBytes = [u8; 2048];

// Ensure that we can safely transmute between the two.
const _: () = assert!(size_of::<ReplayHeaderRaw>() == size_of::<ReplayHeaderBytes>());
const _: () = assert!(core::mem::offset_of!(ReplayHeaderRaw, crop_timer_offset) == 0x3A0);
const _: () = assert!(core::mem::offset_of!(ReplayHeaderRaw, packet_stream_end) == 0x3A8);

/// Metadata to generate a replay file.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct ReplayFileMetadata {
    /// Console type
    pub console_type: ReplayConsoleType,

    /// Internal ROM name (max length is 255 bytes)
    pub rom_name: String,

    /// Internal ROM filename (max length is 255 bytes)
    pub rom_filename: String,

    /// blake3 hash of the ROM (after patch)
    pub rom_checksum: ReplayHeaderBlake3Hash,

    /// blake3 hash of the BIOS
    pub bios_checksum: ReplayHeaderBlake3Hash,

    /// Name of the emulator core, including version (max length is 255 bytes)
    ///
    /// If this does not match exactly, it is recommended to warn before proceeding.
    pub emulator_core_name: String,

    /// Patch format to use
    pub patch_format: ReplayPatchFormat,

    /// blake3 hash of the target ROM (before patch)
    pub patch_target_checksum: ReplayHeaderBlake3Hash,

    /// crop start
    pub crop_start: Option<(UnsignedInteger, TimestampMillis)>,

    /// crop end
    pub crop_end: Option<(UnsignedInteger, TimestampMillis)>,

    /// timer offset
    pub timer_offset: Option<TimestampMillis>
}

impl ReplayHeaderRaw {
    /// Reinterpret the header as bytes.
    pub fn as_bytes(&self) -> &ReplayHeaderBytes {
        // SAFETY: ReplayHeaderRaw is safe to transmute to/from ReplayHeaderBytes (and intended to be done so)
        unsafe { reinterpret_ref(self) }
    }
    /// Reinterpret bytes as a raw header.
    pub fn from_bytes(bytes: &ReplayHeaderBytes) -> &ReplayHeaderRaw {
        // SAFETY: ReplayHeaderRaw is safe to transmute to/from ReplayHeaderBytes (and intended to be done so)
        //
        // Of course, there is no guarantee that we're going to get anything valid out of this,
        // but that's not UB.
        unsafe { reinterpret_ref(bytes) }
    }
    /// Where the packet stream ends and the bookmark section begins, if this header records it
    /// (format v5+ and the file was closed).
    pub fn packet_stream_end(&self) -> Option<u64> {
        let end = self.packet_stream_end;
        (self.replay_version >= REPLAY_VERSION_BOOKMARK_SECTION && end != 0).then_some(end)
    }

    /// Whether two headers describe the same replay, ignoring the fields a bookmark edit rewrites
    /// (`replay_version` and `packet_stream_end`).
    pub fn same_replay_as(&self, other: &ReplayHeaderRaw) -> bool {
        let mut a = *self;
        let mut b = *other;
        a.replay_version = 0;
        b.replay_version = 0;
        a.packet_stream_end = 0;
        b.packet_stream_end = 0;
        a.as_bytes() == b.as_bytes()
    }

    /// Parse the header.
    /// 
    /// Returns an error with a description if it is invalid.
    pub fn parse(&self) -> Result<ReplayFileMetadata, String> {
        let signature_start = self.signature_start;
        let signature_end = self.signature_end;
        let replay_version = self.replay_version;

        if signature_start != SIGNATURE_START {
            return Err(format!("Unrecognized signature_start {signature_start:X?}"));
        }
        if signature_end != SIGNATURE_END {
            return Err(format!("Unrecognized signature_end {signature_end:X?}"));
        }
        if self.replay_version < REPLAY_VERSION_MINIMUM_SUPPORTED || self.replay_version > REPLAY_VERSION {
            return Err(format!("Unrecognized replay format version {replay_version} (not in {REPLAY_VERSION_MINIMUM_SUPPORTED}..={REPLAY_VERSION})"));
        }

        fn parse_string_buffer(what: &ReplayHeaderString, name: &str) -> Result<String, String> {
            CStr::from_bytes_until_nul(what.as_slice())
                .map_err(|_| format!("{name} length exceeds 255 bytes"))?
                .to_str()
                .map_err(|_| format!("{name} is non-UTF-8 (cannot parse)"))
                .map(|s| s.to_owned())
        }

        Ok(ReplayFileMetadata {
            console_type: self.console_type.get().map_err(|i| format!("Unrecognized console_type 0x{i:08X}"))?,
            patch_format: self.patch_format.get().map_err(|i| format!("Unrecognized patch_format 0x{i:08X}"))?,

            bios_checksum: self.bios_checksum,
            rom_checksum: self.rom_checksum,
            patch_target_checksum: self.patch_target_checksum,

            rom_name: parse_string_buffer(&self.rom_name, "rom_name")?,
            rom_filename: parse_string_buffer(&self.rom_filename, "rom_filename")?,
            emulator_core_name: parse_string_buffer(&self.emulator_core_name, "emulator_core_name")?,

            crop_start: (self.crop_start != 0).then_some((self.crop_start_frame, self.crop_start_millis)),
            crop_end: (self.crop_end != 0).then_some((self.crop_end_frame, self.crop_end_millis)),
            timer_offset: (self.crop_start != 0).then_some(self.crop_timer_offset)
        })
    }
}

impl ReplayFileMetadata {
    /// Convert the parsed header into a raw header.
    pub fn as_raw_header(&self) -> Result<ReplayHeaderRaw, String> {
        fn into_str_bytes(what: &str, name: &'static str) -> Result<ReplayHeaderString, String> {
            let mut result = [0u8; 256];
            let limit = result.len() - 1;
            let result_minus_null_termination = &mut result[0..limit];
            let what_bytes = what.as_bytes();
            
            result_minus_null_termination.get_mut(0..what_bytes.len())
                .ok_or_else(|| format!("{name} exceeds {limit} bytes"))?
                .copy_from_slice(what_bytes);

            Ok(result)
        }

        Ok(ReplayHeaderRaw {
            signature_start: SIGNATURE_START,
            replay_version: REPLAY_VERSION,
            console_type: MaybeEnum::new(self.console_type),
            rom_name: into_str_bytes(&self.rom_name, "rom_name")?,
            rom_filename: into_str_bytes(&self.rom_filename, "rom_filename")?,
            rom_checksum: self.rom_checksum,
            bios_checksum: self.bios_checksum,
            emulator_core_name: into_str_bytes(&self.emulator_core_name, "emulator_core_name")?,
            patch_format: MaybeEnum::new(self.patch_format),
            patch_data_length: 0,
            patch_target_checksum: self.patch_target_checksum,
            signature_end: SIGNATURE_END,

            crop_start_frame: self.crop_start.map(|i| i.0).unwrap_or(0),
            crop_end_frame: self.crop_end.map(|i| i.0).unwrap_or(0),
            crop_start_millis: self.crop_start.map(|i| i.1).unwrap_or(0.into()),
            crop_end_millis: self.crop_end.map(|i| i.1).unwrap_or(0.into()),
            crop_timer_offset: self.timer_offset.unwrap_or(0.into()),

            crop_start: u8::from(self.crop_start.is_some()),
            crop_end: u8::from(self.crop_end.is_some()),

            packet_stream_end: 0,

            _padding_0: [0u8; _],
            _padding_1: [0u8; _],
            _padding_2: [0u8; _]
        })
    }

    /// Apply a [`ResumeCropPolicy`] to this metadata's crop / timing markers for a resume at
    /// `resume_frame`, returning the adjusted metadata.
    pub fn with_resume_crop(mut self, resume_frame: UnsignedInteger, policy: ResumeCropPolicy) -> Self {
        match policy {
            ResumeCropPolicy::PreserveStartDropEnd => {
                let keep_start = self.crop_start.map(|(frame, _)| frame <= resume_frame).unwrap_or(false);
                if !keep_start {
                    self.crop_start = None;
                    self.timer_offset = None;
                }
                self.crop_end = None;
            }
            ResumeCropPolicy::DropAll => {
                self.crop_start = None;
                self.crop_end = None;
                self.timer_offset = None;
            }
            ResumeCropPolicy::PreserveAll => {}
        }
        self
    }
}

/// Console type to use for replays.
#[derive(Copy, Clone, PartialEq, Debug, TryFromPrimitive, Default, IntoPrimitive)]
#[repr(u32)]
pub enum ReplayConsoleType {
    /// This is valid, but the user should probably not accept such a replay.
    #[default]
    Unknown,

    /// Game Boy
    GameBoy,

    /// Super Game Boy 2
    SuperGameBoy2,

    /// Game Boy Color
    GameBoyColor,

    /// Game Boy Advance
    GameBoyAdvance,

    /// Nintendo DS
    NintendoDS
}

impl ReplayConsoleType {
    /// Get the console name in human readable format
    pub const fn name(self) -> &'static str {
        match self {
            Self::Unknown => "Unknown",
            Self::GameBoy => "Game Boy",
            Self::SuperGameBoy2 => "Super Game Boy 2",
            Self::GameBoyColor => "Game Boy Color",
            Self::GameBoyAdvance => "Game Boy Advance",
            Self::NintendoDS => "Nintendo DS"
        }
    }
}

impl core::fmt::Display for ReplayConsoleType {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.name())
    }
}

/// Determines what patch format to use.
#[derive(Copy, Clone, PartialEq, Debug, TryFromPrimitive, Default, IntoPrimitive)]
#[repr(u32)]
pub enum ReplayPatchFormat {
    /// The ROM is unpatched
    #[default]
    Unpatched,

    /// The patch is in BPS format
    BPS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_bytes(crop_start_byte: u8) -> ReplayHeaderBytes {
        let mut bytes = [0u8; 2048];
        bytes[0..4].copy_from_slice(&SIGNATURE_START);
        bytes[4..8].copy_from_slice(&REPLAY_VERSION.to_le_bytes());
        bytes[0x0C] = crop_start_byte;
        bytes[2044..2048].copy_from_slice(&SIGNATURE_END);
        bytes
    }

    /// The raw `crop_start` byte is a flag (any non-zero value means "set"), not a literal `bool`
    /// (which would be undefined behaviour to read from an arbitrary file byte via transmute).
    #[test]
    fn crop_start_byte_is_treated_as_a_flag_not_a_literal_bool() {
        for (byte, expect_some) in [(0x7Fu8, true), (0u8, false), (1u8, true), (0xFFu8, true)] {
            let bytes = header_bytes(byte);
            let header = ReplayHeaderRaw::from_bytes(&bytes);
            let parsed = header.parse().unwrap_or_else(|e| panic!("byte {byte:#X}: {e}"));
            assert_eq!(parsed.crop_start.is_some(), expect_some, "byte {byte:#X}");
            assert_eq!(parsed.timer_offset.is_some(), expect_some, "byte {byte:#X}");
        }
    }

    /// `as_raw_header` always normalizes a set flag to exactly `1`.
    #[test]
    fn as_raw_header_writes_exactly_one_for_a_set_flag() {
        let metadata = ReplayFileMetadata { crop_start: Some((5, 10.into())), timer_offset: Some(1.into()), ..Default::default() };
        let raw = metadata.as_raw_header().unwrap();
        assert_eq!(raw.crop_start, 1);
        assert_eq!(raw.crop_end, 0);
    }
}
