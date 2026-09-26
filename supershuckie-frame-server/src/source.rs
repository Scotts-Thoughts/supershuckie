//! What a replay says about itself, read from its header and index only.

use std::path::Path;

use supershuckie_core::ScreenLayout;
use supershuckie_replay_recorder::replay_file::playback::{ReplayFilePlayer, ReplayFileReadError};
use supershuckie_replay_recorder::replay_file::ReplayConsoleType;
use supershuckie_replay_recorder::BookmarkTable;

/// Everything `--probe` prints and `Info` carries that does not need a core.
#[derive(Clone, Debug)]
pub struct ReplaySummary {
    pub console: ReplayConsoleType,
    pub frames: u64,
    /// Frame indices of every keyframe, ascending.
    pub keyframes: Vec<u64>,
    /// `(name, in frame)`, in frame order.
    pub bookmarks: Vec<(String, u64)>,
    /// Every bookmark with its out frame, type and keyframe flag (for `--probe`).
    pub bookmark_table: BookmarkTable,
    /// `(start, end)` frame indices of the marked range, when both marks are set.
    pub crop: Option<(u64, u64)>,
    /// Counters as of the last keyframe, in name order.
    pub counters: Vec<(String, i64)>,
    pub rom_checksum: [u8; 32],
    pub rom_filename: String,
    pub rom_name: String,
    pub core_recorded: String,
    pub format_version: u32,
}

impl ReplaySummary {
    pub fn of(player: &ReplayFilePlayer) -> Self {
        let metadata = player.get_replay_metadata();

        let keyframes: Vec<u64> = player.all_keyframes().keys().copied().collect();

        // The bookmark section of a closed v5 file, the stream's newest snapshot, or a pre-v5 file's
        // bookmark packets; the section and the legacy index need no decompression.
        let bookmark_table = player.bookmark_table().clone();
        let mut bookmarks: Vec<(String, u64)> = bookmark_table
            .bookmarks
            .iter()
            .map(|b| (b.name.clone(), b.in_frame))
            .collect();
        bookmarks.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));

        // The keyframe index carries each keyframe's metadata (counters included) whether or not
        // its state is inside a compressed blob, so the last one is free to read.
        let counters: Vec<(String, i64)> = player
            .all_keyframes()
            .iter()
            .next_back()
            .and_then(|(_, list)| list.last())
            .map(|k| k.counters.iter().map(|c| (c.name.clone(), c.value)).collect())
            .unwrap_or_default();

        let crop = match (metadata.crop_start, metadata.crop_end) {
            (Some((start, _)), Some((end, _))) => Some((start, end)),
            _ => None,
        };

        Self {
            console: metadata.console_type,
            frames: player.get_total_frames(),
            keyframes,
            bookmarks,
            bookmark_table,
            crop,
            counters,
            rom_checksum: metadata.rom_checksum,
            rom_filename: metadata.rom_filename.clone(),
            rom_name: metadata.rom_name.clone(),
            core_recorded: metadata.emulator_core_name.clone(),
            format_version: player.get_replay_version(),
        }
    }

    pub fn rom_hash_hex(&self) -> String {
        self.rom_checksum.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// The composited picture size for `console` under `layout`, without a core.
///
/// Matches `supershuckie_core::export::output_geometry` on the screens each core exposes.
pub fn geometry(console: ReplayConsoleType, layout: ScreenLayout) -> (u32, u32) {
    match console {
        ReplayConsoleType::GameBoy | ReplayConsoleType::GameBoyColor | ReplayConsoleType::SuperGameBoy2 => (160, 144),
        ReplayConsoleType::GameBoyAdvance => (240, 160),
        ReplayConsoleType::NintendoDS => match layout {
            ScreenLayout::VerticalStack => (256, 384),
            ScreenLayout::HorizontalStack => (512, 192),
            ScreenLayout::TopOnly | ScreenLayout::BottomOnly => (256, 192),
        },
        // 400x240 over a 320x240 screen; the stack takes the wider one's width.
        ReplayConsoleType::Nintendo3DS => match layout {
            ScreenLayout::VerticalStack => (400, 480),
            ScreenLayout::HorizontalStack => (720, 240),
            ScreenLayout::TopOnly => (400, 240),
            ScreenLayout::BottomOnly => (320, 240),
        },
        ReplayConsoleType::Unknown => (0, 0),
    }
}

/// The native picture rate of `console` as a rational; the values `EmulatorCore::frame_rate`
/// reports for each core.
pub fn frame_rate(console: ReplayConsoleType) -> (u32, u32) {
    match console {
        ReplayConsoleType::GameBoy
        | ReplayConsoleType::GameBoyColor
        | ReplayConsoleType::SuperGameBoy2
        | ReplayConsoleType::GameBoyAdvance => (4194304, 70224),
        ReplayConsoleType::NintendoDS => (33513982, 560190),
        ReplayConsoleType::Nintendo3DS => (268111856, 4481136),
        ReplayConsoleType::Unknown => (60, 1),
    }
}

/// The protocol's `layout` byte: 0 stacked, 1 side by side, 2 first only, 3 second only.
pub fn layout_from_byte(layout: u8) -> ScreenLayout {
    match layout {
        1 => ScreenLayout::HorizontalStack,
        2 => ScreenLayout::TopOnly,
        3 => ScreenLayout::BottomOnly,
        _ => ScreenLayout::VerticalStack,
    }
}

/// Read and parse a replay. Blobs stay compressed; nothing is emulated.
pub fn open_replay(path: &Path) -> Result<ReplayFilePlayer, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    ReplayFilePlayer::new(&bytes, true).map_err(|e| format!("{} is not a usable replay: {}", path.display(), describe(&e)))
}

fn describe(e: &ReplayFileReadError) -> String {
    match e {
        ReplayFileReadError::InvalidReplayFile { explanation } => format!("invalid replay file ({explanation})"),
        ReplayFileReadError::BrokenPacket { explanation } => format!("broken packet ({explanation})"),
        ReplayFileReadError::EndOfStream => "unexpected end of stream".to_string(),
        ReplayFileReadError::Other { explanation } => explanation.to_string(),
    }
}

/// The consoles this build can emulate, by the names `--probe` and `Info` use for them.
pub fn console_names() -> Vec<String> {
    [
        ReplayConsoleType::GameBoy,
        ReplayConsoleType::SuperGameBoy2,
        ReplayConsoleType::GameBoyColor,
        ReplayConsoleType::GameBoyAdvance,
        ReplayConsoleType::NintendoDS,
    ]
    .iter()
    .map(|c| c.name().to_string())
    .collect()
}
