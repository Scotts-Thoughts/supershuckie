use crate::util::UTF8CString;
use crate::{SuperShuckieEmulatorType, SETTINGS_FILE};
use num_enum::TryFromPrimitive;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::CStr;
use std::fs;
use std::hint::unreachable_unchecked;
use std::num::{NonZeroIsize, NonZeroU32, NonZeroU64, NonZeroU8, NonZeroUsize};
use std::path::Path;
use std::path::PathBuf;
use supershuckie_core::emulator::Input;
use supershuckie_replay_recorder::replay_file::record::ReplayFileRecorderSettings;
use supershuckie_replay_recorder::Speed;

/// Name of the file a settings file that could not be read or parsed is copied to (next to the
/// original, which is left in place), in the config dir.
const BAD_SETTINGS_FILE: &str = "settings.json.bad";

/// Copy the unreadable settings file aside (best-effort) and fall back to default settings, with
/// a warning to surface on the first tick (C4).
fn settings_read_fallback(settings_path: &Path, config_dir: &Path, error: impl std::fmt::Display) -> (Settings, String) {
    let _ = fs::copy(settings_path, config_dir.join(BAD_SETTINGS_FILE));
    let mut settings = Settings::default();
    settings.clamp();
    (settings, format!("Your settings file could not be read and default settings are in use. The unreadable file was kept as {BAD_SETTINGS_FILE}. ({error})"))
}

/// Prepare the data and config directories and load settings from them. Never fails: directory
/// creation failures and an unreadable/unparsable settings file become warnings (returned
/// alongside the settings to use) instead of aborting startup (C4).
pub(crate) fn try_to_init_data_dir_and_get_settings(data_dir: &Path, config_dir: &Path) -> (Settings, Vec<String>) {
    let mut warnings = Vec::new();

    if !data_dir.exists() {
        if let Err(e) = fs::create_dir_all(data_dir) {
            warnings.push(format!("Failed to create the data directory {}: {e}", data_dir.display()));
        }
    }
    if !config_dir.exists() {
        if let Err(e) = fs::create_dir_all(config_dir) {
            warnings.push(format!("Failed to create the config directory {}: {e}", config_dir.display()));
        }
    }

    let settings_path = config_dir.join(SETTINGS_FILE);

    let settings_str = match fs::read_to_string(&settings_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            let (settings, warning) = settings_read_fallback(&settings_path, config_dir, e);
            warnings.push(warning);
            return (settings, warnings);
        }
    };

    let settings_str = if settings_str.trim().is_empty() { "{}".to_owned() } else { settings_str };

    let mut settings: Settings = match serde_json::from_str(&settings_str) {
        Ok(s) => s,
        Err(e) => {
            let (settings, warning) = settings_read_fallback(&settings_path, config_dir, e);
            warnings.push(warning);
            return (settings, warnings);
        }
    };

    settings.clamp();
    (settings, warnings)
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "ReplaySettings::default")]
    pub replay: ReplaySettings,

    #[serde(default = "RecentROMs::default")]
    pub recent_roms: RecentROMs,

    #[serde(default = "EmulationSettings::default")]
    pub emulation: EmulationSettings,

    #[serde(default = "GameBoySettings::default")]
    pub game_boy_settings: GameBoySettings,
    
    #[serde(default = "GameBoyAdvanceSettings::default")]
    pub game_boy_advance_settings: GameBoyAdvanceSettings,

    #[serde(default = "NintendoDSSettings::default")]
    pub nintendo_ds_settings: NintendoDSSettings,

    #[serde(default = "BTreeMap::default")]
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub rom_config: BTreeMap<String, ROMConfig>,

    #[serde(default = "PokeAByteSettings::default")]
    pub pokeabyte: PokeAByteSettings,

    #[serde(default = "SimpleEnabledByDefaultSettings::default")]
    pub external_commands: SimpleEnabledByDefaultSettings,

    #[serde(default = "ExportSettings::default")]
    pub export: ExportSettings,

    #[serde(default = "AudioSettings::default")]
    pub audio: AudioSettings,

    #[serde(default = "MemoryToolsSettings::default")]
    pub memory_tools: MemoryToolsSettings,

    #[serde(default = "BookmarkSettings::default")]
    pub bookmarks: BookmarkSettings,

    #[serde(default = "PlayTogetherSettings::default")]
    pub play_together: PlayTogetherSettings,

    #[serde(default = "BTreeMap::default")]
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub custom: BTreeMap<String, UTF8CString>
}

/// Play Together: playing alongside other players over the network.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct PlayTogetherSettings {
    /// The name other players see.
    #[serde(default = "PlayTogetherSettings::DEFAULT_DISPLAY_NAME")]
    pub display_name: String,

    /// The colour asked for when hosting or joining (a palette index; 0 = let the host pick).
    #[serde(default)]
    pub color: u8,

    /// The TCP port to host on.
    #[serde(default = "PlayTogetherSettings::DEFAULT_HOST_PORT")]
    pub host_port: u16,

    /// The address to host on: `0.0.0.0` (reachable from the network) unless changed.
    #[serde(default = "PlayTogetherSettings::DEFAULT_BIND_ADDRESS")]
    pub bind_address: String,

    /// The last code joined with, to prefill the dialog.
    #[serde(default = "String::new")]
    pub last_join_code: String,

    /// Write every other player's game to a replay file of its own.
    #[serde(default = "PlayTogetherSettings::DEFAULT_SAVE_PEER_REPLAYS")]
    pub save_peer_replays: bool,

    /// Display scale of the other players' windows.
    #[serde(default = "PlayTogetherSettings::DEFAULT_PEER_VIDEO_SCALE")]
    pub peer_video_scale: NonZeroU8,

    /// Let Nintendo DS games into sessions (unsupported for now: a state is 20 MB).
    #[serde(default = "PlayTogetherSettings::DEFAULT_ALLOW_NINTENDO_DS")]
    pub allow_nintendo_ds: bool,

    /// Sync pause: when anyone pauses, everyone's game pauses. The host's setting applies to the
    /// whole session; a client's copy only matters for sessions it hosts later.
    #[serde(default = "PlayTogetherSettings::DEFAULT_SYNC_PAUSE")]
    pub sync_pause: bool,

    /// The link cable's input delay in frames: 0 picks it from the round-trip times, else this
    /// many at least (the larger of the two players' settings wins).
    #[serde(default)]
    pub link_input_delay: u8,

    /// Plug in without asking when another player requests a link cable (for unattended
    /// sessions and the smoke test).
    #[serde(default)]
    pub link_auto_accept: bool,

    /// ROMs by blake3 hash (lowercase hex), so another player's ROM can be found on this
    /// machine without asking. Learned from every ROM loaded or located.
    #[serde(default = "BTreeMap::default")]
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub known_roms: BTreeMap<String, UTF8CString>
}

