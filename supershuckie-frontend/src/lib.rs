pub mod util;
pub mod settings;
pub mod replay_convert;
pub mod memory_tools;
pub mod bookmarks;
pub mod play_together;

use std::cell::OnceCell;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use crate::settings::*;
use crate::util::{check_user_file_name, tail_utf8, UTF8CString};
use std::ffi::CStr;
use std::fmt::Formatter;
use std::fs::File;
use std::hint::unreachable_unchecked;
use std::io::Write;
use std::num::{NonZeroU64, NonZeroU8};
use std::path::{absolute, Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};
use std::io::BufWriter;
use std::borrow::Cow;
use std::process::{Child, ChildStdin, Command, Stdio};
use num_enum::TryFromPrimitive;
use supershuckie_core::emulator::{EmulatorCore, GameBoyColor, Input, Model, PartialReplayRecordMetadata, ScreenData, ScreenDataEncoding, NullEmulatorCore, NintendoDS, GameBoyAdvance};
use supershuckie_core::{std_timestamp_provider, AudioOutput, ElapsedTimeStats, ReplayPlayerAttachError, Speed, SuperShuckieRapidFire, ThreadedSuperShuckieCore};
use supershuckie_core::{ExportRange, ScreenLayout, VideoExportError, VideoExportHandle, VideoFrameSink};
use supershuckie_frontend_webserver::{Stats, SuperShuckieServerCommand, SuperShuckieWebserver};
use supershuckie_replay_recorder::replay_file::{ReplayConsoleType, ReplayHeaderBlake3Hash, ReplayPatchFormat};
use supershuckie_replay_recorder::{blake3_hash, ByteVec, SignedInteger, TimestampMillis, UnsignedInteger};
use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::replay_file::record::{ReplayFileRecorderSettings, ReplayFileWriteError, ResumeCropPolicy};

const SETTINGS_FILE: &str = "settings.json";
const SAVE_STATE_EXTENSION: &str = "save_state";
const SAVE_DATA_EXTENSION: &str = "sav";
const REPLAY_EXTENSION: &str = "replay";

pub type ConnectedControllerIndex = u32;

#[derive(Copy, Clone, PartialEq, Debug, TryFromPrimitive)]
#[repr(u8)]
pub enum SuperShuckieEmulatorType {
    GameBoy,
    GameBoySGB2,
    GameBoyColor,
    GameBoyAdvance,
    NintendoDS
}

impl SuperShuckieEmulatorType {
    /// Return true if this uses a shared config with another system.
    ///
    /// Does not return true if this system owns that config.
    pub const fn uses_shared_config(self) -> bool {
        match self {
            SuperShuckieEmulatorType::GameBoy => false,
            SuperShuckieEmulatorType::GameBoySGB2 => true,
            SuperShuckieEmulatorType::GameBoyColor => true,
            SuperShuckieEmulatorType::GameBoyAdvance => false,
            SuperShuckieEmulatorType::NintendoDS => false,
        }
    }

    /// Get the human-readable name.
    #[inline]
    pub const fn name(self) -> &'static str {
        match self.name_cstr().to_str() {
            Ok(n) => n,
            Err(_) => unsafe { unreachable_unchecked() }
        }
    }

    /// Get the human-readable name as a C string.
    pub const fn name_cstr(self) -> &'static CStr {
        match self {
            SuperShuckieEmulatorType::GameBoy => c"Game Boy",
            SuperShuckieEmulatorType::GameBoySGB2 => c"Super Game Boy 2",
            SuperShuckieEmulatorType::GameBoyColor => c"Game Boy Color",
            SuperShuckieEmulatorType::GameBoyAdvance => c"Game Boy Advance",
            SuperShuckieEmulatorType::NintendoDS => c"Nintendo DS"
        }
    }

    /// The replay console type family this emulator type belongs to (used to sanity-check a
    /// replay's console type against a system before reinstantiating a core for it).
    pub const fn replay_console_type(self) -> ReplayConsoleType {
        match self {
            SuperShuckieEmulatorType::GameBoy => ReplayConsoleType::GameBoy,
            SuperShuckieEmulatorType::GameBoySGB2 => ReplayConsoleType::SuperGameBoy2,
            SuperShuckieEmulatorType::GameBoyColor => ReplayConsoleType::GameBoyColor,
            SuperShuckieEmulatorType::GameBoyAdvance => ReplayConsoleType::GameBoyAdvance,
            SuperShuckieEmulatorType::NintendoDS => ReplayConsoleType::NintendoDS
        }
    }
}

impl core::fmt::Display for SuperShuckieEmulatorType {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}



pub enum UserInput {
    Keyboard { keycode: i32 },
    Button { controller: ConnectedControllerIndex, button: i32 },
    Axis { controller: ConnectedControllerIndex, axis: i32 }
}

/// Frame pacing state for [`SuperShuckieFrontend::present_latest_frame`].
#[derive(Default)]
struct PresentPacer {
    window_start: Option<Instant>,
    window_refreshes: u32,
    window_start_generation: u32,
    /// Display refreshes each drawn frame should be shown for: the measured ratio when it is
    /// close to a whole number of at least 2, else 1 (present as soon as a frame is available).
    refreshes_per_frame: u32,
    refreshes_since_present: u32,
}

pub struct SuperShuckieFrontend {
    core: ThreadedSuperShuckieCore,
    emulator_type: Option<SuperShuckieEmulatorType>,

    callbacks: Box<dyn SuperShuckieFrontendCallbacks>,

    user_dir: PathBuf,
    config_dir: PathBuf,
    pokeabyte_error: Option<UTF8CString>,

    /// The speed the core was last set to (base speed, turbo, or the link cable's).
    current_speed: Speed,

    loaded_rom_data: Option<Vec<u8>>,

    current_input: Input,
    current_rapid_fire_input: Option<SuperShuckieRapidFire>,
    current_toggled_input: Option<Input>,
    current_save_state_history: Vec<Vec<u8>>,
    current_save_state_history_position: usize,
    current_replay: Option<UTF8CString>,

    web_server: Option<SuperShuckieWebserver>,
    external_commands_error: Option<UTF8CString>,

    connected_controllers: BTreeMap<ConnectedControllerIndex, UTF8CString>,

    rom_name: Option<Arc<UTF8CString>>,
    save_file: Option<Arc<UTF8CString>>,
    recording_replay_file: Option<ReplayFileInfo>,

    /// Whether the replay currently attached for playback had damaged data dropped when it was
    /// parsed. Cleared whenever the replay is unloaded (see [`Self::close_replay`]).
    current_replay_truncated: bool,

    bios_override: Option<Vec<u8>>,

    last_read_elapsed_time_stats: ElapsedTimeStats,

    /// `(when, emulated frame count then)` for [`Self::get_emulation_fps`].
    fps_window: Option<(Instant, u64)>,
    /// When set, `tick` no longer hands new frames to the UI as they arrive; the UI asks for the
    /// latest one with `present_latest_frame` (once per display refresh, to keep a steady cadence).
    present_on_demand: bool,
    /// `screen_generation` of the last frame handed to the UI.
    last_presented_screen_generation: u32,
    present_pacer: PresentPacer,
    last_emulation_fps: f64,
    last_read_replay_stats: Option<LastReadReplayCropData>,

    last_replay_and_frame: Option<(UTF8CString, u32)>,

    /// In-progress video export, if any (owned here so the C/Qt layer just polls/cancels).
    current_export: Option<VideoExportHandle>,

    /// A replay conversion built by [`plan_replay_conversion`](Self::plan_replay_conversion) and
    /// not started yet.
    pending_conversion_plan: Option<replay_convert::ConversionPlan>,

    /// The replay conversion in progress, if any.
    current_conversion: Option<replay_convert::ConversionJob>,

    /// Samples from the running core, whichever core that currently is; the audio device reads
    /// from this on its own thread for the life of the frontend.
    audio_output: Arc<AudioOutput>,

    /// RAM viewer, search, watch, editing and freezing.
    memory_tools: memory_tools::MemoryTools,

    /// The current replay's bookmarks.
    bookmarks: bookmarks::ReplayBookmarks,

    /// Changes whenever the user's bookmark types do (see [`Self::bookmark_generation`]).
    bookmark_types_generation: u64,

    /// Errors to surface on the next [`Self::tick`], for calls that cannot return one to their
    /// caller directly (background cleanup, a reload triggered by a setting change, ...). See
    /// [`Self::report_later`].
    deferred_errors: Vec<String>,

    /// Whether the core thread's death has already been reported (and the ROM unloaded) this
    /// "life"; reset whenever a new core is assigned (see [`Self::assign_core`]).
    core_death_reported: bool,

    /// The settings changed since they were last written to disk; `tick()` coalesces any number
    /// of changes into a single write (M10). See [`Self::mark_settings_dirty`].
    settings_dirty: bool,

    /// The Play Together session, if any (see [`play_together`]).
    play_together: Option<play_together::PlayTogetherSession>,

    /// Extra paths that may hold another player's ROM (see
    /// [`Self::play_together_add_rom_candidates`]).
    play_together_rom_candidates: Vec<PathBuf>,

    settings: Settings
}

impl SuperShuckieFrontend {
    pub fn new<DATA: AsRef<Path>, CONF: AsRef<Path>>(data: DATA, config_dir: CONF, callbacks: Box<dyn SuperShuckieFrontendCallbacks>) -> Self {
        let data_dir = data.as_ref().to_owned();

        let (settings, settings_warnings) = try_to_init_data_dir_and_get_settings(
            data_dir.as_ref(),
            config_dir.as_ref(),
        );

        let audio_output = Arc::new(AudioOutput::new(settings.audio.latency_ms as u32));
        let mut memory_tools = memory_tools::MemoryTools::new(data_dir.join("tables"));
        memory_tools.set_confirm_writes_while_recording(settings.memory_tools.confirm_writes_while_recording);

        let mut s = Self {
            core: ThreadedSuperShuckieCore::new(Box::new(NullEmulatorCore)),
            emulator_type: None,
            user_dir: data_dir,
            rom_name: None,
            save_file: None,
            loaded_rom_data: None,
            current_rapid_fire_input: None,
            current_toggled_input: None,
            callbacks,
            settings,
            current_input: Input::default(),
            current_save_state_history: Vec::new(),
            last_read_elapsed_time_stats: ElapsedTimeStats::default(),
            fps_window: None,
            present_on_demand: false,
            last_presented_screen_generation: 0,
            present_pacer: PresentPacer::default(),
            last_emulation_fps: 0.0,
            current_save_state_history_position: 0,
            recording_replay_file: None,
            current_replay_truncated: false,
            pokeabyte_error: None,
            current_speed: Speed::default(),
            config_dir: config_dir.as_ref().to_owned(),
            web_server: None,
            external_commands_error: None,
            connected_controllers: BTreeMap::new(),
            last_read_replay_stats: None,
            bios_override: None,
            last_replay_and_frame: None,
            current_replay: None,
            current_export: None,
            pending_conversion_plan: None,
            current_conversion: None,
            audio_output,
            memory_tools,
            bookmarks: bookmarks::ReplayBookmarks::new(),
            bookmark_types_generation: 0,
            deferred_errors: Vec::new(),
            core_death_reported: false,
            settings_dirty: false,
            play_together: None,
            play_together_rom_candidates: Vec::new()
        };

        // C4: startup never aborts on a bad/unreadable settings file; surface what happened
        // instead, on the first tick.
        for warning in settings_warnings {
            s.report_later(warning);
        }

        // This is not tied to the core, so we want to immediately enable this.
        if s.settings.external_commands.enabled {
            let _ = s.set_external_commands_enabled(true);
        }

        s.unload_rom();
        s
    }

    /// Create a save state.
    ///
    /// If `name` is set, that name will be used.
    ///
    /// Returns the name of the save state if created.
    pub fn create_save_state(&mut self, name: Option<&str>) -> Result<UTF8CString, UTF8CString> {
        self.refuse_if_exporting()?;
        if let Some(n) = name {
            check_user_file_name(n)?;
        }

        if !self.is_game_running() {
            return Err("Game not running".into())
        }

        let current_rom_name = self.get_current_rom_name().expect("no rom name when game is running in create_save_state");
        let save_states_dir = self.get_save_states_dir_for_rom(current_rom_name);

        let (mut file, filename, _) = self.load_file_or_make_generic(&save_states_dir, name, None, SAVE_STATE_EXTENSION)?;

        let state = self.create_save_state_now().ok_or("The emulator did not produce a save state")?;
        file.write_all(&state)
            .map_err(|e| format!("Can't write to {filename}: {e}").into())
            .map(|_| filename.into())
    }

    /// Connect a controller.
    pub fn connect_controller(&mut self, controller_name: &str) -> ConnectedControllerIndex {
        for i in 0..=ConnectedControllerIndex::MAX {
            if self.connected_controllers.contains_key(&i) {
                continue
            }
            self.connected_controllers.insert(i, controller_name.into());
            return i;
        }

        panic!("Out of controller indices");
    }

    /// Get a list of all connected controllers.
    pub fn get_connected_controllers(&self) -> Vec<UTF8CString> {
        self.connected_controllers.iter().map(|(_,v)| v.to_owned()).collect()
    }

    /// Disconnect a controller.
    pub fn disconnect_controller(&mut self, controller: ConnectedControllerIndex) {
        self.connected_controllers.remove(&controller);
    }

    /// Get the name of the connected controller.
    pub fn name_of_controller(&self, controller: ConnectedControllerIndex) -> Option<&str> {
        self.connected_controllers.get(&controller).map(|i| i.as_str())
    }

    /// Get the name of the connected controller as a C string.
    pub fn name_of_controller_c_str(&self, controller: ConnectedControllerIndex) -> Option<&CStr> {
        self.connected_controllers.get(&controller).map(|i| i.as_c_str())
    }

    /// Mark the start of a replay.
    pub fn mark_replay_start(&mut self, timer_offset: TimestampMillis) -> Result<(), ()> {
        self.refuse_if_exporting().map_err(|_| ())?;
        if let Ok(n) = self.core.mark_start(timer_offset) {
            let stats = self.get_or_create_last_read_replay_stats();
            stats.timer_offset = Some(timer_offset);
            stats.start = Some(n);
            Ok(())
        }
        else {
            Err(())
        }
    }

    /// Mark the end of a replay.
    pub fn mark_replay_end(&mut self) -> Result<(), ()> {
        self.refuse_if_exporting().map_err(|_| ())?;
        if let Ok(n) = self.core.mark_end() {
            self.get_or_create_last_read_replay_stats().end = Some(n);
            Ok(())
        }
        else {
            Err(())
        }
    }

    fn get_or_create_last_read_replay_stats(&mut self) -> &mut LastReadReplayCropData {
        if self.last_read_replay_stats.is_none() {
            self.last_read_replay_stats = Some(LastReadReplayCropData::default());
        }
        self.last_read_replay_stats.as_mut().expect("should be created")
    }

    /// Change the given replay counter, adding delta.
    #[inline]
    pub fn change_replay_counter(&mut self, counter: String, delta: SignedInteger) {
        self.core.change_replay_counter(counter, delta);
    }

    /// Get all replay counters.
    #[inline]
    pub fn get_replay_counters(&self) -> BTreeMap<String, SignedInteger> {
        self.core.get_replay_counters()
    }

    fn load_file_or_make_generic(&mut self, dir: &Path, name: Option<&str>, generic_prefix: Option<&str>, extension: &str) -> Result<(File, String, PathBuf), UTF8CString> {
        match name {
            Some(name) => {
                check_user_file_name(name)?;
                let filename = format!("{name}.{extension}");
                let path = dir.join(&filename);
                // H4: an explicit name must never silently replace an existing file.
                match File::create_new(&path) {
                    Ok(file) => Ok((file, filename, path)),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(format!("{name} already exists").into()),
                    Err(e) => Err(format!("Can't open {name} for writing: {e}").into())
                }
            },
            None => {
                let prefix = generic_prefix.unwrap_or(self.get_current_save_name().expect("no save name when game is running in load_file_or_make_generic"));
                let mut i = 0u64;
                loop {
                    let filename = format!("{prefix}-{i}.{extension}");
                    let path = dir.join(&filename);
                    let Ok(file) = File::create_new(&path) else {
                        i = i.checked_add(1).ok_or_else(|| UTF8CString::from_str("Maximum number of generics reached."))?;
                        continue
                    };
                    return Ok((file, filename, path))
                }
            }
        }
    }