impl PlayTogetherSettings {
    const DEFAULT_DISPLAY_NAME: fn() -> String = || String::from("Player");
    const DEFAULT_HOST_PORT: fn() -> u16 = || 30170;
    const DEFAULT_BIND_ADDRESS: fn() -> String = || String::from("0.0.0.0");
    const DEFAULT_SAVE_PEER_REPLAYS: fn() -> bool = || true;
    const DEFAULT_PEER_VIDEO_SCALE: fn() -> NonZeroU8 = || unsafe { NonZeroU8::new_unchecked(2) };
    const DEFAULT_ALLOW_NINTENDO_DS: fn() -> bool = || false;
    const DEFAULT_SYNC_PAUSE: fn() -> bool = || false;

    /// Longest display name kept.
    pub const MAX_DISPLAY_NAME_BYTES: usize = 32;

    /// Largest scale the other players' windows get.
    pub const MAX_PEER_VIDEO_SCALE: u8 = 12;

    /// Most ROM paths remembered by hash.
    pub const MAX_KNOWN_ROMS: usize = 64;

    /// Bring out-of-range values from the config file back into range.
    pub(crate) fn clamp(&mut self) {
        let mut name = self.display_name.trim().to_owned();
        name.retain(|c| !c.is_control());
        while name.len() > Self::MAX_DISPLAY_NAME_BYTES {
            name.pop();
        }
        self.display_name = if name.is_empty() { Self::DEFAULT_DISPLAY_NAME() } else { name };
        if !supershuckie_play_together::is_valid_color(self.color) {
            self.color = supershuckie_play_together::COLOR_RANDOM;
        }

        if self.host_port == 0 {
            self.host_port = Self::DEFAULT_HOST_PORT();
        }
        if self.bind_address.trim().parse::<std::net::IpAddr>().is_err() {
            self.bind_address = Self::DEFAULT_BIND_ADDRESS();
        }
        else {
            self.bind_address = self.bind_address.trim().to_owned();
        }
        if self.peer_video_scale.get() > Self::MAX_PEER_VIDEO_SCALE {
            self.peer_video_scale = NonZeroU8::new(Self::MAX_PEER_VIDEO_SCALE).unwrap();
        }
        if self.link_input_delay > supershuckie_play_together::MAX_LINK_DELAY {
            self.link_input_delay = supershuckie_play_together::MAX_LINK_DELAY;
        }
        while self.known_roms.len() > Self::MAX_KNOWN_ROMS {
            let first = self.known_roms.keys().next().cloned().expect("non-empty");
            self.known_roms.remove(&first);
        }
    }
}

impl Default for PlayTogetherSettings {
    fn default() -> Self {
        Self {
            display_name: Self::DEFAULT_DISPLAY_NAME(),
            color: supershuckie_play_together::COLOR_RANDOM,
            host_port: Self::DEFAULT_HOST_PORT(),
            bind_address: Self::DEFAULT_BIND_ADDRESS(),
            last_join_code: String::new(),
            save_peer_replays: Self::DEFAULT_SAVE_PEER_REPLAYS(),
            peer_video_scale: Self::DEFAULT_PEER_VIDEO_SCALE(),
            allow_nintendo_ds: Self::DEFAULT_ALLOW_NINTENDO_DS(),
            sync_pause: Self::DEFAULT_SYNC_PAUSE(),
            link_input_delay: 0,
            link_auto_accept: false,
            known_roms: BTreeMap::new()
        }
    }
}

/// Audio playback. Off by default: a fresh install plays nothing until the user turns it on.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioSettings {
    #[serde(default = "AudioSettings::DEFAULT_ENABLED")]
    pub enabled: bool,

    #[serde(default = "AudioSettings::DEFAULT_MUTED")]
    pub muted: bool,

    /// Percent, 0..=100.
    #[serde(default = "AudioSettings::DEFAULT_VOLUME")]
    pub volume: u8,

    /// Stay silent while the game runs at any speed other than 1x (turbo or a non-1x base
    /// speed), so alternating between sped-up and normal play is not a barrage of fast audio.
    #[serde(default = "AudioSettings::DEFAULT_MUTE_WHEN_SPED_UP")]
    pub mute_when_sped_up: bool,

    /// Most audio queued between the emulator and the device, in milliseconds.
    #[serde(default = "AudioSettings::DEFAULT_LATENCY_MS")]
    pub latency_ms: u16
}

impl AudioSettings {
    const DEFAULT_ENABLED: fn() -> bool = || false;
    const DEFAULT_MUTED: fn() -> bool = || false;
    const DEFAULT_VOLUME: fn() -> u8 = || 100;
    const DEFAULT_MUTE_WHEN_SPED_UP: fn() -> bool = || true;
    const DEFAULT_LATENCY_MS: fn() -> u16 = || 64;

    pub const MAX_VOLUME: u8 = 100;
    pub const MIN_LATENCY_MS: u16 = 16;
    pub const MAX_LATENCY_MS: u16 = 500;

    /// Bring out-of-range values from the config file back into range.
    pub(crate) fn clamp(&mut self) {
        self.volume = self.volume.min(Self::MAX_VOLUME);
        self.latency_ms = self.latency_ms.clamp(Self::MIN_LATENCY_MS, Self::MAX_LATENCY_MS);
    }
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            enabled: Self::DEFAULT_ENABLED(),
            muted: Self::DEFAULT_MUTED(),
            volume: Self::DEFAULT_VOLUME(),
            mute_when_sped_up: Self::DEFAULT_MUTE_WHEN_SPED_UP(),
            latency_ms: Self::DEFAULT_LATENCY_MS()
        }
    }
}

impl Settings {
    pub(crate) fn get_rom_config_or_default(&mut self, rom: &str) -> &mut ROMConfig {
        if !self.rom_config.contains_key(rom) {
            self.rom_config.insert(rom.to_owned(), ROMConfig::default());
        }
        self.rom_config.get_mut(rom).expect("we just added the rom??")
    }

    /// Bring out-of-range values loaded from (or set directly into) the settings file back into
    /// range, so a hand-edited or old config file cannot leave the app in a broken state (L13).
    pub(crate) fn clamp(&mut self) {
        self.audio.clamp();
        self.replay.zstd_compression_level = self.replay.zstd_compression_level.clamp(1, 22);

        // Same normalisation `set_speed_settings` applies: NaN/zero/negative collapse to the
        // minimum representable speed, and the value is otherwise snapped to the fixed-point grid
        // `Speed` stores it as.
        self.emulation.base_speed_multiplier = Speed::from_multiplier_float(self.emulation.base_speed_multiplier).into_multiplier_float();
        self.emulation.turbo_speed_multiplier = Speed::from_multiplier_float(self.emulation.turbo_speed_multiplier).into_multiplier_float();

        self.export.default_crf = self.export.default_crf.min(51);

        self.recent_roms.clamp();
        self.play_together.clamp();
        self.pokeabyte.clamp();
    }
}

/// Hard upper bound on how many entries the "Open recent ROM" menu shows.
pub const MAX_RECENT_ROMS: usize = 10;

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct RecentROMs {
    pub max_recent_roms: NonZeroIsize,
    pub recent_roms: Vec<UTF8CString>
}

impl Default for RecentROMs {
    fn default() -> Self {
        Self {
            max_recent_roms: unsafe { NonZeroIsize::new_unchecked(MAX_RECENT_ROMS as isize) },
            recent_roms: Vec::new()
        }
    }
}

impl RecentROMs {
    /// Cap `max_recent_roms` to [`MAX_RECENT_ROMS`] and truncate the list to match.
    pub(crate) fn clamp(&mut self) {
        if self.max_recent_roms.get() > MAX_RECENT_ROMS as isize {
            self.max_recent_roms = NonZeroIsize::new(MAX_RECENT_ROMS as isize).unwrap();
        }
        let max = self.max_recent_roms.get().max(0) as usize;
        self.recent_roms.truncate(max);
    }
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct ReplaySettings {
    /// Hard cap on the uncompressed bytes buffered per compressed blob while recording.
    ///
    /// Since format v4 this counts the actual (delta-encoded) bytes, so it is rarely the limit
    /// that closes a blob; `max_recording_blob_minutes` normally is.
    #[serde(default = "ReplaySettings::MAX_RECORDING_BLOB_SIZE_MB")]
    pub max_recording_blob_size_mb: NonZeroU32,

    /// Close a compressed blob (= one delta-keyframe chain) after this many minutes of recording.
    ///
    /// Every blob starts with a full keyframe, so longer chains make Nintendo DS replays smaller
    /// (on a 2h14 HeartGold replay: 15 min = 192 MiB, 30 min = 143 MiB, 60 min = 117 MiB) at the
    /// cost of a slower cold seek into the chain, which since format v6 decodes only up to the
    /// keyframe it wants (measured 2026-09-17 in the core: 15 min = 53 ms, 30 min = 68 ms,
    /// 60 min = 101 ms; a 15-minute v5 chain was 97 ms). A decoded chain also occupies about
    /// 3.3 MiB of RAM per minute. Default 30 (a settings file written by an older build keeps its
    /// own value, typically 15).
    #[serde(default = "ReplaySettings::MAX_RECORDING_BLOB_MINUTES")]
    pub max_recording_blob_minutes: NonZeroU32,

    /// Decompress every blob of a replay when it is opened instead of on demand.
    ///
    /// Decompressed blobs hold compact delta chains (about the compressed size of the file, not
    /// the sum of the keyframe states), so this is affordable even for long Nintendo DS replays.
    #[serde(default = "ReplaySettings::AUTO_DECOMPRESS_REPLAYS_UPFRONT")]
    pub auto_decompress_replays_upfront: bool,

    /// zstd compression level for recording. The default is 9 as of format v4 (it was 3); a value
    /// persisted from an older settings file is kept as-is.
    #[serde(default = "ReplaySettings::DEFAULT_MAX_ZSTD_COMPRESSION_LEVEL")]
    pub zstd_compression_level: i32,

    /// Leave regenerated output buffers (melonDS 3D vertex/polygon banks, mGBA m4a mixed PCM) out
    /// of delta keyframes. Cuts Nintendo DS replays by roughly another third and Game Boy Advance
    /// ones by half; the only effect is that a keyframe loaded from the middle of a chain holds the
    /// chain start's copy of those buffers until the game's next frame overwrites them, which the
    /// player never shows.
    #[serde(default = "ReplaySettings::MASK_TRANSIENT_BUFFERS")]
    pub mask_transient_buffers: bool,

    #[serde(default = "ReplaySettings::DEFAULT_FRAMES_PER_KEYFRAME")]
    pub frames_per_keyframe: NonZeroU64,

    #[serde(default = "ReplaySettings::AUTO_STOP_PLAYBACK_ON_INPUT")]
    pub auto_stop_playback_on_input: bool,

    #[serde(default = "ReplaySettings::AUTO_UNPAUSE_ON_INPUT")]
    pub auto_unpause_on_input: bool,

    #[serde(default = "ReplaySettings::AUTO_PAUSE_ON_RECORD")]
    pub auto_pause_on_record: bool,

    #[serde(default = "ReplaySettings::IGNORE_SPEED_CHANGES_IN_REPLAYS")]
    pub ignore_speed_changes_in_replays: bool,

    #[serde(default = "ReplaySettings::AUTO_RESYNC_KEYFRAMES_IN_REPLAYS")]
    pub auto_resync_keyframes_in_replays: bool,

    #[serde(default = "ReplaySettings::DISABLE_SAVE_STATES_WHEN_RECORDING")]
    pub disable_save_states_when_recording: bool,

    #[serde(default = "ReplaySettings::DISABLE_SPEED_CHANGES_WHEN_RECORDING")]
    pub disable_speed_changes_when_recording: bool,

    /// While the timeline is being dragged, show the nearest keyframe (at most one keyframe
    /// interval, normally 2 seconds, before the pointer) instead of emulating up to the exact
    /// frame on every mouse move; the exact frame is sought when the drag ends. Keeps a Nintendo
    /// DS drag at about 20 ms per step instead of up to 300 ms. Default true.
    #[serde(default = "ReplaySettings::SNAP_TIMELINE_DRAG_TO_KEYFRAMES")]
    pub snap_timeline_drag_to_keyframes: bool,
}

impl Default for ReplaySettings {
    fn default() -> Self {
        Self {
            max_recording_blob_size_mb: Self::MAX_RECORDING_BLOB_SIZE_MB(),
            max_recording_blob_minutes: Self::MAX_RECORDING_BLOB_MINUTES(),
            auto_decompress_replays_upfront: Self::AUTO_DECOMPRESS_REPLAYS_UPFRONT(),
            zstd_compression_level: Self::DEFAULT_MAX_ZSTD_COMPRESSION_LEVEL(),
            mask_transient_buffers: Self::MASK_TRANSIENT_BUFFERS(),
            frames_per_keyframe: Self::DEFAULT_FRAMES_PER_KEYFRAME(),
            auto_stop_playback_on_input: Self::AUTO_STOP_PLAYBACK_ON_INPUT(),
            auto_unpause_on_input: Self::AUTO_UNPAUSE_ON_INPUT(),
            auto_pause_on_record: Self::AUTO_PAUSE_ON_RECORD(),
            ignore_speed_changes_in_replays: Self::IGNORE_SPEED_CHANGES_IN_REPLAYS(),
            auto_resync_keyframes_in_replays: Self::AUTO_RESYNC_KEYFRAMES_IN_REPLAYS(),
            disable_save_states_when_recording: Self::DISABLE_SAVE_STATES_WHEN_RECORDING(),
            disable_speed_changes_when_recording: Self::DISABLE_SPEED_CHANGES_WHEN_RECORDING(),
            snap_timeline_drag_to_keyframes: Self::SNAP_TIMELINE_DRAG_TO_KEYFRAMES()
        }
    }
}

impl ReplaySettings {
    const MAX_RECORDING_BLOB_SIZE_MB: fn() -> NonZeroU32 = || unsafe { NonZeroU32::new_unchecked(
        (ReplayFileRecorderSettings::default().minimum_uncompressed_bytes_per_blob / 1024 / 1024) as u32
    ) };
    const MAX_RECORDING_BLOB_MINUTES: fn() -> NonZeroU32 = || unsafe { NonZeroU32::new_unchecked(
        (ReplayFileRecorderSettings::default().max_frames_per_blob / (60 * 60)).max(1) as u32
    ) };
    const AUTO_DECOMPRESS_REPLAYS_UPFRONT: fn() -> bool = || false;
    const DEFAULT_MAX_ZSTD_COMPRESSION_LEVEL: fn() -> i32 = || ReplayFileRecorderSettings::default().compression_level;
    const MASK_TRANSIENT_BUFFERS: fn() -> bool = || ReplayFileRecorderSettings::default().mask_transient_buffers;
    const DEFAULT_FRAMES_PER_KEYFRAME: fn() -> NonZeroU64 = || unsafe { NonZeroU64::new_unchecked(120) };
    const AUTO_STOP_PLAYBACK_ON_INPUT: fn() -> bool = || false;
    const AUTO_UNPAUSE_ON_INPUT: fn() -> bool = || false;
    const AUTO_PAUSE_ON_RECORD: fn() -> bool = || false;
    const IGNORE_SPEED_CHANGES_IN_REPLAYS: fn() -> bool = || false;
    const AUTO_RESYNC_KEYFRAMES_IN_REPLAYS: fn() -> bool = || false;
    const DISABLE_SAVE_STATES_WHEN_RECORDING: fn() -> bool = || false;
    const DISABLE_SPEED_CHANGES_WHEN_RECORDING: fn() -> bool = || false;
    const SNAP_TIMELINE_DRAG_TO_KEYFRAMES: fn() -> bool = || true;