    /// Loads a save state with the given name if it exists.
    ///
    /// If it does, and it is successfully loaded, `Ok(true)` is returned.
    ///
    /// If it does not exist, `Ok(false)` is returned.
    pub fn load_save_state_if_exists(&mut self, name: &str) -> Result<bool, UTF8CString> {
        self.refuse_if_exporting()?;
        check_user_file_name(name)?;

        if !self.is_game_running() {
            return Err("Game not running".into())
        }

        let replay_state = self.get_replay_state();

        if self.settings.replay.disable_save_states_when_recording && replay_state == SuperShuckieReplayState::Recording {
            return Err("Cannot load save states when recording a replay as it is disabled".into());
        }

        if self.core.is_playing_back() {
            return Err("Cannot load save states when playing back a replay".into());
        }
        self.refuse_if_link_cable_plugged()?;

        let current_rom_name = self.get_current_rom_name().expect("no rom name when game is running in load_save_state_if_exists");
        let save_states_dir = self.get_save_states_dir_for_rom(current_rom_name);
        let save_state_file = save_states_dir.join(format!("{name}.{SAVE_STATE_EXTENSION}"));

        if !save_state_file.is_file() {
            return Ok(false)
        }

        self.push_save_state_history();

        let save_state = std::fs::read(save_state_file).map_err(|e| format!("Failed to load save state {name}: {e}"))?;
        self.core.load_save_state(save_state);
        Ok(true)
    }

    /// Loads a replay with the given name if it exists.
    ///
    /// If it does, and it is successfully loaded, `Ok(true)` is returned.
    ///
    /// If it does not exist, `Ok(false)` is returned.
    pub fn load_replay_if_exists(&mut self, name: &str, override_errors: bool) -> Result<bool, UTF8CString> {
        self.refuse_if_exporting()?;
        if self.play_together.is_some() {
            return Err("Leave the Play Together session first: the game being played together cannot be replaced by a replay.".into())
        }
        check_user_file_name(name)?;
        self.assert_replays_available()?;

        let current_rom_name = self.get_current_rom_name().expect("no rom name when game is running in load_replay_if_exists");
        let replay_dir = self.get_replays_dir_for_rom(current_rom_name);
        let replay_file = replay_dir.join(format!("{name}.{REPLAY_EXTENSION}"));

        if !replay_file.is_file() {
            return Ok(false)
        }

        let file = match std::fs::read(&replay_file) {
            Ok(n) => n,
            Err(e) => {
                return Err(format!("Failed to read replay {name}:\n\n{e}").into())
            }
        };

        let mut player = match ReplayFilePlayer::new(file, override_errors) {
            Ok(n) => n,
            Err(e) => {
                return Err(format!("Failed to parse replay {name}:\n\n{e:?}").into())
            }
        };

        if self.settings.replay.auto_decompress_replays_upfront {
            player.decompress_all_blobs();
        }

        // Parse everything we need into locals first (M3): nothing about the running core or the
        // current replay/recording state changes until we know this replay can actually attach.
        let current_emulator_type = self.emulator_type.expect("???? no emulator type when reloading a replay?");
        let metadata = player.get_replay_metadata();
        let expected_type = match metadata.console_type {
            ReplayConsoleType::GameBoy => SuperShuckieEmulatorType::GameBoy,
            ReplayConsoleType::SuperGameBoy2 => SuperShuckieEmulatorType::GameBoySGB2,
            ReplayConsoleType::GameBoyColor => SuperShuckieEmulatorType::GameBoyColor,
            _ => current_emulator_type
        };

        let stats = LastReadReplayCropData {
            start: metadata.crop_start,
            end: metadata.crop_end,
            timer_offset: metadata.timer_offset
        };

        let bookmark_table = player.bookmark_table().clone();
        let truncated = player.stream_truncated();
        let loaded_bookmarks = bookmarks::LoadedReplay {
            name: name.to_owned(),
            path: replay_file.clone(),
            header: player.raw_header_bytes(),
            version: player.get_replay_version(),
            truncated
        };

        // TODO: let the user supply their own bios override instead
        let new_bios_override = self.compute_builtin_bios_override(metadata.bios_checksum);
        let needs_reinstantiate = current_emulator_type != expected_type || new_bios_override.is_some();

        if needs_reinstantiate {
            // Check compatibility before tearing down the running core/session for nothing: an
            // incompatible replay should not cost the player their game state.
            if expected_type.replay_console_type() != metadata.console_type {
                return Err(format!(
                    "This replay file is incompatible:\n\nConsole types don't match! (replay: {:?}, rom: {:?})",
                    metadata.console_type,
                    expected_type.replay_console_type()
                ).into())
            }
            if !override_errors && metadata.rom_checksum != *self.core.rom_checksum() {
                return Err(
                    "This replay file has mismatched data which may prevent playback:\n\nThe ROM checksum does not match the one this replay was recorded with.".into()
                )
            }
        }

        // M6: the core silently drops whatever else is going on (a live recording, another
        // replay's playback) when a replay attaches; keep our bookkeeping in sync with that. This
        // comes after the compatibility checks above so that an incompatible replay does not cost
        // the player their live recording.
        if let Err(e) = self.stop_recording_replay() {
            self.report_later(e.to_string());
        }
        self.close_replay();

        let was_paused = self.is_paused();
        self.set_paused(true);

        // Whatever replay was loaded is going away (attaching replaces it even if it fails); save
        // its bookmark changes first.
        self.finish_replay_bookmarks();

        if needs_reinstantiate && let Err(e) = self.instantiate_and_load_core(expected_type) {
            self.set_paused(was_paused);
            return Err(e)
        }

        match self.core.attach_replay_player(player, override_errors) {
            Ok(()) => {}
            Err(ReplayPlayerAttachError::Incompatible { description }) => {
                self.set_paused(was_paused);
                return Err(format!("This replay file is incompatible:\n\n{description}").into())
            }
            Err(ReplayPlayerAttachError::MismatchedMetadata { issues }) => {
                self.set_paused(was_paused);
                let mut err = String::new();

                err += "This replay file has mismatched data which may prevent playback:";

                for issue in issues {
                    err += "\n\n";
                    err += &issue.to_string();
                }

                return Err(err.into())
            }
            Err(ReplayPlayerAttachError::Failed { description }) => {
                self.set_paused(was_paused);
                // The core detached itself; nothing is attached any more. Files a recording may
                // have had open are kept (not deleted), just no longer tracked as "current".
                self.recording_replay_file.take();
                self.bookmarks.clear();
                return Err(format!("This replay could not be loaded:\n\n{description}").into())
            }
        }

        self.bios_override = new_bios_override;
        self.last_read_replay_stats = Some(stats);
        self.current_replay_truncated = truncated;
        self.save_file = Some(Arc::new("replay".into()));
        self.current_replay = Some(name.into());
        self.last_replay_and_frame = None;
        self.bookmarks.set_playback(loaded_bookmarks, bookmark_table);

        Ok(true)
    }

    /// Close (unload) the loaded replay, if any, whether it is playing or stopped. Live play
    /// continues from the current frame.
    ///
    /// To hand the game back to the user while keeping the replay loaded, see
    /// [`Self::stop_replay_playback`].
    #[inline]
    pub fn close_replay(&mut self) {
        if self.current_export.is_some() {
            return
        }

        let Some(r) = self.current_replay.take() else {
            return
        };

        self.finish_replay_bookmarks();

        // "Continue last replay" goes back to where the replay was, which for a stopped replay is
        // its resume point rather than wherever live play has got to since.
        self.last_replay_and_frame = Some((r, self.last_read_elapsed_time_stats.replay_frame));
        self.last_read_replay_stats = None;
        self.current_replay_truncated = false;

        self.core.detach_replay_player();
        self.reset_speed();
        self.current_input = Input::default();

        // L9: the "replay" scratch save file is only meaningful while a replay is attached; once
        // it detaches, live play saves to the ROM's configured save file again.
        if let Some(rom_name) = self.get_current_rom_name() {
            let rom_name = rom_name.to_owned();
            self.save_file = Some(Arc::new(self.get_current_save_file_name_for_rom(&rom_name)));
        }
    }

    /// Stop the loaded replay from driving the emulator without closing it: the game keeps
    /// running from the current frame, live and under the user's input, while the replay stays
    /// loaded (its timeline can still be seeked in, which puts the game at the chosen frame and
    /// hands it back) and can be resumed from where it was stopped or last seeked to with
    /// [`Self::resume_replay_playback`]. The replay state stays
    /// [`Playback`](SuperShuckieReplayState::Playback); see [`Self::is_replay_playback_stopped`].
    ///
    /// Does nothing unless a replay is playing back.
    pub fn stop_replay_playback(&mut self) {
        if self.current_export.is_some() || !self.core.is_playing_back() {
            return
        }

        self.core.stop_replay_playback();
        // The user's own speed applies again (playback follows the replay's speed changes).
        self.reset_speed();
        self.current_input = Input::default();
    }

    /// Resume playing back a stopped replay (see [`Self::stop_replay_playback`]) from its resume
    /// point; whatever was played live since is discarded. The pause state is left alone.
    ///
    /// Does nothing unless a replay is stopped. Errors if the replay cannot be read there.
    pub fn resume_replay_playback(&mut self) -> Result<(), UTF8CString> {
        self.refuse_if_exporting()?;
        if !self.core.is_replay_playback_stopped() {
            return Ok(())
        }

        let result = self.core.resume_replay_playback();
        self.refresh_screen(true);
        result.map_err(|e| format!("Could not resume the replay: {e}").into())
    }

    /// Put a stopped replay (see [`Self::stop_replay_playback`]) back at its resume point without
    /// resuming playback: whatever was played live since is discarded and the user stays in
    /// control from that frame, which remains the resume point. Does nothing unless a replay is
    /// stopped. Like [`Self::go_to_replay_frame`] the seek happens on the core thread; a failure
    /// is reported by the next [`Self::tick`].
    pub fn go_to_replay_resume_point(&mut self) {
        if self.current_export.is_some() {
            return
        }
        self.core.go_to_replay_resume_point();
    }

    /// Whether the loaded replay is stopped (see [`Self::stop_replay_playback`]).
    #[inline]
    pub fn is_replay_playback_stopped(&self) -> bool {
        self.core.is_replay_playback_stopped()
    }

    /// Get the replay playback stats if a replay is loaded (playing or stopped).
    pub fn get_replay_playback_stats(&self) -> Option<SuperShuckieReplayTimes> {
        if !self.core.has_replay_attached() {
            return None;
        }

        let frames = self.core.get_playback_total_frames();
        let ms = self.core.get_playback_total_milliseconds();
        Some(SuperShuckieReplayTimes { total_milliseconds: ms, total_frames: frames })
    }

    /// What [`Self::bios_override`] would become for a replay whose BIOS checksum is `hash`,
    /// without changing anything yet (see the M3 pre-check in
    /// [`load_replay_if_exists`](Self::load_replay_if_exists)).
    fn compute_builtin_bios_override(&self, hash: ReplayHeaderBlake3Hash) -> Option<Vec<u8>> {
        let gba_bios = include_bytes!("../../bootrom/agb/gba_bios.bin");
        (hash == blake3_hash(gba_bios)).then(|| gba_bios.to_vec())
    }

    fn push_save_state_history(&mut self) {
        let Some(state) = self.create_save_state_now() else {
            return
        };

        self.current_save_state_history.truncate(self.current_save_state_history_position);
        self.current_save_state_history.push(state);

        while self.current_save_state_history.len() > self.settings.emulation.max_save_state_history.get() {
            self.current_save_state_history.remove(0);
        }

        self.current_save_state_history_position = self.current_save_state_history.len();

    }

    fn create_save_state_now(&self) -> Option<Vec<u8>> {
        self.core.create_save_state()
    }

    /// The game's save state right now, as bytes (blocks on the emulator thread; `None` without
    /// a game or when the emulator would not produce one). For tools and tests.
    pub fn create_save_state_bytes(&self) -> Option<Vec<u8>> {
        if self.current_export.is_some() || !self.is_game_running() {
            return None
        }
        self.create_save_state_now()
    }

    /// Undo loading a save state, loading the state before loading the save state.
    pub fn undo_load_save_state(&mut self) -> bool {
        if self.refuse_if_exporting().is_err() {
            return false
        }

        if self.current_save_state_history_position == 0 {
            return false // no more to go
        }

        let replay_state = self.get_replay_state();

        if self.settings.replay.disable_save_states_when_recording && replay_state == SuperShuckieReplayState::Recording {
            return false;
        }

        if self.core.is_playing_back() || self.is_link_cable_plugged() {
            return false;
        }

        let Some(backup) = self.create_save_state_now() else {
            return false
        };
        self.current_save_state_history_position -= 1;

        let history = &mut self.current_save_state_history[self.current_save_state_history_position];
        let state_to_load = std::mem::replace(history, backup);

        self.core.load_save_state(state_to_load);
        true
    }

    /// Redo loading a save state, loading the save state before undoing loading the save state.
    pub fn redo_load_save_state(&mut self) -> bool {
        if self.refuse_if_exporting().is_err() {
            return false
        }

        if self.current_save_state_history_position == self.current_save_state_history.len() {
            return false // no more to go
        }

        let replay_state = self.get_replay_state();

        if self.settings.replay.disable_save_states_when_recording && replay_state == SuperShuckieReplayState::Recording {
            return false;
        }

        if self.core.is_playing_back() || self.is_link_cable_plugged() {
            return false;
        }

        let Some(backup) = self.create_save_state_now() else {
            return false
        };

        let history = &mut self.current_save_state_history[self.current_save_state_history_position];
        self.current_save_state_history_position += 1;

        let state_to_load = std::mem::replace(history, backup);

        self.core.load_save_state(state_to_load);
        true
    }

    #[inline]
    pub fn set_touch(&mut self, at: Option<(u8, u8)>) {
        self.current_input.touch = at;
        self.core.enqueue_input(self.current_input);
    }

    pub fn on_user_input(&mut self, input: UserInput, value: f64) {
        let Some(mode) = self.emulator_type else {
            return
        };

        let controls = self.get_control_settings(mode);

        let Some(control) = (match input {
            UserInput::Keyboard { keycode } => controls.keyboard_controls.get(&keycode).copied(),
            UserInput::Button { button, controller } => {
                self.connected_controllers.get(&controller)
                    .and_then(|i| controls.controller_controls.get(i.as_str()))
                    .and_then(|i| i.buttons.get(&button))
                    .copied()
            }
            UserInput::Axis { axis, controller } => {
                self.connected_controllers.get(&controller)
                    .and_then(|i| controls.controller_controls.get(i.as_str()))
                    .and_then(|i| i.axis.get(&axis))
                    .copied()
            }
        })
        else {
            return
        };

        let pressed = value > 0.5;

        if control.control.is_button() {
            if pressed && self.settings.replay.auto_stop_playback_on_input && self.core.is_playing_back() {
                self.stop_replay_playback();
            }

            if pressed && self.settings.replay.auto_unpause_on_input && self.is_paused() {
                self.set_paused(false);
            }

            match control.modifier {
                ControlModifier::Normal => {
                    control.control.set_for_input(&mut self.current_input, pressed);
                    self.core.enqueue_input(self.current_input);
                },
                ControlModifier::Rapid => {
                    if self.current_rapid_fire_input.is_none() {
                        if !pressed {
                            return
                        }

                        let mut new_rapid_fire = SuperShuckieRapidFire::default();
                        new_rapid_fire.hold_length = unsafe { NonZeroU64::new_unchecked(3) };
                        new_rapid_fire.interval = unsafe { NonZeroU64::new_unchecked(3) };
                        self.current_rapid_fire_input = Some(new_rapid_fire);
                    }

                    let Some(input) = self.current_rapid_fire_input.as_mut() else { unreachable!("we just enabled rapid fire input...!") };
                    control.control.set_for_input(&mut input.input, pressed);
                    if !pressed && input.input.is_empty() {
                        self.current_rapid_fire_input = None;
                    }
                    self.core.set_rapid_fire_input(self.current_rapid_fire_input);
                },
                ControlModifier::Toggle => {
                    if !pressed {
                        return
                    }
                    
                    if self.current_toggled_input.is_none() {
                        self.current_toggled_input = Some(Input::new());
                    }

                    let Some(input) = self.current_toggled_input.as_mut() else { unreachable!("we just enabled toggled input...!") };
                    control.control.invert_for_input(input);
                    if !pressed && input.is_empty() {
                        self.current_toggled_input = None;
                    }
                    self.core.set_toggled_input(self.current_toggled_input);
                },
                ControlModifier::SinglePress => {
                    // Only the press counts: the core releases the button itself once the hold
                    // has run, and the key has to be let go before it can press again (the GUI
                    // does not pass on key repeats).
                    if !pressed {
                        return
                    }

                    let mut input = Input::new();
                    control.control.set_for_input(&mut input, true);
                    self.core.press_for_frames(input, ControlModifier::SINGLE_PRESS_HOLD_LENGTH);
                }
            }
        }
        else if self.is_game_running() {
            match control.control {
                Control::Turbo => {
                    if !self.settings.replay.disable_speed_changes_when_recording || self.get_replay_state() != SuperShuckieReplayState::Recording {
                        self.apply_turbo(value)
                    }
                },
                Control::Reset => if pressed {
                    self.core.hard_reset();
                }
                Control::Pause => if pressed && self.is_game_running() {
                    self.set_paused(!self.is_paused());
                }
                Control::SwapScreens => if pressed {
                    self.set_swap_nds_screens(!self.get_swap_nds_screens());
                }

                Control::A => unreachable!(),
                Control::B => unreachable!(),
                Control::Start => unreachable!(),
                Control::Select => unreachable!(),
                Control::Up => unreachable!(),
                Control::Down => unreachable!(),
                Control::Left => unreachable!(),
                Control::Right => unreachable!(),
                Control::L => unreachable!(),
                Control::R => unreachable!(),
                Control::X => unreachable!(),
                Control::Y => unreachable!(),
            }
        }
    }

    pub fn load_rom<P: AsRef<Path>>(&mut self, path: P) -> Result<(), UTF8CString> {
        let path = path.as_ref();
        let Ok(path) = absolute(path) else {
            return Err(format!("Can't resolve path {} (failed)", path.display()).into())
        };
        let Some(path_utf8) = path.to_str() else {
            return Err(format!("Can't resolve path {} (not UTF-8)", path.display()).into())
        };

        let Some(filename) = path.file_name().and_then(|i| i.to_str()) else {
            return Err(format!(
                "{} does not appear to be a valid ROM file (missing filename)",
                path.display()
            ).into())
        };

        let Some(extension) = path.extension().and_then(|i| i.to_str()) else {
            return Err(format!("{filename} does not appear to be a valid ROM file (missing extension)").into())
        };

        let data = std::fs::read(&path).map_err(|e| {
            format!("Failed to read ROM at {filename}: {e}")
        })?;

        let emulator_to_use = match extension.to_lowercase().as_str() {
            "gb" | "gbc" => self.choose_for_game_boy(data.as_slice()),
            "gba" => SuperShuckieEmulatorType::GameBoyAdvance,
            "nds" => SuperShuckieEmulatorType::NintendoDS,
            unknown => return Err(format!("Unknown or unsupported ROM file type .{unknown}").into())
        };

        self.create_userdata_for_rom(filename)?;
        self.close_rom();
        self.loaded_rom_data = Some(data);
        self.rom_name = Some(Arc::new(UTF8CString::from_str(filename)));
        self.emulator_type = Some(emulator_to_use);
        self.save_file = Some(Arc::new(self.get_current_save_file_name_for_rom(filename)));

        if let Err(e) = self.reload_core() {
            self.unload_rom();
            return Err(format!("Failed to load {filename}: {e}").into())
        }

        let path_cstr = UTF8CString::from_str(path_utf8);
        self.settings.recent_roms.recent_roms.retain(|i| i != &path_cstr);
        self.settings.recent_roms.recent_roms.insert(0, path_cstr);
        // L12: keep the list capped at the configured maximum.
        self.settings.recent_roms.clamp();
        self.last_replay_and_frame = None;
        // So another player's copy of this ROM can be found here by hash (Play Together).
        let rom_checksum = *self.core.rom_checksum();
        self.remember_rom(rom_checksum, &path);

        Ok(())
    }

    /// Get all recent ROMs.
    #[inline]
    pub fn get_recent_roms(&self) -> Vec<UTF8CString> {
        self.settings.recent_roms.recent_roms.clone()
    }

    /// Clear all recent ROMs.
    #[inline]
    pub fn clear_recent_roms(&mut self) {
        self.settings.recent_roms.recent_roms.clear();
    }

    /// Get the control settings.
    #[inline]
    pub fn get_control_settings(&self, emulator_type: SuperShuckieEmulatorType) -> &Controls {
        match emulator_type {
            SuperShuckieEmulatorType::GameBoySGB2 | SuperShuckieEmulatorType::GameBoyColor | SuperShuckieEmulatorType::GameBoy => &self.settings.game_boy_settings.controls,
            SuperShuckieEmulatorType::GameBoyAdvance => &self.settings.game_boy_advance_settings.controls,
            SuperShuckieEmulatorType::NintendoDS => &self.settings.nintendo_ds_settings.controls
        }
    }

    /// Overwrite the control settings.
    #[inline]
    pub fn set_control_settings(&mut self, controls: Controls, emulator_type: SuperShuckieEmulatorType) {
        *match emulator_type {
            SuperShuckieEmulatorType::GameBoySGB2 | SuperShuckieEmulatorType::GameBoyColor | SuperShuckieEmulatorType::GameBoy => &mut self.settings.game_boy_settings.controls,
            SuperShuckieEmulatorType::GameBoyAdvance => &mut self.settings.game_boy_advance_settings.controls,
            SuperShuckieEmulatorType::NintendoDS => &mut self.settings.nintendo_ds_settings.controls
        } = controls;
    }

    /// Hard reset the console.
    #[inline]
    pub fn hard_reset_console(&mut self) {
        if self.current_export.is_some() {
            return
        }
        self.core.hard_reset()
    }

    fn create_userdata_for_rom(&mut self, rom: &str) -> Result<(), UTF8CString> {
        fn create_if_not_dir(what: &Path) -> Result<(), UTF8CString> {
            if !what.is_dir() && let Err(e) = std::fs::create_dir(what) {
                return Err(format!("Failed to create userdata dir for {}: {e}", what.display()).into());
            }
            Ok(())
        }

        create_if_not_dir(&self.get_userdir_for_rom(rom))?;
        create_if_not_dir(&self.get_save_states_dir_for_rom(rom))?;
        create_if_not_dir(&self.get_save_data_dir_for_rom(rom))?;
        create_if_not_dir(&self.get_replays_dir_for_rom(rom))?;

        Ok(())
    }

    fn get_save_states_dir_for_rom(&self, rom: &str) -> PathBuf {
        self.get_userdir_for_rom(rom).join("save states")
    }

    fn get_save_data_dir_for_rom(&self, rom: &str) -> PathBuf {
        self.get_userdir_for_rom(rom).join("save data")
    }

    /// The replays directory of the ROM file named `rom` (also where a friend's replay goes when
    /// their game is followed from that file).
    pub fn get_replays_dir_for_rom(&self, rom: &str) -> PathBuf {
        self.get_userdir_for_rom(rom).join("replays")
    }

    fn get_screenshots_dir_for_rom(&self, rom: &str) -> PathBuf {
        self.get_userdir_for_rom(rom).join("screenshots")
    }

    /// Get the screenshots directory for the current ROM, creating it if needed.
    ///
    /// Returns `None` if no ROM is loaded or the directory could not be created.
    pub fn get_screenshots_dir_for_current_rom(&self) -> Option<UTF8CString> {
        let rom = self.get_current_rom_name()?;
        let dir = self.get_screenshots_dir_for_rom(rom);
        std::fs::create_dir_all(&dir).ok()?;
        Some(dir.to_str().expect("screenshot path is not UTF-8").into())
    }

    fn get_userdir_for_rom(&self, filename: &str) -> PathBuf {
        self.user_dir.join(format!("{filename}-data"))
    }

    #[inline]
    pub fn get_user_dir(&self) -> UTF8CString {
        self.user_dir.to_str().expect("path is not UTF-8").into()
    }

    #[inline]
    pub fn get_dir_for_current_rom(&self) -> Option<UTF8CString> {
        self.get_current_rom_name()
            .map(|rom| self.get_userdir_for_rom(rom).to_str().expect("rom path is not UTF-8").into())
    }

    #[inline]
    pub fn reload_core(&mut self) -> Result<(), UTF8CString> {
        self.refuse_if_link_cable_plugged()?;
        let emulator_type = self.emulator_type.expect("reload_rom_in_place with no emulator type");
        self.bios_override = None;
        self.instantiate_and_load_core(emulator_type)
    }

    fn instantiate_and_load_core(&mut self, emulator_type: SuperShuckieEmulatorType) -> Result<(), UTF8CString> {
        let rom_name = self.get_current_rom_name().expect("reload_rom_in_place with no loaded ROM");
        let save_file = self.get_current_save_name().expect("reload_rom_in_place with no save file");
        let save_file_data = self.get_save_file_data(rom_name, save_file);
        let rom_data = self.loaded_rom_data.as_ref().expect("reload_rom_in_place with no loaded rom");
        let core = self.make_new_core(rom_data, save_file_data, emulator_type)?;
        self.switch_core(ThreadedSuperShuckieCore::new(core));
        Ok(())
    }

    fn switch_core(&mut self, core: ThreadedSuperShuckieCore) {
        self.abort_export_and_wait();
        self.before_unload_or_reload_rom();
        let was_transferred = self.settings.pokeabyte.enabled && self.core.transfer_pokeabyte_integration(&core);
        self.assign_core(core);
        self.after_switch_core();

        self.force_refresh_screens();
        self.current_input = Input::default();
        self.set_game_speed(Speed::from_multiplier_float(self.settings.emulation.base_speed_multiplier));
        if !was_transferred && self.settings.pokeabyte.enabled {
            let _ = self.set_pokeabyte_enabled(true);
        }
    }

    fn assign_core(&mut self, new_core: ThreadedSuperShuckieCore) {
        let was_paused = self.core.is_paused();
        if was_paused {
            new_core.pause();
        }
        self.core = new_core;
        self.core_death_reported = false;
        let watch_file = self.rom_name.as_ref().map(|rom| self.get_userdir_for_rom(rom.as_str()).join("ram-watch.json"));
        self.memory_tools.core_switched(&self.core, watch_file);
    }

    fn reset_save_state_history(&mut self) {
        self.current_save_state_history = Vec::new();
        self.current_save_state_history_position = 0;
    }

    fn make_new_core(&self, rom_data: &[u8], save_file: Option<Vec<u8>>, emulator_type: SuperShuckieEmulatorType) -> Result<Box<dyn EmulatorCore>, UTF8CString> {
        let bios = self.get_bios_for_core(emulator_type);
        self.make_new_core_with_bios(rom_data, save_file, emulator_type, bios, self.settings.nintendo_ds_settings.jit)
    }

    /// Build a core for `emulator_type` with an explicit BIOS (and, for the Nintendo DS, JIT
    /// setting), e.g. one following another player's game with their BIOS.
    pub(crate) fn make_new_core_with_bios(&self, rom_data: &[u8], save_file: Option<Vec<u8>>, emulator_type: SuperShuckieEmulatorType, bios: Vec<u8>, nds_jit: bool) -> Result<Box<dyn EmulatorCore>, UTF8CString> {
        let sram = save_file.as_ref().map(|i| i.as_slice());

        let core: Box<dyn EmulatorCore> = match emulator_type {
            SuperShuckieEmulatorType::GameBoy => Box::new(GameBoyColor::new_from_rom(rom_data, bios.as_slice(), sram, Model::DmgB)),
            SuperShuckieEmulatorType::GameBoySGB2 => Box::new(GameBoyColor::new_from_rom(rom_data, bios.as_slice(), sram, Model::Sgb2)),
            SuperShuckieEmulatorType::GameBoyColor => Box::new(GameBoyColor::new_from_rom(rom_data, bios.as_slice(), sram, Model::Cgb0)),
            SuperShuckieEmulatorType::GameBoyAdvance => Box::new(
                GameBoyAdvance::new_from_rom(rom_data, sram, bios.as_slice(), std_timestamp_provider())
                    .map_err(|e| format!("mGBA rejected the ROM: {e}"))?
            ),
            SuperShuckieEmulatorType::NintendoDS => {
                let mut core = Box::new(
                    NintendoDS::new_from_rom(
                        rom_data,
                        sram,
                        std_timestamp_provider(),
                        nds_jit
                    ).map_err(|e| format!("melonDS rejected the ROM: {e}"))?
                );

                let date = self.get_nds_date().get_cleaned();
                core.set_date(
                    date.year,
                    date.month,
                    date.day,
                    date.hour,
                    date.minute,
                    date.second
                );

                core
            }
        };

        Ok(core)
    }

    fn get_current_save_file_name_for_rom(&mut self, rom: &str) -> UTF8CString {
        self.settings.get_rom_config_or_default(rom).save_name.clone()
    }

    fn get_save_file_data(&self, rom: &str, save_file: &str) -> Option<Vec<u8>> {
        std::fs::read(self.get_save_path(rom, save_file)).ok()
    }

    fn delete_save_file_data(&mut self, rom: &str, save_file: &str) {
        let _ = std::fs::remove_file(self.get_save_path(rom, save_file)).ok();
    }

    fn get_save_path(&self, rom: &str, save_file: &str) -> PathBuf {
        self.get_save_data_dir_for_rom(rom)
            .join(format!("{save_file}.{SAVE_DATA_EXTENSION}"))
    }

    fn get_bios_for_core(&self, emulator_kind: SuperShuckieEmulatorType) -> Vec<u8> {
        if let Some(s) = self.bios_override.clone() {
            return s;
        }
        self.default_bios_for(emulator_kind)
    }

    /// The BIOS/boot ROM a fresh core of `emulator_kind` gets when no replay overrides it.
    pub(crate) fn default_bios_for(&self, emulator_kind: SuperShuckieEmulatorType) -> Vec<u8> {
        match emulator_kind {
            SuperShuckieEmulatorType::GameBoy | SuperShuckieEmulatorType::GameBoySGB2 => include_bytes!("../../bootrom/dmg/dmg.bin").to_vec(),
            SuperShuckieEmulatorType::GameBoyColor => include_bytes!("../../bootrom/cgb/cgb_boot/cgb_boot_fast.bin").to_vec(),
            SuperShuckieEmulatorType::GameBoyAdvance => Vec::new(),
            SuperShuckieEmulatorType::NintendoDS => Vec::new()
        }
    }

    /// Close the ROM, saving.
    pub fn close_rom(&mut self) {
        // A running export would make the SRAM save below refuse; end it first so the save
        // actually happens (unload_rom would abort it anyway).
        self.abort_export_and_wait();
        self.save_sram_unchecked();
        self.unload_rom();
    }

    /// Unload the ROM without saving. Leaves any Play Together session: the game being
    /// published is going away.
    pub fn unload_rom(&mut self) {
        self.play_together_leave();
        self.abort_export_and_wait();
        self.before_unload_or_reload_rom();
        self.assign_core(ThreadedSuperShuckieCore::new(Box::new(NullEmulatorCore)));
        self.save_file = None;
        self.rom_name = None;
        self.emulator_type = None;
        self.current_input = Input::default();
        self.after_switch_core();
    }

    /// Set whether or not the game is paused.
    pub fn set_paused(&mut self, paused: bool) {
        if self.current_export.is_some() {
            return
        }
        if paused {
            self.core.pause();
        }
        else {
            self.core.start();
        }
    }

    /// Set whether or not the game is paused temporarily.
    pub fn set_playback_frozen(&mut self, paused: bool) {
        if self.current_export.is_some() {
            return
        }
        self.core.set_playback_frozen(paused);
    }

    /// Get whether or not the game is manually paused
    pub fn is_paused(&self) -> bool {
        self.core.is_paused()
    }

    /// Save the SRAM.
    pub fn save_sram(&mut self) -> Result<(), UTF8CString> {
        self.refuse_if_exporting()?;

        if !self.is_game_running() {
            return Err("Game not running".into())
        }

        let current_rom = self.get_current_rom_name().expect("save_sram with no current ROM");
        let current_save = self.get_current_save_name().expect("save_sram with no current save");

        let sram = self.core.get_sram().ok_or("Failed to read save data from the emulator")?;
        let save_file = self.get_save_path(current_rom, current_save);

        std::fs::write(&save_file, sram).map_err(|e| format!("Failed to write SRAM to disk: {e}").into())
    }

    fn save_sram_unchecked(&mut self) {
        let _ = self.save_sram();
    }

    /// Return `true` if a ROM is running.
    #[inline]
    pub fn is_game_running(&self) -> bool {
        self.emulator_type.is_some()
    }

    /// Calls the `refresh_screens` callback regardless of if there's a new frame.
    #[inline]
    pub fn force_refresh_screens(&mut self) {
        self.refresh_screen(true);
    }