    /// `max_recording_blob_minutes` as a frame count. The cap is coarse by design, so a nominal
    /// 60 fps is used for every console.
    pub fn max_frames_per_blob(&self) -> u64 {
        u64::from(self.max_recording_blob_minutes.get()).saturating_mul(60 * 60)
    }
}

/// RAM tools settings.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct MemoryToolsSettings {
    /// Ask before the first RAM edit or freeze of each recording (they are recorded into it).
    #[serde(default = "MemoryToolsSettings::DEFAULT_CONFIRM_WRITES_WHILE_RECORDING")]
    pub confirm_writes_while_recording: bool
}

impl MemoryToolsSettings {
    const DEFAULT_CONFIRM_WRITES_WHILE_RECORDING: fn() -> bool = || true;
}

impl Default for MemoryToolsSettings {
    fn default() -> Self {
        Self { confirm_writes_while_recording: Self::DEFAULT_CONFIRM_WRITES_WHILE_RECORDING() }
    }
}

/// Replay bookmark types and preferences.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct BookmarkSettings {
    /// The user's bookmark types, in the order they were created.
    #[serde(default = "Vec::new")]
    pub types: Vec<BookmarkTypeSetting>,

    /// Type given to new bookmarks unless one is asked for; 0 is untyped.
    #[serde(default = "BookmarkSettings::DEFAULT_ACTIVE_TYPE", with = "hex_type_id")]
    pub active_type: u64,

    /// Ask before the first bookmark edit upgrades a pre-v5 replay (older builds cannot open it
    /// afterwards).
    #[serde(default = "BookmarkSettings::DEFAULT_CONFIRM_REPLAY_UPGRADE")]
    pub confirm_replay_upgrade: bool
}

impl BookmarkSettings {
    const DEFAULT_ACTIVE_TYPE: fn() -> u64 = || 0;
    const DEFAULT_CONFIRM_REPLAY_UPGRADE: fn() -> bool = || true;