    /// Set the video scale for the current system.
    ///
    /// This value is saved per-system.
    pub fn set_video_scale(&mut self, scale: NonZeroU8) {
        let old_scale = match self.emulator_type {
            None => return,
            Some(n) => match n {
                SuperShuckieEmulatorType::GameBoy
                | SuperShuckieEmulatorType::GameBoySGB2
                | SuperShuckieEmulatorType::GameBoyColor => &mut self.settings.game_boy_settings.video_scale,
                SuperShuckieEmulatorType::GameBoyAdvance => &mut self.settings.game_boy_advance_settings.video_scale,
                SuperShuckieEmulatorType::NintendoDS => &mut self.settings.nintendo_ds_settings.video_scale
            }
        };

        if scale == *old_scale {
            return
        }

        *old_scale = scale;
        self.update_video_mode();
    }

    /// Get the game speed settings.
    pub fn get_speed_settings(&self, base: &mut f64, turbo: &mut f64) {
        *base = self.settings.emulation.base_speed_multiplier;
        *turbo = self.settings.emulation.turbo_speed_multiplier;
    }

    /// Set the game speed.
    pub fn set_speed_settings(&mut self, mut base: f64, mut turbo: f64) {
        base = Speed::from_multiplier_float(base).into_multiplier_float();
        turbo = Speed::from_multiplier_float(turbo).into_multiplier_float();

        self.settings.emulation.base_speed_multiplier = base;
        self.settings.emulation.turbo_speed_multiplier = turbo;

        self.reset_speed();
    }

    /// Set a custom setting.
    pub fn set_custom_setting(&mut self, setting: &str, value: Option<UTF8CString>) {
        match value {
            Some(n) => { self.settings.custom.insert(setting.to_owned(), n); },
            None => { self.settings.custom.remove(setting); }
        }
    }

    /// Get the Nintendo DS date.
    #[inline]
    pub fn get_nds_date(&self) -> &NintendoDSDate {
        &self.settings.nintendo_ds_settings.date
    }

    /// Set the Nintendo DS date.
    #[inline]
    pub fn set_nds_date(&mut self, date: NintendoDSDate) {
        self.settings.nintendo_ds_settings.date = date;
    }

    /// Get the Nintendo DS date.
    #[inline]
    pub fn get_jit_enabled(&self) -> bool {
        self.settings.nintendo_ds_settings.jit
    }

    /// Set the Nintendo DS date.
    #[inline]
    pub fn set_jit_enabled(&mut self, enabled: bool) {
        if self.refuse_if_exporting().is_err() {
            return
        }

        if self.settings.nintendo_ds_settings.jit == enabled {
            return
        }
        self.settings.nintendo_ds_settings.jit = enabled;

        if self.emulator_type == Some(SuperShuckieEmulatorType::NintendoDS) {
            // L11: reload_core/assign_core mirror whatever pause state the core has when it is
            // swapped in; since we're about to force a pause below to snapshot the state, capture
            // the real pause state now so it can be restored afterwards instead of the game
            // staying paused regardless of what it was before the JIT toggle.
            let was_paused = self.is_paused();

            if let Err(e) = self.stop_recording_replay() {
                self.report_later(e.to_string());
            }
            self.close_replay();

            self.core.pause();
            let state = self.create_save_state_now();

            match self.reload_core() {
                Ok(()) => {
                    if let Some(state) = state {
                        self.core.load_save_state(state);
                    }
                    self.set_paused(was_paused);
                }
                Err(e) => {
                    self.report_later(e.to_string());
                    self.unload_rom();
                }
            }
        }

    }

    /// Get whether the DS top/bottom screens are swapped on-screen.
    #[inline]
    pub fn get_swap_nds_screens(&self) -> bool {
        self.settings.nintendo_ds_settings.swap_screens
    }

    /// Set whether the DS top/bottom screens are swapped on-screen.
    ///
    /// Re-emits the video mode so the frontend can re-lay out the screens immediately.
    pub fn set_swap_nds_screens(&mut self, swap: bool) {
        if self.settings.nintendo_ds_settings.swap_screens == swap {
            return
        }
        self.settings.nintendo_ds_settings.swap_screens = swap;

        if self.emulator_type == Some(SuperShuckieEmulatorType::NintendoDS) {
            self.update_video_mode();
        }
    }

    /// Get a custom setting.
    pub fn get_custom_setting(&self, setting: &str) -> Option<&UTF8CString> {
        self.settings.custom.get(setting)
    }

    /// Set the current save file, optionally initializing (clearing) the old one.
    ///
    /// The game will be reloaded.
    pub fn load_or_create_save_file(&mut self, save_file: &str, initialize: bool) {
        if !self.is_game_running() {
            return;
        }

        self.set_current_save_file(save_file);

        if initialize {
            let rom_name = self.get_current_rom_name_arc().expect("save file when not running");
            self.delete_save_file_data(rom_name.as_str(), save_file);
        }

        if let Err(e) = self.reload_core() {
            self.report_later(e.to_string());
            self.unload_rom();
        }
    }

    /// Set the current save file.
    ///
    /// The game will NOT be reloaded.
    pub fn set_current_save_file(&mut self, save_file: &str) {
        if !self.is_game_running() {
            return;
        }

        self.save_sram_unchecked();

        let rom_name = self.get_current_rom_name_arc().expect("save file when not running");
        self.settings.get_rom_config_or_default(rom_name.as_str()).save_name = save_file.into();
        self.save_file = Some(Arc::new(save_file.into()));
    }

    /// Handle any logic that needs to be done regularly.
    ///
    /// Returns any errors that may occur
    pub fn tick(&mut self) -> Result<(), UTF8CString> {
        let mut errors = String::new();

        // Deferred errors from calls that could not return one to their caller directly (see
        // Self::report_later).
        for e in self.deferred_errors.drain(..) {
            errors += &format!("{e}\n");
        }

        // M10: coalesce any number of settings changes made since the last tick into one write.
        if self.settings_dirty {
            self.settings_dirty = false;
            if let Err(e) = self.write_config() {
                self.report_later(e.to_string());
            }
        }

        // Errors from the atomics-driven seek path (go_to_replay_frame/advance_playback_frames)
        // and from unstalling playback, which are fire-and-forget commands with nowhere else to
        // report a failure to.
        for e in self.core.take_playback_errors() {
            errors += &format!("- REPLAY PLAYBACK ERROR: {e}\n");
        }

        if !self.core.is_alive() && !self.core_death_reported {
            self.core_death_reported = true;
            errors += "The emulator thread has stopped; the ROM was unloaded.\n";
            self.unload_rom();
        }

        if self.present_on_demand {
            // Keep the cached elapsed-time stats fresh; the frame itself waits for the UI's call.
            self.last_read_elapsed_time_stats = self.core.get_elapsed_time();
        }
        else {
            self.refresh_screen(false);
        }
        self.tick_play_together(&mut errors);

        let playing_back = self.core.is_playing_back();
        let recording = self.recording_replay_file.is_some();
        let exporting = self.current_export.is_some();
        self.memory_tools.tick(&self.core, playing_back, recording, exporting);
        if self.memory_tools.confirm_writes_while_recording() != self.settings.memory_tools.confirm_writes_while_recording {
            self.settings.memory_tools.confirm_writes_while_recording = self.memory_tools.confirm_writes_while_recording();
        }

        let replay_errors = self.core.get_replay_recording_errors();
        if !replay_errors.is_empty() {
            // A temp-sink-only failure doesn't affect the final file and doesn't stop the
            // recording (the core thread only force-stops on a non-TempSink error); report it
            // once and move on.
            let mut other = Vec::new();
            let mut temp_sink_message = None;

            for e in replay_errors {
                if matches!(e, ReplayFileWriteError::TempSink { .. }) {
                    temp_sink_message.get_or_insert_with(|| e.to_string());
                }
                else {
                    other.push(e);
                }
            }

            if let Some(message) = temp_sink_message {
                errors += &format!("- The temp copy of the recording can no longer be written ({message}); the recording continues into the final file\n");
            }

            if !other.is_empty() {
                let len_after_three = other.len().saturating_sub(3);

                for i in other.iter().take(3) {
                    errors += &format!("- REPLAY ERROR: {i}\n");
                }

                if len_after_three > 0 {
                    errors += &format!("...and {len_after_three} more replay error(s)\n");
                }

                if let Err(e) = self.stop_recording_replay() {
                    errors += &format!("{e}\n");
                }
            }
        }

        if let Some(error) = self.tick_bookmarks() {
            errors += &format!("{error}\n");
        }

        if let Some(mut s) = self.web_server.take() {
            let mut stats: OnceCell<Arc<Stats>> = OnceCell::new();
            let mut replays: OnceCell<Arc<Vec<String>>> = OnceCell::new();

            let reset_stats = |stats: &mut OnceCell<Arc<Stats>>| {
                *stats = OnceCell::new();
            };

            let make_stats = |what: &SuperShuckieFrontend, stats: &OnceCell<Arc<Stats>>| -> Arc<Stats> {
                stats.get_or_init(move || {
                    let counters = what.get_replay_counters();

                    // use cached to avoid DoSing the emulator lol
                    let stats = what.last_read_elapsed_time_stats;

                    let replay_state = what.get_replay_state();
                    let is_playing_back = replay_state == SuperShuckieReplayState::Playback;
                    let is_playback_stopped = what.core.is_replay_playback_stopped();
                    let is_recording = replay_state == SuperShuckieReplayState::Recording;
                    let is_playback_finished = what.core.is_replay_playback_finished();

                    let replay_stats = what.last_read_replay_stats.as_ref();

                    let timer_start = replay_stats.and_then(|i| i.start).map(|n| n.1.0 as u32);
                    let timer_end = replay_stats.and_then(|i| i.end).map(|n| n.1.0 as u32);
                    let timer_offset = replay_stats.and_then(|i| i.timer_offset).map(|n| n.0 as u32);

                    let mut timer_current = if let Some(start) = timer_start {
                        let value = stats.milliseconds.saturating_sub(start);

                        if let Some(end) = timer_end {
                            Some(value.min(end.saturating_sub(start)))
                        }
                        else {
                            Some(value)
                        }
                    }
                    else {
                        None
                    };

                    if let Some(c) = timer_current.as_mut() && let Some(offset) = timer_offset {
                        *c = c.wrapping_add(offset)
                    };

                    let frame_times = what.get_frame_time_stats();

                    Arc::new(Stats {
                        time_start: timer_start,
                        time_end: timer_end,
                        time_current: timer_current,
                        time_offset: timer_offset,
                        total_elapsed_time: stats.milliseconds,
                        total_elapsed_frames: stats.frames,
                        is_playing_back,
                        is_playback_stopped,
                        is_recording,
                        is_playback_finished,
                        current_speed: stats.speed.into_multiplier_float(),
                        counters,
                        is_paused: what.is_paused(),
                        emulation_fps: what.last_emulation_fps,
                        frame_time_ms: frame_times.average_frame_micros as f64 / 1000.0,
                        frame_budget_ms: frame_times.budget_micros as f64 / 1000.0,
                        frames_over_budget: frame_times.frames_over_budget,
                        bookmark_generation: what.bookmark_generation(),
                    })
                }).clone()
            };

            while let Some(s) = s.next_server_command() {
                match s {
                    SuperShuckieServerCommand::Stats(t) => {
                        let _ = t.send(make_stats(self, &stats).clone());
                    }
                    SuperShuckieServerCommand::PlayTogetherState(t) => {
                        let _ = t.send(serde_json::to_string(&self.play_together_state()).unwrap_or_else(|_| "{}".to_owned()));
                    }
                    SuperShuckieServerCommand::Bookmarks(t, request) => {
                        reset_stats(&mut stats);
                        let _ = t.send(self.handle_bookmark_request(request));
                    }
                    SuperShuckieServerCommand::MarkStart(t, timer_offset) => {
                        reset_stats(&mut stats);
                        let _ = t.send(self.mark_replay_start(TimestampMillis(timer_offset as UnsignedInteger)).is_ok());
                    }
                    SuperShuckieServerCommand::MarkEnd(t) => {
                        reset_stats(&mut stats);
                        let _ = t.send(self.mark_replay_end().is_ok());
                    }
                    SuperShuckieServerCommand::IncrementCounter(t, name, counter) => {
                        reset_stats(&mut stats);
                        // M10: an unbounded number of distinct counter names (or unbounded name
                        // length) requested over the REST API would otherwise grow the counter map
                        // without limit.
                        const MAX_COUNTER_NAME_LEN: usize = 64;
                        const MAX_COUNTERS: usize = 256;
                        let existing_counters = self.get_replay_counters();
                        let too_many_counters = !existing_counters.contains_key(&name) && existing_counters.len() >= MAX_COUNTERS;
                        if self.current_export.is_some() || name.len() > MAX_COUNTER_NAME_LEN || too_many_counters {
                            let _ = t.send(false);
                        }
                        else {
                            let _ = t.send(self.get_replay_state() == SuperShuckieReplayState::Recording);
                            self.change_replay_counter(name, counter);
                        }
                    }
                    SuperShuckieServerCommand::SetPaused(t, paused) => {
                        reset_stats(&mut stats);
                        if self.current_export.is_some() {
                            let _ = t.send(false);
                        }
                        else {
                            self.set_paused(paused);
                            let _ = t.send(true);
                        }
                    }
                    SuperShuckieServerCommand::LoadReplay(t, replay) => {
                        reset_stats(&mut stats);
                        let _ = match self.load_replay_if_exists(&replay, true) {
                            Ok(true) => t.send(true),
                            _ => t.send(false)
                        };
                    }
                    SuperShuckieServerCommand::GoToFrame(t, frame) => {
                        reset_stats(&mut stats);
                        if self.current_export.is_some() || self.get_replay_state() != SuperShuckieReplayState::Playback {
                            let _ = t.send(false);
                            continue;
                        }
                        self.go_to_replay_frame(frame);
                        let _ = t.send(true);
                    }
                    SuperShuckieServerCommand::EnumerateReplays(t) => {
                        let replays_now = replays.get_or_init(|| {
                            if let Some(n) = self.get_current_rom_name() {
                                Arc::new(self.get_all_replays_for_rom(n).iter().map(UTF8CString::to_string).collect())
                            }
                            else {
                                Arc::new(Vec::new())
                            }
                        }).clone();
                        let _ = t.send(replays_now);
                    }
                    SuperShuckieServerCommand::SetPlaybackSpeed(t, speed) => {
                        reset_stats(&mut stats);
                        let replay_state = self.get_replay_state();
                        let refuse_recording = replay_state == SuperShuckieReplayState::Recording && self.settings.replay.disable_speed_changes_when_recording;
                        let refuse_playback = self.core.is_playing_back() && !self.settings.replay.ignore_speed_changes_in_replays;
                        if self.is_game_running() && self.current_export.is_none() && !refuse_recording && !refuse_playback {
                            self.set_game_speed(Speed::from_multiplier_float(speed));
                            let _ = t.send(true);
                        }
                        else {
                            let _ = t.send(false);
                        }
                    }
                    SuperShuckieServerCommand::LoadROM(t, path) => {
                        reset_stats(&mut stats);
                        replays = OnceCell::new();
                        if self.current_export.is_some() {
                            let _ = t.send(Err("A video export is in progress; cancel it first".to_owned()));
                        }
                        else {
                            let _ = t.send(self.load_rom(&path).map_err(|e| e.to_string()));
                        }
                    }
                }
            }

            self.web_server = Some(s);
        }

        if errors.is_empty() {
            Ok(())
        }
        else {
            Err(errors.trim().into())
        }
    }

    fn refresh_screen(&mut self, force: bool) {
        let current_stats = self.core.get_elapsed_time();
        let new_frame_drawn = current_stats.screen_generation != self.last_presented_screen_generation;
        self.last_read_elapsed_time_stats = current_stats;

        // Frames that were emulated but not drawn (fast-forward) leave the screens untouched, so
        // there is nothing to upload for them.
        if !force && !new_frame_drawn {
            return
        }

        self.last_presented_screen_generation = current_stats.screen_generation;
        self.core.read_screens(|screens| {
            self.callbacks.refresh_screens(screens);
        })
    }

    /// Choose whether new frames reach the UI from `tick` as they arrive (`false`, the default) or
    /// only when the UI calls [`Self::present_latest_frame`] (`true`). A UI that calls it once per
    /// display refresh shows exactly one frame per refresh, instead of a cadence that drifts
    /// against the display and periodically doubles and skips frames.
    pub fn set_present_on_demand(&mut self, on_demand: bool) {
        self.present_on_demand = on_demand;
        self.present_pacer = PresentPacer::default();
    }