    /// The type with this id.
    pub fn get_type(&self, id: u64) -> Option<&BookmarkTypeSetting> {
        (id != 0).then(|| self.types.iter().find(|t| t.id == id)).flatten()
    }
}

impl Default for BookmarkSettings {
    fn default() -> Self {
        Self {
            types: Vec::new(),
            active_type: Self::DEFAULT_ACTIVE_TYPE(),
            confirm_replay_upgrade: Self::DEFAULT_CONFIRM_REPLAY_UPGRADE()
        }
    }
}

/// A user-defined bookmark type.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct BookmarkTypeSetting {
    /// Non-zero id, stored in replays as well; written as 16 hex digits.
    #[serde(with = "hex_type_id")]
    pub id: u64,

    pub name: String,

    /// `0xRRGGBB`; written as `#RRGGBB`.
    #[serde(with = "hex_color")]
    pub color: u32
}

/// Bookmark type ids as 16 hex digits (`""` for 0), so they survive JSON readers that cannot hold a u64.
pub mod hex_type_id {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn format(id: u64) -> String {
        if id == 0 { String::new() } else { format!("{id:016x}") }
    }

    pub fn parse(text: &str) -> Option<u64> {
        let text = text.trim();
        if text.is_empty() {
            return Some(0)
        }
        u64::from_str_radix(text, 16).ok()
    }

    pub fn serialize<S: Serializer>(id: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format(*id))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        let text = String::deserialize(deserializer)?;
        parse(&text).ok_or_else(|| serde::de::Error::custom(format!("invalid bookmark type id {text:?}")))
    }
}

/// Colors as `#RRGGBB`.
pub mod hex_color {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn format(color: u32) -> String {
        format!("#{:06X}", color & 0xFF_FFFF)
    }

    pub fn parse(text: &str) -> Option<u32> {
        let hex = text.trim().strip_prefix('#')?;
        (hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit())).then(|| u32::from_str_radix(hex, 16).ok()).flatten()
    }

    pub fn serialize<S: Serializer>(color: &u32, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format(*color))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u32, D::Error> {
        let text = String::deserialize(deserializer)?;
        parse(&text).ok_or_else(|| serde::de::Error::custom(format!("invalid color {text:?} (expected #RRGGBB)")))
    }
}

/// Settings for the "export video from replay" feature.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct ExportSettings {
    /// Path to the `ffmpeg` binary. Defaults to `"ffmpeg"` (resolved on `PATH`); the user can
    /// override this with a full path to a specific `ffmpeg` executable.
    #[serde(default = "ExportSettings::FFMPEG_PATH")]
    pub ffmpeg_path: String,

    /// The default encoding preset to use for exports.
    #[serde(default = "ExportPreset::default")]
    pub default_preset: ExportPreset,

    /// The default integer (nearest-neighbour) upscale factor. Defaults to 1.
    #[serde(default = "ExportSettings::DEFAULT_SCALE")]
    pub default_scale: NonZeroU8,

    /// The default constant rate factor (CRF) for H.264 exports. Defaults to 18.
    #[serde(default = "ExportSettings::DEFAULT_CRF")]
    pub default_crf: u8,

    /// The default directory to write exported videos to. `None` means alongside the replays.
    #[serde(default = "ExportSettings::OUTPUT_DIR")]
    pub output_dir: Option<PathBuf>,
}

impl Default for ExportSettings {
    fn default() -> Self {
        Self {
            ffmpeg_path: Self::FFMPEG_PATH(),
            default_preset: ExportPreset::default(),
            default_scale: Self::DEFAULT_SCALE(),
            default_crf: Self::DEFAULT_CRF(),
            output_dir: Self::OUTPUT_DIR()
        }
    }
}

impl ExportSettings {
    const FFMPEG_PATH: fn() -> String = || "ffmpeg".to_owned();
    const DEFAULT_SCALE: fn() -> NonZeroU8 = || unsafe { NonZeroU8::new_unchecked(1) };
    const DEFAULT_CRF: fn() -> u8 = || 18;
    const OUTPUT_DIR: fn() -> Option<PathBuf> = || None;
}

/// A preset describing how an exported video should be encoded.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub enum ExportPreset {
    /// MP4 container with H.264 video.
    Mp4H264,

    /// Matroska (MKV) container with lossless FFV1 video.
    LosslessFfv1Mkv,

    /// Custom: the contained string is split on whitespace into extra ffmpeg output arguments.
    Custom(String)
}