    /// Whether frames wait for [`Self::present_latest_frame`]; see [`Self::set_present_on_demand`].
    pub fn present_on_demand(&self) -> bool {
        self.present_on_demand
    }

    /// Hand the UI the newest drawn frame, if one arrived since the last one it was given. Meant
    /// to be called once per display refresh with [`Self::set_present_on_demand`] on; harmless
    /// (and redundant) otherwise.
    ///
    /// When the display refreshes an integer number of times per drawn frame (a 120 Hz display
    /// showing 60 frames per second), a frame that arrives a little early is held for the next
    /// refresh so that every frame is shown for the same number of refreshes. Otherwise the drift
    /// between the two clocks periodically puts frame arrival right at the refresh boundary, where
    /// timing jitter alone decides whether a frame shows for one refresh or three, for as long as
    /// the drift takes to move past it. A frame is never held while a newer one is already waiting.
    pub fn present_latest_frame(&mut self) {
        let now = Instant::now();
        let stats = self.core.get_elapsed_time();
        let pacer = &mut self.present_pacer;
        pacer.refreshes_since_present = pacer.refreshes_since_present.saturating_add(1);

        // Measure display refreshes per drawn frame over the last second or so.
        match pacer.window_start {
            Some(start) if now.duration_since(start) >= Duration::from_secs(1) => {
                let refreshes = pacer.window_refreshes as f64;
                let drawn = stats.screen_generation.wrapping_sub(pacer.window_start_generation) as f64;
                let ratio = if drawn > 0.0 { refreshes / drawn } else { 0.0 };
                let rounded = ratio.round();
                pacer.refreshes_per_frame = if rounded >= 2.0 && (ratio - rounded).abs() < 0.15 { rounded as u32 } else { 1 };
                pacer.window_start = Some(now);
                pacer.window_refreshes = 0;
                pacer.window_start_generation = stats.screen_generation;
            }
            Some(_) => pacer.window_refreshes += 1,
            None => {
                pacer.window_start = Some(now);
                pacer.window_refreshes = 0;
                pacer.window_start_generation = stats.screen_generation;
            }
        }

        let pending = stats.screen_generation.wrapping_sub(self.last_presented_screen_generation);
        if pending == 0 {
            self.last_read_elapsed_time_stats = stats;
            return
        }
        if pending == 1 && pacer.refreshes_since_present < pacer.refreshes_per_frame {
            // Arrived early for our cadence: show it on the next refresh instead.
            self.last_read_elapsed_time_stats = stats;
            return
        }
        pacer.refreshes_since_present = 0;
        self.refresh_screen(false);
    }

    /// Emulated frames per second, averaged over the last second or so (drawn or not). This is
    /// the real emulation rate, unlike the on-screen refresh rate which never exceeds the number
    /// of frames actually drawn.
    pub fn get_emulation_fps(&mut self) -> f64 {
        let now = Instant::now();
        let frames = self.core.get_emulated_frame_count();
        if let Some((at, count)) = self.fps_window {
            let elapsed = now.duration_since(at).as_secs_f64();
            if elapsed >= 1.0 {
                self.last_emulation_fps = frames.saturating_sub(count) as f64 / elapsed;
                self.fps_window = Some((now, frames));
            }
        }
        else {
            self.fps_window = Some((now, frames));
            self.last_emulation_fps = 0.0;
        }
        self.last_emulation_fps
    }

    /// Frame-time diagnostics from the core thread.
    #[inline]
    pub fn get_frame_time_stats(&self) -> supershuckie_core::FrameTimeStats {
        self.core.get_frame_time_stats()
    }

    fn get_current_rom_name_arc(&self) -> Option<Arc<UTF8CString>> {
        self.rom_name.clone()
    }

    /// The recorder settings derived from the replay settings (used for recording, resuming and
    /// converting replays alike).
    fn recorder_settings(&self) -> ReplayFileRecorderSettings {
        ReplayFileRecorderSettings {
            minimum_uncompressed_bytes_per_blob: (self.settings.replay.max_recording_blob_size_mb.get() as usize)
                .saturating_mul(1024)
                .saturating_mul(1024),
            compression_level: self.settings.replay.zstd_compression_level,
            max_frames_per_blob: self.settings.replay.max_frames_per_blob(),
            mask_transient_buffers: self.settings.replay.mask_transient_buffers,
        }
    }

    /// The replays directory of the current ROM, if a game is loaded (a sensible starting point
    /// for file dialogs).
    pub fn get_replays_dir_for_current_rom(&self) -> Option<PathBuf> {
        self.rom_name.as_ref().map(|rom| self.get_replays_dir_for_rom(rom.as_str()))
    }

    /// Work out what converting `path` (a replay file or a folder of them) to the current format
    /// would do, and remember it for [`start_replay_conversion`](Self::start_replay_conversion).
    ///
    /// Returns a one-line description of the plan, or an error if there is nothing to convert
    /// (already the current format, being recorded, unreadable) or a conversion is running.
    pub fn plan_replay_conversion(&mut self, path: &Path) -> Result<String, UTF8CString> {
        if self.current_conversion.is_some() {
            return Err("A replay conversion is already in progress".into());
        }

        // Never touch the recording in progress.
        let exclude: Vec<PathBuf> = self
            .recording_replay_file
            .as_ref()
            .map(|r| vec![r.final_replay_path.clone(), r.temp_replay_path.clone()])
            .unwrap_or_default();

        let plan = replay_convert::plan_conversion(path, &exclude).map_err(UTF8CString::from)?;
        if plan.files.is_empty() {
            return Err(format!("Nothing to convert. {}", plan.describe()).into());
        }

        let description = plan.describe();
        self.pending_conversion_plan = Some(plan);
        Ok(description)
    }

    /// Start converting the replays of the last [`plan_replay_conversion`](Self::plan_replay_conversion)
    /// on a background thread, with the app's replay settings.
    ///
    /// Each replay is converted into a temporary file next to it and verified; only then is the
    /// original replaced (kept as `<name>.replay.bak` when `keep_backups`). Poll with
    /// [`poll_replay_conversion`](Self::poll_replay_conversion), cancel with
    /// [`cancel_replay_conversion`](Self::cancel_replay_conversion), and collect the summary with
    /// [`poll_replay_conversion_finished`](Self::poll_replay_conversion_finished).
    pub fn start_replay_conversion(&mut self, keep_backups: bool) -> Result<(), UTF8CString> {
        if self.current_conversion.is_some() {
            return Err("A replay conversion is already in progress".into());
        }
        let Some(plan) = self.pending_conversion_plan.take() else {
            return Err("No replay conversion was planned".into());
        };
        self.current_conversion = Some(replay_convert::ConversionJob::start(plan, self.recorder_settings(), keep_backups));
        Ok(())
    }

    /// Progress of the replay conversion in progress, if any.
    pub fn poll_replay_conversion(&self) -> Option<replay_convert::ConversionStatus> {
        self.current_conversion.as_ref().map(|job| job.status())
    }

    /// Ask the replay conversion in progress to stop (after cleaning up the file it is on).
    pub fn cancel_replay_conversion(&self) {
        if let Some(job) = self.current_conversion.as_ref() {
            job.cancel();
        }
    }

    /// Non-blocking check for the end of the replay conversion.
    ///
    /// Returns `None` while it is still running (or none is active); otherwise the summary, and
    /// the job is cleared.
    pub fn poll_replay_conversion_finished(&mut self) -> Option<replay_convert::ConversionSummary> {
        if !self.current_conversion.as_ref()?.is_finished() {
            return None;
        }
        self.current_conversion.take().map(|job| job.finish())
    }

    pub fn get_current_rom_name(&self) -> Option<&str> {
        self.rom_name.as_ref().map(|i| i.as_str())
    }

    pub fn get_current_rom_name_c_str(&self) -> Option<&CStr> {
        self.rom_name.as_ref().map(|i| i.as_c_str())
    }

    pub fn get_current_save_name(&self) -> Option<&str> {
        self.save_file.as_ref().map(|i| i.as_str())
    }

    pub fn get_current_save_name_c_str(&self) -> Option<&CStr> {
        self.save_file.as_ref().map(|i| i.as_c_str())
    }

    #[inline]
    pub fn set_auto_stop_playback_on_input_setting(&mut self, new_setting: bool) {
        self.settings.replay.auto_stop_playback_on_input = new_setting
    }

    #[inline]
    pub fn get_auto_stop_playback_on_input_setting(&self) -> bool {
        self.settings.replay.auto_stop_playback_on_input
    }

    #[inline]
    pub fn set_auto_unpause_on_input_setting(&mut self, new_setting: bool) {
        self.settings.replay.auto_unpause_on_input = new_setting
    }

    #[inline]
    pub fn get_auto_unpause_on_input_setting(&self) -> bool {
        self.settings.replay.auto_unpause_on_input
    }

    #[inline]
    pub fn set_auto_pause_on_record_setting(&mut self, new_setting: bool) {
        self.settings.replay.auto_pause_on_record = new_setting
    }

    #[inline]
    pub fn get_auto_pause_on_record_setting(&self) -> bool {
        self.settings.replay.auto_pause_on_record
    }

    #[inline]
    pub fn set_auto_decompress_replays_upfront_setting(&mut self, new_setting: bool) {
        self.settings.replay.auto_decompress_replays_upfront = new_setting;
    }

    #[inline]
    pub fn get_auto_decompress_replays_upfront_setting(&self) -> bool {
        self.settings.replay.auto_decompress_replays_upfront
    }

    /// Get the number of milliseconds elapsed.
    #[inline]
    pub fn get_elapsed_milliseconds(&self) -> u32 {
        self.last_read_elapsed_time_stats.milliseconds
    }

    /// Get the number of frames elapsed.
    #[inline]
    pub fn get_elapsed_frames(&self) -> u32 {
        self.last_read_elapsed_time_stats.frames
    }

    /// Get the loaded replay's position: the frame being played back, or the frame playback
    /// resumes from while the replay is stopped (see [`Self::stop_replay_playback`]). 0 without a
    /// replay.
    #[inline]
    pub fn get_replay_frame(&self) -> u32 {
        self.last_read_elapsed_time_stats.replay_frame
    }

    /// Skip to the desired frame.
    #[inline]
    pub fn go_to_replay_frame(&mut self, frame: u32) {
        if self.current_export.is_some() {
            return
        }
        self.core.go_to_replay_frame(frame);
    }

    #[inline]
    pub fn advance_playback_frames(&mut self, delta: i32) {
        if self.current_export.is_some() {
            return
        }
        self.core.advance_playback_frames(delta)
    }