impl Default for ExportPreset {
    fn default() -> Self {
        ExportPreset::Mp4H264
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct ROMConfig {
    pub save_name: UTF8CString
}

impl Default for ROMConfig {
    fn default() -> Self {
        Self {
            save_name: "default".into()
        }
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct SimpleEnabledByDefaultSettings {
    pub enabled: bool
}

/// The Poke-A-Byte integration server (UDP + shared memory, one per game).
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct PokeAByteSettings {
    #[serde(default = "PokeAByteSettings::DEFAULT_ENABLED")]
    pub enabled: bool,

    /// The UDP port the player's own game is served on (Poke-A-Byte connects to 55356 unless
    /// told otherwise).
    #[serde(default = "PokeAByteSettings::DEFAULT_PORT")]
    pub port: u16,

    /// Also serve every friend's game in a Play Together session, each on the lowest free port
    /// above `port`, so one Poke-A-Byte can read all of them (its `/instances/<port>/` routes).
    #[serde(default = "PokeAByteSettings::DEFAULT_SERVE_FRIENDS")]
    pub serve_friends: bool
}

impl PokeAByteSettings {
    const DEFAULT_ENABLED: fn() -> bool = || true;
    const DEFAULT_PORT: fn() -> u16 = || supershuckie_core::POKEABYTE_DEFAULT_PORT;
    const DEFAULT_SERVE_FRIENDS: fn() -> bool = || true;

    /// How many ports above `port` a friend's game may be served on (`port + 1 ..= port + MAX_FRIEND_PORTS`).
    pub const MAX_FRIEND_PORTS: u16 = 32;

    /// Bring out-of-range values from the config file back into range.
    pub(crate) fn clamp(&mut self) {
        if self.port == 0 {
            self.port = Self::DEFAULT_PORT();
        }
    }
}

impl Default for PokeAByteSettings {
    fn default() -> Self {
        Self {
            enabled: Self::DEFAULT_ENABLED(),
            port: Self::DEFAULT_PORT(),
            serve_friends: Self::DEFAULT_SERVE_FRIENDS()
        }
    }
}

impl Default for SimpleEnabledByDefaultSettings {
    fn default() -> Self {
        Self {
            enabled: true
        }
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct EmulationSettings {
    #[serde(default = "EmulationSettings::DEFAULT_BASE_SPEED_MULTIPLIER")]
    pub base_speed_multiplier: f64,

    #[serde(default = "EmulationSettings::DEFAULT_TURBO_SPEED_MULTIPLIER")]
    pub turbo_speed_multiplier: f64,

    #[serde(default = "EmulationSettings::DEFAULT_MAX_SAVE_STATE_HISTORY")]
    pub max_save_state_history: NonZeroUsize
}

impl EmulationSettings {
    const DEFAULT_BASE_SPEED_MULTIPLIER: fn() -> f64 = || 1.0;
    const DEFAULT_TURBO_SPEED_MULTIPLIER: fn() -> f64 = || 2.0;
    const DEFAULT_MAX_SAVE_STATE_HISTORY: fn() -> NonZeroUsize = || unsafe { NonZeroUsize::new_unchecked(100) };
}

impl Default for EmulationSettings {
    fn default() -> Self {
        Self {
            base_speed_multiplier: EmulationSettings::DEFAULT_BASE_SPEED_MULTIPLIER(),
            turbo_speed_multiplier: EmulationSettings::DEFAULT_TURBO_SPEED_MULTIPLIER(),
            max_save_state_history: EmulationSettings::DEFAULT_MAX_SAVE_STATE_HISTORY()
        }
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct GameBoySettings {
    #[serde(default = "GameBoyMode::default")]
    pub gbc_mode: GameBoyMode,

    #[serde(default = "bool::default")]
    pub sgb: bool,

    #[serde(default = "GameBoySettings::DEFAULT_VIDEO_SCALE")]
    pub video_scale: NonZeroU8,

    #[serde(default = "Controls::default")]
    pub controls: Controls
}

impl GameBoySettings {
    const DEFAULT_VIDEO_SCALE: fn() -> NonZeroU8 = || unsafe { NonZeroU8::new_unchecked(4) };
}

impl Default for GameBoySettings {
    fn default() -> Self {
        Self {
            gbc_mode: GameBoyMode::default(),
            sgb: false,
            video_scale: Self::DEFAULT_VIDEO_SCALE(),
            controls: Controls::default()
        }
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct GameBoyAdvanceSettings {
    #[serde(default = "GameBoyAdvanceSettings::DEFAULT_VIDEO_SCALE")]
    pub video_scale: NonZeroU8,

    #[serde(default = "Controls::default")]
    pub controls: Controls
}

impl GameBoyAdvanceSettings {
    const DEFAULT_VIDEO_SCALE: fn() -> NonZeroU8 = || unsafe { NonZeroU8::new_unchecked(4) };
}

impl Default for GameBoyAdvanceSettings {
    fn default() -> Self {
        Self {
            video_scale: Self::DEFAULT_VIDEO_SCALE(),
            controls: Controls::default()
        }
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct NintendoDSSettings {
    #[serde(default = "NintendoDSDate::default")]
    pub date: NintendoDSDate,

    #[serde(default = "bool::default")]
    pub jit: bool,

    /// Swap the on-screen positions of the top and bottom DS screens.
    #[serde(default = "bool::default")]
    pub swap_screens: bool,

    #[serde(default = "NintendoDSSettings::DEFAULT_VIDEO_SCALE")]
    pub video_scale: NonZeroU8,

    #[serde(default = "Controls::default")]
    pub controls: Controls
}

impl NintendoDSSettings {
    const DEFAULT_VIDEO_SCALE: fn() -> NonZeroU8 = || unsafe { NonZeroU8::new_unchecked(2) };
}

impl Default for NintendoDSSettings {
    fn default() -> Self {
        Self {
            date: NintendoDSDate::default(),
            jit: false,
            swap_screens: false,
            video_scale: Self::DEFAULT_VIDEO_SCALE(),
            controls: Controls::default()
        }
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[repr(C)]
pub struct NintendoDSDate {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8
}

impl NintendoDSDate {
    /// Clean up ranges.
    pub fn get_cleaned(self) -> Self {
        let mut cleaned = Self {
            year: self.year.clamp(2000, 2099),
            month: self.month.clamp(1, 12),
            day: self.day.clamp(1, 31),
            hour: self.hour.clamp(0, 23),
            minute: self.minute.clamp(0, 59),
            second: self.second.clamp(0, 59),
        };

        let max_days_per_month = |month: u8| match month {
            2 => 29,
            4|6|9|11 => 30,
            _ => 31
        };

        cleaned.day = cleaned.day.min(max_days_per_month(cleaned.month));

        // handle leap years (M9: February is 29 days in a leap year and 28 otherwise; this used
        // to be backwards, capping at 28 only IN leap years)
        //
        // (the % 100/400 is technically redundant since the range is 2000-2099, and 2000 was a leap year; idc)
        let is_leap = cleaned.year % 4 == 0 && (cleaned.year % 100 != 0 || cleaned.year % 400 == 0);
        if cleaned.month == 2 && !is_leap {
            cleaned.day = cleaned.day.min(28);
        }

        cleaned
    }
}

#[derive(Copy, Clone, PartialEq, Debug, Serialize, Deserialize, Default, TryFromPrimitive)]
#[repr(u32)]
pub enum GameBoyMode {
    /// Run all Game Boy games in Game Boy Color mode
    #[serde(rename = "GBC-always")]
    #[default]
    AlwaysGBC = 0,

    /// Run Game Boy games in Game Boy mode
    #[serde(rename = "GBC-auto")]
    GBInGBMode = 1,

    /// Run all Game Boy games in Game Boy mode (except Game Boy Color-only games, which cannot
    /// run there and always get a Game Boy Color)
    #[serde(rename = "GBC-never")]
    AlwaysGB = 2
}

pub type ControlMap = BTreeMap<i32, ControlSetting>;

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct Controls {
    #[serde(default = "BTreeMap::default")]
    pub keyboard_controls: ControlMap,

    #[serde(default = "BTreeMap::default")]
    pub controller_controls: BTreeMap<String, ControllerSettings>
}

impl Controls {
    pub(crate) const fn new() -> Self {
        Self {
            keyboard_controls: ControlMap::new(),
            controller_controls: BTreeMap::new()
        }
    }
}

impl Default for Controls {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ControllerSettings {
    #[serde(default = "BTreeMap::default")]
    pub buttons: ControlMap,

    #[serde(default = "BTreeMap::default")]
    pub axis: ControlMap,
}

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ControlSetting {
    pub control: Control,
    #[serde(default = "ControlModifier::default")]
    #[serde(skip_serializing_if = "ControlModifier::is_default")]
    pub modifier: ControlModifier
}

// FIXME: Determine if we need this. If not, get rid of it!
impl ControlSetting {
    pub const fn as_u64(self) -> u64 {
        let low = self.control as u64;
        let high = self.modifier as u64;
        low | (high << 32)
    }
    pub fn from_u64(u: u64) -> Option<Self> {
        let low = u as u32;
        let high = (u >> 32) as u32;

        let control = Control::try_from(low).ok()?;
        let modifier = ControlModifier::try_from(high).ok()?;

        Some(Self { control, modifier })
    }
}

#[derive(Copy, Clone, Default, Debug, PartialEq, Serialize, Deserialize, TryFromPrimitive)]
#[repr(u32)]
#[serde(rename_all = "snake_case")]
pub enum ControlModifier {
    #[default]
    Normal,
    Rapid,
    Toggle,
    /// One short press per key press, however long the key is held: the button goes down for
    /// [`ControlModifier::SINGLE_PRESS_HOLD_LENGTH`] frames and comes back up on its own.
    // Briefly saved as `single_frame` (before it held for more than one).
    #[serde(alias = "single_frame")]
    SinglePress
}

impl ControlModifier {
    /// How many frames a [`ControlModifier::SinglePress`] press holds the button for: as short
    /// as a tap can be while a game that does not read the joypad every frame still sees it.
    pub const SINGLE_PRESS_HOLD_LENGTH: NonZeroU64 = NonZeroU64::new(3).unwrap();

    fn is_default(&self) -> bool {
        self == &ControlModifier::Normal
    }

    #[inline]
    pub const fn as_str(self) -> &'static str {
        let cstr = self.as_c_str();
        let Ok(str) = cstr.to_str() else {
            // SAFETY: Trust me bro.
            unsafe { unreachable_unchecked() }
        };
        str
    }

    pub const fn as_c_str(self) -> &'static CStr {
        match self {
            ControlModifier::Normal => c"Normal",
            ControlModifier::Rapid => c"Rapid Fire",
            ControlModifier::Toggle => c"Toggle",
            ControlModifier::SinglePress => c"Single Press"
        }
    }

}

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize, TryFromPrimitive)]
#[repr(u32)]
#[serde(rename_all = "snake_case")]
pub enum Control {
    Up,
    Down,
    Left,
    Right,

    A,
    B,
    Start,
    Select,

    L,
    R,
    X,
    Y,

    Turbo,
    Reset,
    Pause,
    SwapScreens
}
impl Control {
    pub const fn is_button(self) -> bool {
        match self {
            Control::A => true,
            Control::B => true,
            Control::Start => true,
            Control::Select => true,
            Control::Up => true,
            Control::Down => true,
            Control::Left => true,
            Control::Right => true,
            Control::L => true,
            Control::R => true,
            Control::X => true,
            Control::Y => true,
            Control::Turbo => false,
            Control::Reset => false,
            Control::Pause => false,
            Control::SwapScreens => false
        }
    }

    pub(crate) const fn set_for_input(&self, input: &mut Input, value: bool) {
        match self {
            Control::A => input.a = value,
            Control::B => input.b = value,
            Control::Start => input.start = value,
            Control::Select => input.select = value,
            Control::Up => input.d_up = value,
            Control::Down => input.d_down = value,
            Control::Left => input.d_left = value,
            Control::Right => input.d_right = value,
            Control::L => input.l = value,
            Control::R => input.r = value,
            Control::X => input.x = value,
            Control::Y => input.y = value,
            Control::Turbo => {}
            Control::Reset => {}
            Control::Pause => {}
            Control::SwapScreens => {}
        }
    }

    pub(crate) const fn invert_for_input(&self, input: &mut Input) {
        match self {
            Control::A => input.a = !input.a,
            Control::B => input.b = !input.b,
            Control::Start => input.start = !input.start,
            Control::Select => input.select = !input.select,
            Control::Up => input.d_up = !input.d_up,
            Control::Down => input.d_down = !input.d_down,
            Control::Left => input.d_left = !input.d_left,
            Control::Right => input.d_right = !input.d_right,
            Control::L => input.l = !input.l,
            Control::R => input.r = !input.r,
            Control::X => input.x = !input.x,
            Control::Y => input.y = !input.y,
            Control::Turbo => {}
            Control::Reset => {}
            Control::Pause => {}
            Control::SwapScreens => {}
        }
    }

    #[inline]
    pub const fn as_str(self) -> &'static str {
        let cstr = self.as_c_str();
        let Ok(str) = cstr.to_str() else {
            // SAFETY: Trust me bro.
            unsafe { unreachable_unchecked() }
        };
        str
    }

    pub const fn as_c_str(self) -> &'static CStr {
        match self {
            Control::A => c"A",
            Control::B => c"B",
            Control::Start => c"Start",
            Control::Select => c"Select",
            Control::Up => c"D-Up",
            Control::Down => c"D-Down",
            Control::Left => c"D-Left",
            Control::Right => c"D-Right",
            Control::L => c"L",
            Control::R => c"R",
            Control::X => c"X",
            Control::Y => c"Y",
            Control::Turbo => c"Turbo",
            Control::Reset => c"Reset console",
            Control::Pause => c"Pause",
            Control::SwapScreens => c"Swap screens"
        }
    }

    pub const fn is_available_for_emulator_type(self, emulator_type: SuperShuckieEmulatorType) -> bool {
        match self {
            Control::L | Control::R => matches!(emulator_type, SuperShuckieEmulatorType::NintendoDS | SuperShuckieEmulatorType::GameBoyAdvance),
            Control::X | Control::Y | Control::SwapScreens => matches!(emulator_type, SuperShuckieEmulatorType::NintendoDS),
            _ => true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dirs(name: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("supershuckie-frontend-settings-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        (root.join("data"), root.join("config"))
    }

    #[test]
    fn bad_settings_fall_back_to_defaults_and_keep_a_bad_copy() {
        let (data_dir, config_dir) = temp_dirs("bad");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(config_dir.join(SETTINGS_FILE), b"{not valid json at all").unwrap();

        let (settings, warnings) = try_to_init_data_dir_and_get_settings(&data_dir, &config_dir);

        assert_eq!(warnings.len(), 1, "a single warning should be reported: {warnings:?}");
        assert!(warnings[0].contains("settings.json.bad"), "warning should mention the backup: {}", warnings[0]);
        assert_eq!(settings.audio.volume, AudioSettings::DEFAULT_VOLUME(), "defaults should be in use");

        let backup = fs::read_to_string(config_dir.join(BAD_SETTINGS_FILE)).expect("the bad file should have been copied aside");
        assert_eq!(backup, "{not valid json at all", "the original bad content should be preserved verbatim");

        // The original (unreadable) file is left in place, untouched.
        let original = fs::read_to_string(config_dir.join(SETTINGS_FILE)).unwrap();
        assert_eq!(original, "{not valid json at all");

        let _ = fs::remove_dir_all(data_dir.parent().unwrap());
    }

    #[test]
    fn missing_or_blank_settings_use_defaults_with_no_warning() {
        let (data_dir, config_dir) = temp_dirs("missing");

        let (settings, warnings) = try_to_init_data_dir_and_get_settings(&data_dir, &config_dir);
        assert!(warnings.is_empty(), "a missing settings file is not an error: {warnings:?}");
        assert_eq!(settings.audio.volume, AudioSettings::DEFAULT_VOLUME());
        assert!(data_dir.is_dir(), "the data dir should have been created");
        assert!(config_dir.is_dir(), "the config dir should have been created");

        fs::write(config_dir.join(SETTINGS_FILE), b"   \n").unwrap();
        let (settings, warnings) = try_to_init_data_dir_and_get_settings(&data_dir, &config_dir);
        assert!(warnings.is_empty(), "a blank settings file is not an error: {warnings:?}");
        assert_eq!(settings.audio.volume, AudioSettings::DEFAULT_VOLUME());

        let _ = fs::remove_dir_all(data_dir.parent().unwrap());
    }

    #[test]
    fn out_of_range_values_are_clamped_on_load() {
        let (data_dir, config_dir) = temp_dirs("clamped");
        fs::create_dir_all(&config_dir).unwrap();

        // Hand-written JSON: NaN/Infinity have no JSON representation (serde_json serializes them
        // as `null`, which then fails to parse back as f64), so a negative and a huge-but-finite
        // multiplier stand in here to exercise the same `Speed` normalisation; NaN/Infinity are
        // covered directly against `Settings::clamp` below.
        let json = r#"{
            "replay": { "zstd_compression_level": 99 },
            "emulation": { "base_speed_multiplier": -5.0, "turbo_speed_multiplier": 1000000.0 },
            "export": { "default_crf": 255 },
            "recent_roms": { "max_recent_roms": 2, "recent_roms": ["a", "b", "c", "d"] }
        }"#;
        fs::write(config_dir.join(SETTINGS_FILE), json).unwrap();

        let (loaded, warnings) = try_to_init_data_dir_and_get_settings(&data_dir, &config_dir);
        assert!(warnings.is_empty(), "a parseable file with out-of-range values is not a warning: {warnings:?}");

        assert_eq!(loaded.replay.zstd_compression_level, 22);
        assert_eq!(loaded.emulation.base_speed_multiplier, Speed::from_multiplier_float(-5.0).into_multiplier_float());
        assert_eq!(loaded.emulation.turbo_speed_multiplier, Speed::from_multiplier_float(1000000.0).into_multiplier_float());
        assert_eq!(loaded.export.default_crf, 51);
        assert_eq!(loaded.recent_roms.recent_roms.len(), 2, "recent ROMs must be capped to max_recent_roms");

        let mut roms = RecentROMs {
            max_recent_roms: NonZeroIsize::new(50).unwrap(),
            recent_roms: (0..30).map(|i| UTF8CString::from_str(&i.to_string())).collect(),
        };
        roms.clamp();
        assert_eq!(roms.max_recent_roms.get(), MAX_RECENT_ROMS as isize, "max_recent_roms must never exceed the hard cap");
        assert_eq!(roms.recent_roms.len(), MAX_RECENT_ROMS);

        let _ = fs::remove_dir_all(data_dir.parent().unwrap());
    }

    #[test]
    fn nan_and_infinite_speeds_collapse_to_the_minimum_speed_via_clamp() {
        // NaN/Infinity cannot round-trip through JSON (see the test above), but `clamp()` must
        // still defend against them turning up in memory some other way (e.g. a bad division).
        let minimum_speed = Speed::from_multiplier_float(0.0).into_multiplier_float();

        let mut settings = Settings::default();
        settings.emulation.base_speed_multiplier = f64::NAN;
        settings.emulation.turbo_speed_multiplier = f64::NEG_INFINITY;
        settings.clamp();

        assert_eq!(settings.emulation.base_speed_multiplier, minimum_speed, "NaN must collapse to the minimum speed");
        assert_eq!(settings.emulation.turbo_speed_multiplier, minimum_speed, "-Infinity must collapse to the minimum speed");
    }

    #[test]
    fn nintendo_ds_date_leap_years_are_cleaned_correctly() {
        let date = |year: u16| NintendoDSDate { year, month: 2, day: 29, hour: 0, minute: 0, second: 0 };

        assert_eq!(date(2024).get_cleaned().day, 29, "2024 is a leap year");
        assert_eq!(date(2023).get_cleaned().day, 28, "2023 is not a leap year");
        assert_eq!(date(2100).get_cleaned().day, 28, "2100 is not a leap year (divisible by 100, not 400)");
    }
}