    /// Save the settings to disk now.
    ///
    /// The write is atomic (C4): the new content goes to `settings.json.tmp` first (flushed with
    /// `sync_all`), which is then renamed over `settings.json`. A crash or power loss mid-write
    /// can therefore never leave a half-written, unparsable config file behind. On failure the
    /// `.tmp` file is left in place (for recovery/diagnosis) and `settings.json` is untouched.
    pub fn write_config(&self) -> Result<(), UTF8CString> {
        let json = serde_json::to_string_pretty(&self.settings).expect("failed to serialize settings");
        let final_path = self.config_dir.join(SETTINGS_FILE);
        let tmp_path = self.config_dir.join(format!("{SETTINGS_FILE}.tmp"));

        let write_result: std::io::Result<()> = (|| {
            let mut file = File::create(&tmp_path)?;
            file.write_all(json.as_bytes())?;
            file.sync_all()
        })();

        if let Err(e) = write_result {
            return Err(format!("Failed to write {}: {e}", tmp_path.display()).into());
        }

        // This runs on the UI-driving thread (from `tick()`), so only a short retry to ride out a
        // scanner briefly holding the file; the multi-second retry used for replay conversion
        // would freeze the app.
        let mut last_error = None;
        for attempt in 0..10 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            match std::fs::rename(&tmp_path, &final_path) {
                Ok(()) => return Ok(()),
                Err(e) => last_error = Some(e)
            }
        }
        Err(format!(
            "Failed to save settings: cannot rename {} to {}: {}",
            tmp_path.display(),
            final_path.display(),
            last_error.map(|e| e.to_string()).unwrap_or_default()
        ).into())
    }

    /// Mark the settings as needing a write; `tick()` coalesces any number of these into a single
    /// write rather than hitting disk on every change (M10).
    pub(crate) fn mark_settings_dirty(&mut self) {
        self.settings_dirty = true;
    }

    fn before_unload_or_reload_rom(&mut self) {
        self.reset_save_state_history();
        if let Err(e) = self.stop_recording_replay() {
            self.report_later(e.to_string());
        }
        self.close_replay();
        self.pokeabyte_error = None;
        self.audio_output.clear();
    }

    /// Show `msg` as a `tick()` error on the next tick. For cleanup paths that cannot return an
    /// error to their caller (background housekeeping, a reload triggered indirectly by a setting
    /// change, ...).
    fn report_later(&mut self, msg: String) {
        self.deferred_errors.push(msg);
    }

    /// Start recording a replay.
    ///
    /// If `name` is set, that name will be used.
    ///
    /// Returns the name of the replay if started.
    pub fn start_recording_replay(&mut self, name: Option<&str>) -> Result<UTF8CString, UTF8CString> {
        self.refuse_if_exporting()?;
        if let Some(n) = name {
            check_user_file_name(n)?;
        }
        self.assert_replays_available()?;

        // M6: the core silently closes whatever recording/playback is already going on when a new
        // recording starts; keep our bookkeeping in sync with that before creating any files.
        if let Err(e) = self.stop_recording_replay() {
            self.report_later(e.to_string());
        }
        self.close_replay();

        let current_rom_name = self.get_current_rom_name_arc().expect("no rom name when game is running in start_recording_replay");
        let save_states_dir = self.get_replays_dir_for_rom(current_rom_name.as_str());

        let (final_file, final_replay, final_replay_path) = self.load_file_or_make_generic(&save_states_dir, name, None, REPLAY_EXTENSION)?;
        // An explicit name would give the temp file the final file's path (the generic prefix only
        // applies to generic names), and stopping deletes the temp file.
        let temp_name = name.map(|n| format!("temp-{n}"));
        let (temp_file, _, temp_replay) = self.load_file_or_make_generic(&save_states_dir, temp_name.as_deref(), Some("temp"), REPLAY_EXTENSION)?;

        self.finish_replay_bookmarks();

        if self.settings.replay.auto_pause_on_record {
            self.set_paused(true);
        }

        self.last_read_replay_stats = None;
        self.last_replay_and_frame = None;

        if let Err(e) = self.core.start_recording_replay(PartialReplayRecordMetadata {
            rom_name: current_rom_name.to_string(),
            rom_filename: current_rom_name.to_string(),

            settings: self.recorder_settings(),

            // TODO: patches
            patch_format: ReplayPatchFormat::Unpatched,
            patch_target_checksum: ReplayHeaderBlake3Hash::default(),
            patch_data: ByteVec::default(),

            frames_per_keyframe: self.settings.replay.frames_per_keyframe,

            // have a buffer so we don't destroy your SSD
            final_file: BufWriter::with_capacity(8 * 1024 * 1024, final_file),
            temp_file: BufWriter::with_capacity(8 * 1024 * 1024, temp_file),
        }) {
            let _ = std::fs::remove_file(&final_replay_path);
            let _ = std::fs::remove_file(&temp_replay);
            return Err(format!("Failed to start recording {final_replay}: {e}").into())
        }

        self.bookmarks.set_recording(replay_name_without_extension(&final_replay), Default::default());

        self.recording_replay_file = Some(ReplayFileInfo {
            final_replay_name: final_replay.clone().into(),
            temp_replay_path: temp_replay,
            final_replay_path
        });

        Ok(final_replay.into())
    }

    /// Resume recording from a saved replay.
    ///
    /// A new replay is always created; the source replay is never modified.
    ///
    /// `resume_at_frame` is the frame to resume from. `None` resumes from the end.
    ///
    /// If `new_name` is set, that name is used for the new replay; otherwise a name
    /// derived from `source_name` is generated that does not collide with the source.
    ///
    /// Returns the name of the new replay if started.
    pub fn resume_recording_from_replay(&mut self, source_name: &str, resume_at_frame: Option<u32>, new_name: Option<&str>) -> Result<UTF8CString, UTF8CString> {
        self.refuse_if_exporting()?;
        self.refuse_if_link_cable_plugged()?;
        check_user_file_name(source_name)?;
        if let Some(n) = new_name {
            check_user_file_name(n)?;
        }
        self.assert_replays_available()?;

        let current_rom_name = self.get_current_rom_name_arc().expect("no rom name when game is running in resume_recording_from_replay");
        let replays_dir = self.get_replays_dir_for_rom(current_rom_name.as_str());

        // Resolve the source path (same scheme as load_replay_if_exists). We do NOT read the file
        // here: the core builds the new file's prefix directly from the attached player below, so a
        // second in-RAM copy of a (potentially multi-gigabyte) replay is unnecessary.
        let source_path = replays_dir.join(format!("{source_name}.{REPLAY_EXTENSION}"));
        if !source_path.is_file() {
            return Err(format!("Replay {source_name} does not exist").into());
        }

        // Attach the source replay for playback so the emulator can be positioned at the
        // resume point. This also runs the ROM/BIOS/core compatibility checks. Allow
        // mismatches/corruption (override_errors = true) so resuming is permissive.
        self.load_replay_if_exists(source_name, true)?;

        if self.current_replay_truncated {
            return Err(format!("Replay {source_name} is truncated or damaged; it can be watched but not resumed").into());
        }

        // Choose the new replay name, guaranteeing it never collides with the source.
        let new_name: String = match new_name {
            Some(n) => n.to_owned(),
            None => {
                let base = format!("{source_name}-resume");
                let mut candidate = base.clone();
                let mut i = 0u64;
                loop {
                    let path = replays_dir.join(format!("{candidate}.{REPLAY_EXTENSION}"));
                    if path != source_path && !path.exists() {
                        break;
                    }
                    i = i.checked_add(1).ok_or_else(|| UTF8CString::from_str("Maximum number of generics reached."))?;
                    candidate = format!("{base}-{i}");
                }
                candidate
            }
        };

        // Never write the source path.
        let intended_final_path = replays_dir.join(format!("{new_name}.{REPLAY_EXTENSION}"));
        if intended_final_path == source_path {
            return Err("The new replay name must not be the same as the source replay".into());
        }

        // Open both new files. The temp file MUST have a distinct path from the final file:
        // load_file_or_make_generic ignores the generic prefix when an explicit name is given, so
        // passing Some(new_name) to both would collapse them onto the same path — which then gets
        // deleted on stop (it removes the temp file), destroying the recording. Build an explicit,
        // distinct temp name instead.
        let temp_name = format!("temp-{new_name}");
        let (final_file, final_replay, final_replay_path) = self.load_file_or_make_generic(&replays_dir, Some(new_name.as_str()), None, REPLAY_EXTENSION)?;
        let (temp_file, _, temp_replay) = self.load_file_or_make_generic(&replays_dir, Some(temp_name.as_str()), None, REPLAY_EXTENSION)?;

        let partial = PartialReplayRecordMetadata {
            rom_name: current_rom_name.to_string(),
            rom_filename: current_rom_name.to_string(),

            settings: self.recorder_settings(),

            // TODO: patches
            patch_format: ReplayPatchFormat::Unpatched,
            patch_target_checksum: ReplayHeaderBlake3Hash::default(),
            patch_data: ByteVec::default(),

            frames_per_keyframe: self.settings.replay.frames_per_keyframe,

            // have a buffer so we don't destroy your SSD
            final_file: BufWriter::with_capacity(8 * 1024 * 1024, final_file),
            temp_file: BufWriter::with_capacity(8 * 1024 * 1024, temp_file),
        };

        // The new replay starts with the source's bookmarks (including unsaved changes, which are
        // saved into the source as well) up to the resume frame.
        let resume_frame = resume_at_frame.map(|f| f as u64).unwrap_or(self.core.get_playback_total_frames() as u64);
        let bookmarks = self.bookmarks.table().truncated_to(resume_frame);
        if let Err(e) = self.flush_bookmarks() {
            self.bookmarks_report(format!("Bookmark changes to {source_name} were not saved: {e}"));
        }

        if let Err(e) = self.core.resume_recording_replay(
            resume_at_frame.map(|f| f as u64),
            partial,
            ResumeCropPolicy::PreserveStartDropEnd,
            Some(bookmarks.clone()),
        ) {
            // The core restored the source player on failure (still attached and playing back);
            // only the two files we just created for the new recording need cleaning up.
            let _ = std::fs::remove_file(&final_replay_path);
            let _ = std::fs::remove_file(&temp_replay);
            return Err(format!("Failed to resume from {source_name}: {e}").into())
        }

        self.bookmarks.set_recording(new_name.clone(), bookmarks);

        // load_replay_if_exists force-paused the game for playback positioning. Now that we're
        // recording live, honor auto_pause_on_record: pause if set, otherwise hand control back.
        self.set_paused(self.settings.replay.auto_pause_on_record);

        // Preserve the source replay's timer markers so the on-screen timer keeps reading the
        // resumed value (e.g. 35.83) instead of restarting at 0. load_replay_if_exists populated
        // last_read_replay_stats from the source metadata; mirror the recorder's
        // PreserveStartDropEnd crop policy: keep crop_start + timer_offset when the start lies
        // within the kept prefix, and drop crop_end (the run is being extended past it).
        if let Some(stats) = self.last_read_replay_stats.as_mut() {
            let keep_start = match (stats.start, resume_at_frame) {
                (Some((start_frame, _)), Some(resume_frame)) => start_frame <= resume_frame as u64,
                (Some(_), None) => true, // resuming from the end: the start is always within the prefix
                (None, _) => false,
            };
            if !keep_start {
                stats.start = None;
                stats.timer_offset = None;
            }
            stats.end = None;
        }
        self.last_replay_and_frame = None;
        self.current_replay = None;
        self.current_replay_truncated = false;
        self.current_input = Input::default();

        // L9: the "replay" scratch save file only applies while a replay is attached for
        // playback; live recording after a resume saves to the ROM's configured save file again.
        if let Some(rom_name) = self.get_current_rom_name() {
            let rom_name = rom_name.to_owned();
            self.save_file = Some(Arc::new(self.get_current_save_file_name_for_rom(&rom_name)));
        }

        self.recording_replay_file = Some(ReplayFileInfo {
            final_replay_name: final_replay.clone().into(),
            temp_replay_path: temp_replay,
            final_replay_path
        });

        Ok(final_replay.into())
    }

    /// Resume recording from the replay currently being watched, continuing from the frame it is
    /// currently playing back at.
    ///
    /// A new, separate replay is always created; the source replay is never modified. Errors if no
    /// replay is currently being played back.
    ///
    /// Returns the name of the new replay if started.
    pub fn resume_recording_from_current_replay(&mut self) -> Result<UTF8CString, UTF8CString> {
        let Some(name) = self.current_replay.clone() else {
            return Err("No replay is currently being watched".into());
        };

        // Capture the playback frame BEFORE resuming (the resume re-attaches the source player,
        // which would otherwise reset the position). For a stopped replay that is its resume
        // point, not wherever live play has got to since.
        let frame = self.core.get_elapsed_time().replay_frame;

        self.resume_recording_from_replay(name.as_str(), Some(frame), None)
    }

    /// Export the named replay (for the current ROM) to a video file at `output_path`.
    ///
    /// `range` is an inclusive-start, exclusive-end `(start, end)` frame range. When `None`, the
    /// replay's crop range is used if present, otherwise the entire replay.
    ///
    /// `preset`, `scale`, and `layout` control the encoding and (for multi-screen consoles) the
    /// screen composition. Encoding happens by spawning `ffmpeg` and piping frames to it; the
    /// returned [`VideoExportHandle`] can be polled for progress, cancelled, or waited on. Note
    /// that errors from the encoder (e.g. ffmpeg missing) surface through that handle, not here.
    pub fn export_replay_video(
        &mut self,
        replay_name: &str,
        range: Option<(u32, u32)>,
        output_path: &Path,
        preset: ExportPreset,
        scale: NonZeroU8,
        layout: ScreenLayout,
    ) -> Result<VideoExportHandle, UTF8CString> {
        self.refuse_if_link_cable_plugged()?;
        self.assert_replays_available()?;
        check_user_file_name(replay_name)?;

        let current_rom_name = self.get_current_rom_name_arc().expect("no rom name when game is running in export_replay_video");
        let replays_dir = self.get_replays_dir_for_rom(current_rom_name.as_str());

        // Resolve + read the source replay (same path scheme as load_replay_if_exists). We parse it
        // here only to determine the default export range from the header crop markers.
        let replay_path = replays_dir.join(format!("{replay_name}.{REPLAY_EXTENSION}"));
        if !replay_path.is_file() {
            return Err(format!("Replay {replay_name} does not exist").into());
        }

        let export_range = match range {
            Some((start, end)) => ExportRange {
                start_frame: start as u64,
                end_frame: Some(end as u64)
            },
            None => {
                let bytes = match std::fs::read(&replay_path) {
                    Ok(n) => n,
                    Err(e) => return Err(format!("Failed to read replay {replay_name}:\n\n{e}").into())
                };
                let player = match ReplayFilePlayer::new(bytes, true) {
                    Ok(n) => n,
                    Err(e) => return Err(format!("Failed to parse replay {replay_name}:\n\n{e:?}").into())
                };
                let metadata = player.get_replay_metadata();
                let start = metadata.crop_start.map(|c| c.0).unwrap_or(0);
                let end = metadata.crop_end.map(|c| c.0);
                ExportRange { start_frame: start, end_frame: end }
            }
        };

        // Ensure the replay is attached for playback (this also runs the ROM/BIOS/core
        // compatibility checks). The exporter reuses the attached player.
        self.load_replay_if_exists(replay_name, true)?;

        let sink = FfmpegVideoSink::new(
            self.settings.export.ffmpeg_path.clone(),
            output_path.to_path_buf(),
            preset,
            scale,
            self.settings.export.default_crf,
        );

        let handle = self.core.export_replay(Box::new(sink), export_range, layout);
        Ok(handle)
    }

    /// Begin a video export and retain the handle internally (for the C/Qt layer).
    ///
    /// Only one export may be in progress at a time; starting another while one is running returns
    /// an error. Poll with [`poll_export_progress`](Self::poll_export_progress), finish with
    /// [`poll_export_finished`](Self::poll_export_finished), and cancel with
    /// [`cancel_export`](Self::cancel_export).
    pub fn start_replay_video_export(
        &mut self,
        replay_name: &str,
        range: Option<(u32, u32)>,
        output_path: &Path,
        preset: ExportPreset,
        scale: NonZeroU8,
        layout: ScreenLayout,
    ) -> Result<(), UTF8CString> {
        if self.current_export.is_some() {
            return Err("An export is already in progress".into());
        }
        let handle = self.export_replay_video(replay_name, range, output_path, preset, scale, layout)?;
        self.current_export = Some(handle);
        Ok(())
    }

    /// Return `true` if a video export is currently in progress.
    #[inline]
    pub fn is_exporting(&self) -> bool {
        self.current_export.is_some()
    }

    /// Refuse the caller's request while a video export is in progress (the export owns the core
    /// thread for its duration, so anything that would block on it, or change state it depends
    /// on, has to wait).
    fn refuse_if_exporting(&self) -> Result<(), UTF8CString> {
        if self.current_export.is_some() {
            return Err("A video export is in progress; cancel it first".into())
        }
        Ok(())
    }

    /// Refuse the caller's request while a Play Together link cable is plugged in (or going in):
    /// the two linked games have to stay in step, so nothing may change one of them behind the
    /// other's back.
    fn refuse_if_link_cable_plugged(&self) -> Result<(), UTF8CString> {
        if self.is_link_cable_plugged() {
            return Err("Unplug the link cable first.".into())
        }
        Ok(())
    }

    /// Cancel any export in progress and block until it stops. Used before operations (unloading
    /// or reloading the ROM) that would otherwise block on the core thread, which the export owns
    /// for its duration.
    fn abort_export_and_wait(&mut self) {
        self.cancel_export();
        if let Some(handle) = self.current_export.take() {
            let _ = handle.wait();
        }
    }

    /// Current export progress as `(frames_done, frames_total)`, or `None` if not exporting.
    pub fn poll_export_progress(&self) -> Option<(u64, u64)> {
        self.current_export.as_ref().map(|h| h.progress())
    }

    /// Request cancellation of the in-progress export, if any.
    pub fn cancel_export(&self) {
        if let Some(h) = self.current_export.as_ref() {
            h.cancel();
        }
    }

    /// Non-blocking check for export completion.
    ///
    /// Returns `None` while still running (or if no export is active). When the export finishes it
    /// returns `Some(Ok(()))` on success or `Some(Err(message))` on failure, and clears the handle.
    pub fn poll_export_finished(&mut self) -> Option<Result<(), UTF8CString>> {
        let done = self.current_export.as_ref()?.poll_done()?;
        self.current_export = None;
        Some(done.map_err(|e| format!("Video export failed: {e}").into()))
    }

    /// Get the configured path to the `ffmpeg` binary used for video exports.
    #[inline]
    pub fn get_export_ffmpeg_path(&self) -> &str {
        self.settings.export.ffmpeg_path.as_str()
    }

    /// Set the path to the `ffmpeg` binary used for video exports.
    #[inline]
    pub fn set_export_ffmpeg_path(&mut self, p: String) {
        self.settings.export.ffmpeg_path = p;
    }

    /// Get the default integer upscale factor for video exports.
    #[inline]
    pub fn get_export_default_scale(&self) -> NonZeroU8 {
        self.settings.export.default_scale
    }

    /// Set the default integer upscale factor for video exports.
    #[inline]
    pub fn set_export_default_scale(&mut self, s: NonZeroU8) {
        self.settings.export.default_scale = s;
    }

    /// Get the default H.264 CRF for video exports.
    #[inline]
    pub fn get_export_default_crf(&self) -> u8 {
        self.settings.export.default_crf
    }

    /// Set the default H.264 CRF for video exports.
    #[inline]
    pub fn set_export_default_crf(&mut self, c: u8) {
        self.settings.export.default_crf = c;
    }

    /// Get the default encoding preset for video exports.
    #[inline]
    pub fn get_export_default_preset(&self) -> ExportPreset {
        self.settings.export.default_preset.clone()
    }

    /// Set the default encoding preset for video exports.
    #[inline]
    pub fn set_export_default_preset(&mut self, p: ExportPreset) {
        self.settings.export.default_preset = p;
    }

    /// Get the default output directory for video exports, or `None` to use the replays directory.
    #[inline]
    pub fn get_export_output_dir(&self) -> Option<&Path> {
        self.settings.export.output_dir.as_deref()
    }

    /// Set the default output directory for video exports. `None` uses the replays directory.
    #[inline]
    pub fn set_export_output_dir(&mut self, d: Option<PathBuf>) {
        self.settings.export.output_dir = d;
    }

    fn assert_replays_available(&self) -> Result<(), UTF8CString> {
        let Some(emulator_type) = self.emulator_type else {
            return Err("No ROM loaded".into());
        };

        if !self.is_game_running() {
            return Err("No game running".into());
        }

        if self.settings.nintendo_ds_settings.jit && emulator_type == SuperShuckieEmulatorType::NintendoDS {
            return Err("Replays are disabled for Nintendo DS (JIT enabled)".into());
        }

        Ok(())
    }

    /// Stop recording replay.
    ///
    /// Returns `Err` if the recording could not be finalised (its final file may be missing its
    /// last part); the temp file is then kept rather than deleted, so it can be recovered.
    pub fn stop_recording_replay(&mut self) -> Result<(), UTF8CString> {
        let Some(replay_file) = self.recording_replay_file.take() else {
            return Ok(())
        };

        let zero_frames = self.core.get_elapsed_time().frames == 0;

        self.last_read_replay_stats = None;

        // The recording's bookmarks are written when it closes; hand over the latest first.
        self.push_bookmarks_to_core();
        self.bookmarks.clear();

        let closed_ok = self.core.stop_recording_replay();

        if closed_ok {
            let _ = std::fs::remove_file(&replay_file.temp_replay_path);

            if zero_frames {
                let _ = std::fs::remove_file(&replay_file.final_replay_path);
            }

            Ok(())
        }
        else {
            Err(format!(
                "The recording {} could not be finalised; its last part may be missing. The temp file {} was kept so it can be recovered.",
                replay_file.final_replay_path.file_name().expect("no final file filename???").display(),
                replay_file.temp_replay_path.file_name().expect("no temp file filename???").display()
            ).into())
        }
    }

    #[inline]
    pub fn continue_last_replay(&mut self) -> Result<bool, UTF8CString> {
        let Some((name, frame)) = self.last_replay_and_frame.take() else {
            return Ok(false)
        };

        self.load_replay_if_exists(name.as_str(), true)?;
        let current_frame = self.core.get_elapsed_time().frames;

        if frame == current_frame {
            return Ok(true)
        }

        self.core.pause();
        self.core.go_to_replay_frame(frame);
        self.core.rendezvous();
        self.refresh_screen(true);
        Ok(true)
    }

    #[inline]
    pub fn can_continue_last_replay(&self) -> bool {
        self.last_replay_and_frame.is_some()
    }

    /// Get all saves for the given ROM.
    #[inline]
    pub fn get_all_saves_for_rom(&self, rom: &str) -> Vec<UTF8CString> {
        list_files_in_dir_with_extension(&self.get_save_data_dir_for_rom(rom), SAVE_DATA_EXTENSION)
    }

    /// Get all save states for the given ROM.
    #[inline]
    pub fn get_all_save_states_for_rom(&self, rom: &str) -> Vec<UTF8CString> {
        list_files_in_dir_with_extension(&self.get_save_states_dir_for_rom(rom), SAVE_STATE_EXTENSION)
    }

    /// Get all replays for the given ROM.
    #[inline]
    pub fn get_all_replays_for_rom(&self, rom: &str) -> Vec<UTF8CString> {
        list_files_in_dir_with_extension(&self.get_replays_dir_for_rom(rom), REPLAY_EXTENSION)
    }

    /// Set whether or not speed changes in replays are ignored
    #[inline]
    pub fn set_ignore_speed_changes_in_replays(&mut self, ignored: bool) {
        self.settings.replay.ignore_speed_changes_in_replays = ignored;
        self.core.set_ignore_speed_changes_in_replay(ignored);
        self.reset_speed();
    }

    /// Get whether or not speed changes in replays are ignored.
    #[inline]
    pub fn get_ignore_speed_changes_in_replays(&self) -> bool {
        self.settings.replay.ignore_speed_changes_in_replays
    }

    /// Set whether or not keyframes are automatically resynced on playback.
    #[inline]
    pub fn set_auto_resync_keyframes_in_replay(&mut self, resync: bool) {
        self.settings.replay.auto_resync_keyframes_in_replays = resync;
        self.core.set_auto_resync_keyframes_in_replay(resync);
    }

    /// Get whether or not keyframes are automatically resynced on playback.
    #[inline]
    pub fn get_auto_resync_keyframes_in_replay(&self) -> bool {
        self.settings.replay.auto_resync_keyframes_in_replays
    }

    /// Get the zstd compression level used for new recordings and replay conversions.
    #[inline]
    pub fn get_replay_compression_level(&self) -> i32 {
        self.settings.replay.zstd_compression_level
    }

    /// Set the zstd compression level used for new recordings and replay conversions (clamped to
    /// zstd's 1..=22; existing files are not touched).
    pub fn set_replay_compression_level(&mut self, level: i32) {
        self.settings.replay.zstd_compression_level = level.clamp(1, 22);
    }

    /// Set whether or not save states can be created/loading when recording replays.
    #[inline]
    pub fn set_disable_save_states_when_recording(&mut self, disabled: bool) {
        self.settings.replay.disable_save_states_when_recording = disabled;
    }

    /// Set whether or not save states can be created/loading when recording replays.
    #[inline]
    pub fn get_disable_save_states_when_recording(&self) -> bool {
        self.settings.replay.disable_save_states_when_recording
    }

    /// Get whether or not turbo works when recording replays.
    #[inline]
    pub fn set_disable_speed_changes_when_recording(&mut self, disabled: bool) {
        self.settings.replay.disable_speed_changes_when_recording = disabled;
        self.reset_speed();
    }

    /// Get whether or not turbo works when recording replays.
    #[inline]
    pub fn get_disable_speed_changes_when_recording(&self) -> bool {
        self.settings.replay.disable_speed_changes_when_recording
    }

    /// Set whether a dragged timeline shows the nearest keyframe rather than the exact frame
    /// until the drag ends (see `ReplaySettings::snap_timeline_drag_to_keyframes`).
    #[inline]
    pub fn set_snap_timeline_drag_to_keyframes(&mut self, snap: bool) {
        self.settings.replay.snap_timeline_drag_to_keyframes = snap;
        self.core.set_coarse_seek_while_frozen(snap);
    }

    /// See [`Self::set_snap_timeline_drag_to_keyframes`].
    #[inline]
    pub fn get_snap_timeline_drag_to_keyframes(&self) -> bool {
        self.settings.replay.snap_timeline_drag_to_keyframes
    }

    fn after_switch_core(&mut self) {
        if self.settings.replay.ignore_speed_changes_in_replays {
            self.core.set_ignore_speed_changes_in_replay(true);
        }
        if self.settings.replay.auto_resync_keyframes_in_replays {
            self.core.set_auto_resync_keyframes_in_replay(true);
        }
        self.core.set_coarse_seek_while_frozen(self.settings.replay.snap_timeline_drag_to_keyframes);

        // A new core is a new timeline; nothing the old one queued should be heard.
        self.audio_output.clear();
        self.core.set_audio_output(Some(self.audio_output.clone()));
        self.core.set_audio_mute_when_sped_up(self.settings.audio.mute_when_sped_up);
        self.core.set_audio_enabled(self.settings.audio.enabled);

        self.update_video_mode();
        // A reloaded game is still the one being played together; the followers get a snapshot.
        self.republish_after_core_switch();
    }

    /// The ring the audio device reads from. Stable for the life of the frontend.
    #[inline]
    pub fn audio_output(&self) -> &Arc<AudioOutput> {
        &self.audio_output
    }

    /// Whether audio is rendered and handed to the device.
    #[inline]
    pub fn get_audio_enabled(&self) -> bool {
        self.settings.audio.enabled
    }

    /// Turn audio on or off (off by default).
    pub fn set_audio_enabled(&mut self, enabled: bool) {
        if self.settings.audio.enabled == enabled {
            return
        }
        self.settings.audio.enabled = enabled;
        self.core.set_audio_enabled(enabled);
        if !enabled {
            self.audio_output.clear();
        }
    }

    /// Whether playback is muted (audio keeps being rendered; the device plays it at zero gain).
    #[inline]
    pub fn get_audio_muted(&self) -> bool {
        self.settings.audio.muted
    }

    /// Mute or unmute playback.
    #[inline]
    pub fn set_audio_muted(&mut self, muted: bool) {
        self.settings.audio.muted = muted;
    }

    /// Volume in percent, 0..=100.
    #[inline]
    pub fn get_audio_volume(&self) -> u8 {
        self.settings.audio.volume
    }

    /// Set the volume in percent (clamped to 100).
    #[inline]
    pub fn set_audio_volume(&mut self, volume: u8) {
        self.settings.audio.volume = volume.min(AudioSettings::MAX_VOLUME);
    }

    /// Whether audio stays silent while the game runs at a speed other than 1x.
    #[inline]
    pub fn get_audio_mute_when_sped_up(&self) -> bool {
        self.settings.audio.mute_when_sped_up
    }

    /// Set whether audio stays silent while the game runs at a speed other than 1x.
    pub fn set_audio_mute_when_sped_up(&mut self, mute: bool) {
        if self.settings.audio.mute_when_sped_up == mute {
            return
        }
        self.settings.audio.mute_when_sped_up = mute;
        self.core.set_audio_mute_when_sped_up(mute);
    }

    /// Most audio queued ahead of the device, in milliseconds.
    #[inline]
    pub fn get_audio_latency_ms(&self) -> u16 {
        self.settings.audio.latency_ms
    }

    /// Set how much audio may queue ahead of the device (clamped to a sane range).
    pub fn set_audio_latency_ms(&mut self, latency_ms: u16) {
        let latency_ms = latency_ms.clamp(AudioSettings::MIN_LATENCY_MS, AudioSettings::MAX_LATENCY_MS);
        self.settings.audio.latency_ms = latency_ms;
        self.audio_output.set_max_latency_ms(latency_ms as u32);
    }

    fn update_video_mode(&mut self) {
        let video_scale = match self.emulator_type {
            None => unsafe { NonZeroU8::new_unchecked(4) },
            Some(n) => match n {
                SuperShuckieEmulatorType::GameBoy
                | SuperShuckieEmulatorType::GameBoySGB2
                | SuperShuckieEmulatorType::GameBoyColor => self.settings.game_boy_settings.video_scale,
                SuperShuckieEmulatorType::GameBoyAdvance => self.settings.game_boy_advance_settings.video_scale,
                SuperShuckieEmulatorType::NintendoDS => self.settings.nintendo_ds_settings.video_scale
            }
        };

        // Collect the (read-only) screen geometry while the lock is held, then release it before
        // calling back into the frontend: `change_video_mode` is allowed to call read-only getters
        // on `self`, which would deadlock (or, since the mutex isn't reentrant-safe here, panic)
        // if it ran with the screens lock still held and `&mut self` live (see M13).
        let infos: Vec<ScreenInfo> = self.core.read_screens(|screens| {
            screens.iter().map(|s| ScreenInfo { width: s.width, height: s.height, encoding: s.encoding }).collect()
        });

        self.callbacks.change_video_mode(&infos, video_scale);
    }

    #[inline]
    fn reset_speed(&mut self) {
        self.apply_turbo(0.0);
    }

    fn apply_turbo(&mut self, turbo: f64) {
        if !self.settings.replay.ignore_speed_changes_in_replays && self.core.is_playing_back() {
            return
        }

        let base_speed = self.settings.emulation.base_speed_multiplier;
        let max_speed = self.settings.emulation.turbo_speed_multiplier * base_speed;
        let total_speed = base_speed + (max_speed - base_speed) * turbo;
        self.set_game_speed(Speed::from_multiplier_float(total_speed));
    }

    /// The player's own speed controls: set the game's speed, unless a link cable is in and the
    /// session host's speed rules (see [`play_together::link`]). As the host, the speed is also
    /// told to the session, so every linked pair follows it.
    fn set_game_speed(&mut self, speed: Speed) {
        if self.link_speed_is_the_hosts() {
            return
        }
        self.set_core_speed(speed);
        self.push_link_speed_to_session();
    }

    /// Set the core's speed and remember it.
    fn set_core_speed(&mut self, speed: Speed) {
        self.current_speed = speed;
        self.core.set_speed(speed);
    }

    #[inline]
    /// Get the replay file info, or `None` if not recording.
    pub fn get_replay_file_info(&self) -> Option<&ReplayFileInfo> {
        self.recording_replay_file.as_ref()
    }

    /// Returns true if PokeAByte is enabled, false if not, or an error if there was an error starting it.
    pub fn is_pokeabyte_enabled(&self) -> Result<bool, &UTF8CString> {
        match self.pokeabyte_error.as_ref() {
            Some(e) => Err(e),
            None => Ok(self.settings.pokeabyte.enabled)
        }
    }

    /// Set whether or not the Poke-A-Byte integration server is enabled (the player's own game
    /// on the configured port and, in a Play Together session, every friend's game on a port of
    /// its own; see [`Self::set_pokeabyte_serve_friends`]).
    pub fn set_pokeabyte_enabled(&mut self, enabled: bool) -> Result<(), &UTF8CString> {
        self.settings.pokeabyte.enabled = enabled;
        self.pokeabyte_error = None;
        let result = self.core.set_pokeabyte_port(enabled.then_some(self.settings.pokeabyte.port));
        self.play_together_apply_pokeabyte_setting();
        match result {
            Ok(_) => Ok(()),
            Err(e) => {
                self.pokeabyte_error = Some(e.into());
                Err(self.pokeabyte_error.as_ref().expect("pokeabyte_error was just set earlier..."))
            }
        }
    }

    /// The UDP port the player's own game is served to Poke-A-Byte on.
    #[inline]
    pub fn get_pokeabyte_port(&self) -> u16 {
        self.settings.pokeabyte.port
    }

    /// Serve the player's own game to Poke-A-Byte on `port` from now on. Friends' games keep the
    /// ports they were given; new ones are placed above the new port.
    pub fn set_pokeabyte_port(&mut self, port: u16) -> Result<(), UTF8CString> {
        if port == 0 {
            return Err("Port 0 is not a Poke-A-Byte port.".into())
        }
        if port == self.settings.pokeabyte.port {
            return Ok(())
        }
        self.settings.pokeabyte.port = port;
        self.mark_settings_dirty();
        if !self.settings.pokeabyte.enabled {
            return Ok(())
        }
        self.pokeabyte_error = None;
        match self.core.set_pokeabyte_port(Some(port)) {
            Ok(()) => Ok(()),
            Err(e) => {
                let e = UTF8CString::from(e);
                self.pokeabyte_error = Some(e.clone());
                Err(e)
            }
        }
    }

    /// Whether friends' games in a Play Together session are served to Poke-A-Byte too.
    #[inline]
    pub fn get_pokeabyte_serve_friends(&self) -> bool {
        self.settings.pokeabyte.serve_friends
    }

    /// Serve (or stop serving) friends' games to Poke-A-Byte, each on the lowest free port above
    /// the player's own; applies to the friends already in the session as well.
    pub fn set_pokeabyte_serve_friends(&mut self, serve: bool) {
        if self.settings.pokeabyte.serve_friends == serve {
            return
        }
        self.settings.pokeabyte.serve_friends = serve;
        self.mark_settings_dirty();
        self.play_together_apply_pokeabyte_setting();
    }

    /// Returns true if external commands are enabled, false if not, or an error if there was an error starting it.
    pub fn get_external_commands_enabled(&self) -> Result<bool, &UTF8CString> {
        match self.external_commands_error.as_ref() {
            Some(e) => Err(e),
            None => Ok(self.settings.external_commands.enabled)
        }
    }

    /// Set whether or not external commands are enabled, returning an error if there was an error starting it.
    pub fn set_external_commands_enabled(&mut self, enabled: bool) -> Result<(), &UTF8CString> {
        self.settings.external_commands.enabled = enabled;
        self.external_commands_error = None;
        if !enabled {
            self.web_server = None;
            return Ok(())
        }
        if self.web_server.is_some() {
            return Ok(())
        }
        match SuperShuckieWebserver::new("127.0.0.1:30158") {
            Ok(n) => {
                self.web_server = Some(n);
                Ok(())
            },
            Err(e) => {
                self.external_commands_error = Some(e.into());
                Err(self.external_commands_error.as_ref().expect("??? we just set it"))
            }
        }
    }

    /// The RAM tools' state.
    #[inline]
    pub fn memory_tools(&self) -> &memory_tools::MemoryTools {
        &self.memory_tools
    }

    /// The RAM tools' state, with the running core for operations that reach it.
    #[inline]
    pub fn memory_tools_mut(&mut self) -> (&mut memory_tools::MemoryTools, &ThreadedSuperShuckieCore) {
        (&mut self.memory_tools, &self.core)
    }

    #[inline]
    pub fn get_replay_state(&self) -> SuperShuckieReplayState {
        if self.get_replay_playback_stats().is_some() {
            SuperShuckieReplayState::Playback
        }
        else if self.get_replay_file_info().is_some() {
            SuperShuckieReplayState::Recording
        }
        else {
            SuperShuckieReplayState::NoReplay
        }
    }

    #[inline]
    pub fn get_gbc_mode(&self) -> GameBoyMode {
        self.settings.game_boy_settings.gbc_mode
    }

    #[inline]
    pub fn set_gbc_mode(&mut self, mode: GameBoyMode) {
        self.settings.game_boy_settings.gbc_mode = mode;
        self.reload_game_boy_if_needed();
    }

    #[inline]
    pub fn is_sgb_enabled(&self) -> bool {
        self.settings.game_boy_settings.sgb
    }

    #[inline]
    pub fn set_sgb_enabled(&mut self, enabled: bool) {
        self.settings.game_boy_settings.sgb = enabled;
        self.reload_game_boy_if_needed();
    }

    #[inline]
    pub fn get_emulator_type(&self) -> Option<SuperShuckieEmulatorType> {
        self.emulator_type
    }

    fn reload_game_boy_if_needed(&mut self) {
        let current = match self.emulator_type {
            Some(n) if matches!(n, SuperShuckieEmulatorType::GameBoy | SuperShuckieEmulatorType::GameBoyColor | SuperShuckieEmulatorType::GameBoySGB2) => n,
            _ => return
        };

        let Some(rom) = self.loaded_rom_data.as_ref() else {
            panic!("emulator_type is non-None but we have no loaded rom data???")
        };

        let expected = self.choose_for_game_boy(rom.as_slice());

        if expected != current {
            if self.play_together.is_some() {
                self.report_later(String::from("The Game Boy model cannot change during a Play Together session; the setting applies after you leave."));
                return
            }
            self.emulator_type = Some(expected);
            if let Err(e) = self.reload_core() {
                self.report_later(e.to_string());
                self.unload_rom();
            }
        }
    }

    fn choose_for_game_boy(&self, data: &[u8]) -> SuperShuckieEmulatorType {
        let game_boy = match self.settings.game_boy_settings.sgb {
            true => SuperShuckieEmulatorType::GameBoySGB2,
            false => SuperShuckieEmulatorType::GameBoy
        };

        // A Game Boy Color-only cartridge (CGB flag 0xC0, e.g. Pokémon Crystal) refuses to run on a
        // monochrome Game Boy: it either shows a "requires Game Boy Color" screen or, with a boot
        // ROM that skips the logo, sits on a blank LCD forever. No setting can make that useful, so
        // such a ROM always gets a Game Boy Color; the mode setting only decides dual-mode games.
        if cgb_flag(data) == CgbFlag::ColorOnly {
            return SuperShuckieEmulatorType::GameBoyColor
        }

        match self.settings.game_boy_settings.gbc_mode {
            GameBoyMode::AlwaysGBC => SuperShuckieEmulatorType::GameBoyColor,
            GameBoyMode::AlwaysGB => game_boy,
            GameBoyMode::GBInGBMode => match cgb_flag(data) {
                CgbFlag::Monochrome => game_boy,
                CgbFlag::ColorEnhanced | CgbFlag::ColorOnly => SuperShuckieEmulatorType::GameBoyColor
            }
        }
    }
}

/// What the cartridge header's CGB flag (byte 0x143) says the game supports.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum CgbFlag {
    /// No Game Boy Color support declared (or the ROM is too short to have a header).
    Monochrome,
    /// Runs on both, with extra features on a Game Boy Color (bit 7 set, bit 6 clear).
    ColorEnhanced,
    /// Game Boy Color only (bits 7 and 6 set); does not run on a monochrome Game Boy.
    ColorOnly
}

fn cgb_flag(rom: &[u8]) -> CgbFlag {
    match rom.get(0x143).copied().unwrap_or(0) {
        flag if flag & 0xC0 == 0xC0 => CgbFlag::ColorOnly,
        flag if flag & 0x80 != 0 => CgbFlag::ColorEnhanced,
        _ => CgbFlag::Monochrome
    }
}

#[cfg(test)]
mod cgb_flag_tests {
    use super::{cgb_flag, CgbFlag};

    fn rom_with_flag(flag: u8) -> Vec<u8> {
        let mut rom = vec![0u8; 0x150];
        rom[0x143] = flag;
        rom
    }

    #[test]
    fn header_flags() {
        assert_eq!(cgb_flag(&rom_with_flag(0x00)), CgbFlag::Monochrome);
        assert_eq!(cgb_flag(&rom_with_flag(0x80)), CgbFlag::ColorEnhanced);
        assert_eq!(cgb_flag(&rom_with_flag(0xC0)), CgbFlag::ColorOnly);
        // Some ROMs set stray low bits alongside the mode bits.
        assert_eq!(cgb_flag(&rom_with_flag(0xC1)), CgbFlag::ColorOnly);
        assert_eq!(cgb_flag(&rom_with_flag(0x81)), CgbFlag::ColorEnhanced);
        // No header at all is treated as a plain Game Boy game.
        assert_eq!(cgb_flag(&[0u8; 16]), CgbFlag::Monochrome);
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(C)]
pub enum SuperShuckieReplayState {
    NoReplay,
    Recording,
    Playback
}

/// `name.replay` -> `name`.
fn replay_name_without_extension(file_name: &str) -> String {
    file_name.strip_suffix(&format!(".{REPLAY_EXTENSION}")).unwrap_or(file_name).to_owned()
}

fn list_files_in_dir_with_extension(dir: &Path, extension: &str) -> Vec<UTF8CString> {
    let Ok(n) = std::fs::read_dir(dir) else {
        return Vec::new()
    };

    let mut options = Vec::new();
    for item in n {
        let Ok(item) = item else { continue };
        let path = item.path();
        if path.extension() != Some(extension.as_ref()) {
            continue
        }
        if !path.is_file() {
            continue
        }
        let Some(stem) = path.file_stem() else {
            continue
        };
        let Some(stem_utf8) = stem.to_str() else {
            continue
        };
        options.push(stem_utf8.into());
    }

    // Ensure the number at the end is compared numerically (if the rest is the same)
    options.sort_by(|a: &UTF8CString, b: &UTF8CString| {
        let a_str = a.as_str();
        let b_str = b.as_str();

        let a_split: Vec<&str> = a_str.rsplitn(2, '-').collect();
        let b_split: Vec<&str> = b_str.rsplitn(2, '-').collect();

        if a_split.len() != 2 || b_split.len() != 2 {
            return a_str.cmp(b_str);
        }

        // 1 is the prefix, 0 is the suffix (because of rsplitn)
        let prefix_cmp = a_split[1].cmp(&b_split[1]);
        if prefix_cmp != Ordering::Equal {
            return prefix_cmp;
        }

        let Ok(a_int) = a_split[0].parse::<i64>() else {
            return a_str.cmp(b_str);
        };
        let Ok(b_int) = b_split[0].parse::<i64>() else {
            return a_str.cmp(b_str);
        };
        a_int.cmp(&b_int)
    });

    options
}

#[derive(Copy, Clone, Debug)]
pub struct SuperShuckieReplayTimes {
    pub total_frames: u32,
    pub total_milliseconds: u32
}


/// Info of the replay file.
pub struct ReplayFileInfo {
    /// Name of the replay file being made
    pub final_replay_name: UTF8CString,

    /// Path of the replay file being made
    pub final_replay_path: PathBuf,

    /// Path to the temp file being recorded
    pub temp_replay_path: PathBuf
}

#[derive(Clone, Debug, Default)]
struct LastReadReplayCropData {
    start: Option<(UnsignedInteger, TimestampMillis)>,
    end: Option<(UnsignedInteger, TimestampMillis)>,
    timer_offset: Option<TimestampMillis>,
}

/// Static geometry of a screen, without the pixel buffer (see
/// [`SuperShuckieFrontendCallbacks::change_video_mode`]).
#[derive(Copy, Clone, Debug)]
pub struct ScreenInfo {
    pub width: usize,
    pub height: usize,
    pub encoding: ScreenDataEncoding
}

pub trait SuperShuckieFrontendCallbacks {
    /// Called with the freshly rendered screens' pixels. The core's screens mutex is held for the
    /// duration of this call (and `&mut SuperShuckieFrontend` is *not* available), so this must
    /// not call back into the frontend.
    fn refresh_screens(&mut self, screens: &[ScreenData]);

    /// Called when the video mode (screen count/geometry/encoding or the display scale) changes.
    /// Unlike [`Self::refresh_screens`], the core's screens mutex is not held here, so this may
    /// call back into the frontend — but only through its read-only getters; nothing that would
    /// try to lock the screens or otherwise reenter the core.
    fn change_video_mode(&mut self, screens: &[ScreenInfo], screen_scaling: NonZeroU8);

    /// Like [`Self::refresh_screens`], for another player's game in a Play Together session
    /// (that game's screens mutex is held; do not call back in). Default: ignored.
    fn peer_refresh_screens(&mut self, peer: play_together::PeerId, screens: &[ScreenData]) {
        let _ = (peer, screens);
    }

    /// Like [`Self::change_video_mode`], for another player's game: called when their game
    /// starts here and whenever its display scale changes (no lock is held; read-only getters
    /// may be called). A player who leaves is noticed through
    /// [`SuperShuckieFrontend::play_together_generation`], not a callback. Default: ignored.
    fn peer_change_video_mode(&mut self, peer: play_together::PeerId, screens: &[ScreenInfo], screen_scaling: NonZeroU8) {
        let _ = (peer, screens, screen_scaling);
    }
}

fn _ensure_callbacks_are_object_safe(_: Box<dyn SuperShuckieFrontendCallbacks>) {}

/// Longest tail of ffmpeg's stderr kept for use in error messages.
const FFMPEG_STDERR_MAX_TAIL: usize = 4096;

/// A [`VideoFrameSink`] that pipes raw frames to a spawned `ffmpeg` process.
///
/// The ffmpeg process is spawned lazily in [`VideoFrameSink::begin`] (not in [`Self::new`]), so
/// "ffmpeg missing" errors surface through the export result rather than at construction time.
///
/// stdout is discarded (`Stdio::null()`) and stderr is drained continuously by a background
/// thread (H8): a chatty ffmpeg (e.g. a `Custom` preset that adds `-loglevel info` or
/// `-progress -`) fills its stdout/stderr pipe fast, and with nothing reading them ffmpeg blocks
/// writing to it, in turn stopping it from reading stdin -- which would otherwise block
/// [`Self::push_frame`]'s `write_all` on the core thread forever. The background thread keeps only
/// the last [`FFMPEG_STDERR_MAX_TAIL`] bytes and hands them over a channel once the pipe closes
/// (ffmpeg exits) -- never while ffmpeg is still alive.
pub struct FfmpegVideoSink {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    /// The stderr-draining thread's tail of output, delivered once ffmpeg exits and its stderr
    /// pipe closes. Taken (and so only usable once) by [`Self::read_stderr_tail`].
    stderr_tail: Option<Receiver<Vec<u8>>>,
    output_path: PathBuf,
    ffmpeg_path: String,
    preset: ExportPreset,
    scale: NonZeroU8,
    crf: u8,
}

impl FfmpegVideoSink {
    /// Create a new sink. This only stores the configuration; ffmpeg is spawned in
    /// [`VideoFrameSink::begin`].
    pub fn new(ffmpeg_path: String, output_path: PathBuf, preset: ExportPreset, scale: NonZeroU8, crf: u8) -> Self {
        Self {
            child: None,
            stdin: None,
            stderr_tail: None,
            output_path,
            ffmpeg_path,
            preset,
            scale,
            crf,
        }
    }

    /// Kill the child (if still running) and wait for it, so its stderr pipe closes and the
    /// draining thread's tail becomes available. Never blocks on a live ffmpeg (H8): the child is
    /// always killed first.
    fn kill_and_wait(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// The stderr-draining thread's tail of output, for use in error messages. The child must
    /// already be dead (killed+waited, or waited on the normal exit path) before this is called;
    /// otherwise the thread may not have reached EOF yet and this simply times out. Best-effort;
    /// never fails, and only returns anything the first time it is called per export (the
    /// receiver is consumed).
    fn read_stderr_tail(&mut self) -> String {
        let Some(rx) = self.stderr_tail.take() else {
            return String::new();
        };
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(bytes) => tail_utf8(&bytes, FFMPEG_STDERR_MAX_TAIL),
            Err(_) => String::new()
        }
    }
}

impl VideoFrameSink for FfmpegVideoSink {
    fn begin(&mut self, width: u32, height: u32, fps_num: u32, fps_den: u32) -> Result<(), VideoExportError> {
        let out = self.output_path.clone();
        let scale = self.scale.get();
        let fps = format!("{fps_num}/{fps_den}");
        let video_size = format!("{width}x{height}");

        let mut command = Command::new(&self.ffmpeg_path);

        // Common input args: a raw bgra stream on stdin at the given geometry/fps.
        command.args([
            "-y",
            "-hide_banner",
            "-loglevel", "error",
            "-f", "rawvideo",
            "-pixel_format", "bgra",
            "-video_size", &video_size,
            "-framerate", &fps,
            "-i", "-",
        ]);

        // Per-preset output args. The caller chooses the output file extension.
        match &self.preset {
            ExportPreset::Mp4H264 => {
                // scale=iw*1:ih*1 is harmless, so the scale filter is applied unconditionally.
                command.args([
                    "-vf", &format!("scale=iw*{scale}:ih*{scale}:flags=neighbor,format=yuv420p"),
                    "-r", &fps,
                    "-c:v", "libx264",
                    "-preset", "slow",
                    "-crf", &self.crf.to_string(),
                    "-movflags", "+faststart",
                ]);
                command.arg(&out);
            }
            ExportPreset::LosslessFfv1Mkv => {
                command.args([
                    "-vf", &format!("scale=iw*{scale}:ih*{scale}:flags=neighbor"),
                    "-r", &fps,
                    "-c:v", "ffv1",
                    "-level", "3",
                ]);
                command.arg(&out);
            }
            ExportPreset::Custom(extra) => {
                // Simple whitespace split is sufficient for v1.
                for arg in extra.split_whitespace() {
                    command.arg(arg);
                }
                command.arg(&out);
            }
        }

        command.stdin(Stdio::piped());
        // H8: never piped-and-unread; an ffmpeg preset that writes to stdout (progress reports,
        // etc.) would otherwise fill the pipe and stall the whole export.
        command.stdout(Stdio::null());
        command.stderr(Stdio::piped());

        let mut child = command.spawn().map_err(|e| VideoExportError::Sink {
            explanation: Cow::Owned(format!(
                "Failed to start ffmpeg at '{}': {e}. Check the Export ffmpeg path setting.",
                self.ffmpeg_path
            ))
        })?;

        self.stdin = child.stdin.take();

        // Drain stderr continuously on a background thread (H8) so ffmpeg is never blocked
        // writing to a full pipe nobody is reading; keep only the tail for error messages.
        if let Some(mut stderr) = child.stderr.take() {
            let (tx, rx) = std::sync::mpsc::channel();
            let spawned = std::thread::Builder::new()
                .name("ffmpeg-stderr".to_owned())
                .spawn(move || {
                    let mut tail: Vec<u8> = Vec::new();
                    let mut chunk = [0u8; 4096];
                    loop {
                        match std::io::Read::read(&mut stderr, &mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                tail.extend_from_slice(&chunk[..n]);
                                if tail.len() > FFMPEG_STDERR_MAX_TAIL {
                                    let excess = tail.len() - FFMPEG_STDERR_MAX_TAIL;
                                    tail.drain(..excess);
                                }
                            }
                        }
                    }
                    let _ = tx.send(tail);
                });
            if spawned.is_ok() {
                self.stderr_tail = Some(rx);
            }
        }

        self.child = Some(child);

        Ok(())
    }

    fn push_frame(&mut self, argb: &[u32]) -> Result<(), VideoExportError> {
        // SAFETY: u32 slice reinterpreted as bytes; on a little-endian target the in-memory byte
        // order of 0xAARRGGBB is B, G, R, A, i.e. ffmpeg's `bgra` pixel format. (On a big-endian
        // target the bytes would be `argb` instead; all currently supported targets are LE.)
        let bytes: &[u8] = unsafe {
            core::slice::from_raw_parts(argb.as_ptr() as *const u8, argb.len() * 4)
        };

        let Some(stdin) = self.stdin.as_mut() else {
            return Err(VideoExportError::Sink {
                explanation: Cow::Borrowed("ffmpeg stdin is not available (begin was not called?)")
            });
        };

        if let Err(e) = stdin.write_all(bytes) {
            // ffmpeg likely died (BrokenPipe or otherwise). Make sure it is actually dead (H8:
            // never block reading stderr from a still-live process) before reading its tail.
            self.kill_and_wait();
            let tail = self.read_stderr_tail();
            let explanation = if tail.is_empty() {
                format!("Failed to write frame to ffmpeg: {e}")
            } else {
                format!("Failed to write frame to ffmpeg: {e}\n\nffmpeg said:\n{tail}")
            };
            return Err(VideoExportError::Sink { explanation: Cow::Owned(explanation) });
        }

        Ok(())
    }

    fn finish(&mut self) -> Result<(), VideoExportError> {
        // Close stdin so ffmpeg flushes and exits.
        drop(self.stdin.take());

        let Some(child) = self.child.as_mut() else {
            return Ok(());
        };

        let status = child.wait().map_err(|e| VideoExportError::Sink {
            explanation: Cow::Owned(format!("Failed to wait for ffmpeg to finish: {e}"))
        })?;

        if !status.success() {
            let tail = self.read_stderr_tail();
            let code = status.code().map(|c| c.to_string()).unwrap_or_else(|| "unknown".to_owned());
            let explanation = if tail.is_empty() {
                format!("ffmpeg exited with a non-zero status ({code}).")
            } else {
                format!("ffmpeg exited with a non-zero status ({code}):\n\n{tail}")
            };
            return Err(VideoExportError::Sink { explanation: Cow::Owned(explanation) });
        }

        Ok(())
    }

    fn abort(&mut self) {
        // Kill the child, wait for it, then close stdin. The stderr-draining thread ends on its
        // own once the pipe closes (EOF); its tail is not needed here so it is left unread.
        self.kill_and_wait();
        drop(self.stdin.take());

        // Delete the partial output file.
        let _ = std::fs::remove_file(&self.output_path);
    }
}
