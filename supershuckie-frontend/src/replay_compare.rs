//! Compared replays: other replays played beside the loaded one, kept on the same timer.
//!
//! The loaded (main) replay plays exactly as it always does, on the player's own core, under the
//! timeline's controls. Every compared replay gets a core of its own (a Play Together follower's
//! kind of core: lowered priority, no audio, its own window) whose replay is driven by a target
//! time instead of a clock (see `supershuckie_core::replay_sync`). Each tick the main replay's
//! position is turned into a timer reading and every compared replay is sent to the same
//! reading on its own timer, so pausing, stopping, seeking, frame stepping and speed changes on
//! the timeline carry over by themselves.
//!
//! The timer is the one the player starts and stops (the replay's start and end markers, with
//! its offset); a replay without a start marker uses its replay time instead. When the main
//! replay reaches its end, compared replays that are still going play on to theirs at the main
//! replay's speed.

use crate::play_together::emulator_type_for_console;
use crate::settings::PlayTogetherSettings;
use crate::util::UTF8CString;
use crate::{open_replay_player, ScreenInfo, SuperShuckieEmulatorType, SuperShuckieFrontend, REPLAY_EXTENSION};
use serde::Serialize;
use std::collections::BTreeMap;
use std::num::NonZeroU8;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use supershuckie_core::replay_sync::ReplaySyncTarget;
use supershuckie_core::{CoreThreadRole, ElapsedTimeStats, ReplayPlayerAttachError, ThreadedSuperShuckieCore};
use supershuckie_replay_recorder::replay_file::{ReplayConsoleType, ReplayFileMetadata};
use supershuckie_replay_recorder::{blake3_hash, SignedInteger, TimestampMillis, UnsignedInteger};

/// Identifies a compared replay (never 0).
pub type CompareId = u16;

/// The most replays compared with the main one at once.
pub const MAX_COMPARED_REPLAYS: usize = 8;

/// A compared replay this far (replay milliseconds) behind where it should be is reported as
/// catching up rather than in sync.
const CATCHING_UP_MILLIS: i64 = 250;

/// The start/stop timer of a replay (its start and end markers and offset; see
/// `SuperShuckieFrontend::mark_replay_start`), or the replay time when it has no start marker.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ReplayTimer {
    /// Replay time of the start marker.
    pub start_millis: Option<i64>,
    /// Replay time of the end marker.
    pub end_millis: Option<i64>,
    /// What the timer read at the start marker.
    pub offset_millis: i64
}

impl ReplayTimer {
    pub fn new(start: Option<(UnsignedInteger, TimestampMillis)>, end: Option<(UnsignedInteger, TimestampMillis)>, offset: Option<TimestampMillis>) -> Self {
        Self {
            start_millis: start.map(|(_, ms)| ms.0 as i64),
            end_millis: end.map(|(_, ms)| ms.0 as i64),
            offset_millis: offset.map(|ms| ms.0 as i64).unwrap_or(0)
        }
    }

    pub fn from_metadata(metadata: &ReplayFileMetadata) -> Self {
        Self::new(metadata.crop_start, metadata.crop_end, metadata.timer_offset)
    }

    /// Whether the replay has a timer of its own (a start marker); without one the replay time
    /// stands in.
    #[inline]
    pub fn has_timer(&self) -> bool {
        self.start_millis.is_some()
    }

    /// The replay time at which the timer reads 0 (the start marker less the offset; 0 without a
    /// timer). Replays are lined up by `replay time - base`.
    #[inline]
    pub fn base(&self) -> i64 {
        match self.start_millis {
            Some(start) => start - self.offset_millis,
            None => 0
        }
    }

    /// The line-up clock at replay time `millis` (what the timer would read if it never stopped).
    #[inline]
    pub fn clock_at(&self, millis: i64) -> i64 {
        millis - self.base()
    }

    /// The replay time at line-up clock `clock`.
    #[inline]
    pub fn millis_at(&self, clock: i64) -> i64 {
        clock + self.base()
    }

    /// What the timer reads at replay time `millis`: stopped at the end marker, and negative
    /// before the start marker (the replay time without a timer).
    pub fn reading_at(&self, millis: i64) -> i64 {
        let millis = match self.end_millis {
            Some(end) if self.start_millis.is_some() => millis.min(end),
            _ => millis
        };
        self.clock_at(millis)
    }

    /// The timer's final reading, when it was both started and stopped.
    pub fn final_reading(&self) -> Option<i64> {
        self.start_millis?;
        self.end_millis.map(|end| self.clock_at(end))
    }
}

/// Why a replay could not be added for comparison.
#[derive(Clone, Debug)]
pub enum CompareAddError {
    /// The game the replay was recorded on is not on this machine (or not where it was): ask
    /// for it and try again with its path.
    NeedsRom { rom_name: String, console: String },
    /// The replay does not quite match the game or emulator here; adding it anyway may desync.
    Mismatch(String),
    /// It cannot be compared at all.
    Failed(String)
}

impl CompareAddError {
    pub fn message(&self) -> String {
        match self {
            Self::NeedsRom { rom_name, console } => format!("This replay was recorded on {rom_name} ({console}), which was not found on this computer. Choose the ROM file to play it."),
            Self::Mismatch(m) | Self::Failed(m) => m.clone()
        }
    }
}

struct ComparedReplay {
    id: CompareId,
    name: String,
    path: PathBuf,
    rom_name: String,
    emulator_type: SuperShuckieEmulatorType,
    core: ThreadedSuperShuckieCore,
    target: Arc<ReplaySyncTarget>,
    timer: ReplayTimer,
    total_millis: i64,
    /// The last target sent, to wake the core only when it moves.
    last_target: Option<i64>,
    last_stats: ElapsedTimeStats,
    fps_window: Option<(Instant, u64)>,
    fps: f64,
    video_scale: NonZeroU8,
    window_hidden: bool,
    dead: bool,
    status: CompareStatus
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum CompareStatus {
    /// The main replay's timer has not reached the start of this replay yet.
    NotStarted,
    InSync,
    CatchingUp,
    Ended,
    Error
}

impl CompareStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::NotStarted => "not_started",
            Self::InSync => "in_sync",
            Self::CatchingUp => "catching_up",
            Self::Ended => "ended",
            Self::Error => "error"
        }
    }
}

/// The compared replays and the line-up clock they follow.
#[derive(Default)]
pub struct ReplayCompare {
    replays: Vec<ComparedReplay>,
    next_id: CompareId,
    generation: u64,
    /// Once the main replay has ended: the line-up clock playing on for the compared replays
    /// still going, and when it last moved.
    overrun: Option<(f64, Instant)>
}

impl ReplayCompare {
    fn bump(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }
}

/// A compared replay, for the UI.
#[derive(Serialize, Clone, Debug)]
pub struct ComparedReplayView {
    pub id: CompareId,
    pub name: String,
    pub path: String,
    pub rom_name: String,
    pub console: String,
    /// `not_started`, `in_sync`, `catching_up`, `ended` or `error`.
    pub status: &'static str,
    /// Whether the replay has a start/stop timer of its own (else its replay time is used).
    pub has_timer: bool,
    /// What its timer reads now (its replay time without a timer), in milliseconds.
    pub timer_ms: i64,
    /// Its timer's final reading, when it was started and stopped.
    pub final_ms: Option<i64>,
    pub replay_ms: u64,
    pub total_ms: u64,
    pub fps: f64,
    pub counters: BTreeMap<String, SignedInteger>,
    pub video_scale: u8,
    pub window_hidden: bool
}

/// Everything the UI shows about compared replays.
#[derive(Serialize, Clone, Debug)]
pub struct ReplayCompareStateView {
    /// Whether the main replay has a start/stop timer (else its replay time lines things up).
    pub main_has_timer: bool,
    /// The main timer's final reading, when it was started and stopped.
    pub main_final_ms: Option<i64>,
    /// What the main timer reads now.
    pub main_timer_ms: i64,
    /// Whether the main replay has ended and the compared ones are playing on to theirs.
    pub overrun: bool,
    pub replays: Vec<ComparedReplayView>
}

impl SuperShuckieFrontend {
    /// Changes whenever a replay is added or removed or one's status changes; poll
    /// [`Self::replay_compare_state`] when it does.
    #[inline]
    pub fn replay_compare_generation(&self) -> u64 {
        self.replay_compare.generation
    }

    /// How many replays are being compared with the main one.
    #[inline]
    pub fn replay_compare_count(&self) -> usize {
        self.replay_compare.replays.len()
    }

    /// The main replay's timer.
    fn main_replay_timer(&self) -> ReplayTimer {
        match self.last_read_replay_stats.as_ref() {
            Some(s) => ReplayTimer::new(s.start, s.end, s.timer_offset),
            None => ReplayTimer::default()
        }
    }

    pub fn replay_compare_state(&self) -> ReplayCompareStateView {
        let main = self.main_replay_timer();
        let main_millis = self.core.get_elapsed_time().replay_milliseconds as i64;
        ReplayCompareStateView {
            main_has_timer: main.has_timer(),
            main_final_ms: main.final_reading(),
            main_timer_ms: main.reading_at(main_millis),
            overrun: self.replay_compare.overrun.is_some(),
            replays: self.replay_compare.replays.iter().map(|r| {
                let millis = r.last_stats.milliseconds as i64;
                ComparedReplayView {
                    id: r.id,
                    name: r.name.clone(),
                    path: r.path.to_string_lossy().into_owned(),
                    rom_name: r.rom_name.clone(),
                    console: r.emulator_type.name().to_owned(),
                    status: r.status.as_str(),
                    has_timer: r.timer.has_timer(),
                    timer_ms: r.timer.reading_at(millis),
                    final_ms: r.timer.final_reading(),
                    replay_ms: millis.max(0) as u64,
                    total_ms: r.total_millis.max(0) as u64,
                    fps: r.fps,
                    counters: r.core.get_replay_counters(),
                    video_scale: r.video_scale.get(),
                    window_hidden: r.window_hidden
                }
            }).collect()
        }
    }

    /// The path of the current ROM's replay called `name`.
    pub fn replay_path_for_name(&self, name: &str) -> Result<PathBuf, UTF8CString> {
        crate::util::check_user_file_name(name)?;
        let rom = self.get_current_rom_name().ok_or_else(|| UTF8CString::from("No game is loaded."))?;
        Ok(self.get_replays_dir_for_rom(rom).join(format!("{name}.{REPLAY_EXTENSION}")))
    }

    /// Play the replay at `path` beside the main one, synced to its timer, in a window of its own
    /// (see [`crate::SuperShuckieFrontendCallbacks::compare_change_video_mode`]). Its ROM is
    /// looked for on this machine unless `rom_path` says where it is. With `override_errors` a
    /// replay that does not quite match its ROM or emulator is added anyway.
    pub fn replay_compare_add(&mut self, path: &Path, rom_path: Option<&Path>, override_errors: bool) -> Result<CompareId, CompareAddError> {
        self.refuse_if_exporting().map_err(|e| CompareAddError::Failed(e.to_string()))?;
        if self.current_replay.is_none() || !self.core.has_replay_attached() {
            return Err(CompareAddError::Failed("Play a replay first: compared replays follow the timeline of the replay that is playing.".into()))
        }
        if self.replay_compare.replays.len() >= MAX_COMPARED_REPLAYS {
            return Err(CompareAddError::Failed(format!("At most {MAX_COMPARED_REPLAYS} replays can be compared at once.")))
        }

        let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| String::from("Replay"));
        let mut player = open_replay_player(path, override_errors).map_err(|e| CompareAddError::Failed(format!("{e} ({name})")))?;
        let metadata = player.get_replay_metadata().clone();

        let emulator_type = emulator_type_for_console(metadata.console_type)
            .ok_or_else(|| CompareAddError::Failed(format!("{name} was recorded on an unknown console.")))?;
        if metadata.console_type == ReplayConsoleType::Nintendo3DS {
            return Err(CompareAddError::Failed("Nintendo 3DS replays cannot be compared: only one 3DS game can run at a time.".into()))
        }

        // The ROM: the one loaded, one this machine knows, or the one the user pointed at.
        let rom: Arc<Vec<u8>> = match rom_path {
            Some(rom_path) => {
                let data = std::fs::read(rom_path).map_err(|e| CompareAddError::Failed(format!("Cannot read {}: {e}", rom_path.display())))?;
                if blake3_hash(&data) != metadata.rom_checksum {
                    return Err(CompareAddError::Failed(format!(
                        "{} is not the ROM {name} was recorded on ({}).",
                        rom_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                        metadata.rom_name
                    )))
                }
                self.remember_rom(metadata.rom_checksum, rom_path);
                Arc::new(data)
            }
            None => match self.loaded_rom_data.as_ref().filter(|_| *self.core.rom_checksum() == metadata.rom_checksum) {
                Some(data) => data.clone(),
                None => match self.find_rom_by_hash(&metadata.rom_checksum) {
                    Some((_, data)) => Arc::new(data),
                    None => return Err(CompareAddError::NeedsRom { rom_name: metadata.rom_name.clone(), console: emulator_type.name().to_owned() })
                }
            }
        };

        let bios = self.compute_builtin_bios_override(metadata.bios_checksum).unwrap_or_else(|| self.default_bios_for(emulator_type));
        let core = self.make_new_core_with_bios(&rom, None, emulator_type, bios, false).map_err(|e| CompareAddError::Failed(e.to_string()))?;
        let mut core = ThreadedSuperShuckieCore::new_with_role(core, CoreThreadRole::Follower);
        core.pause();
        core.set_audio_enabled(false);
        core.set_audio_output(None);
        core.set_auto_resync_keyframes_in_replay(self.settings.replay.auto_resync_keyframes_in_replays);
        if matches!(emulator_type, SuperShuckieEmulatorType::GameBoy | SuperShuckieEmulatorType::GameBoyColor | SuperShuckieEmulatorType::GameBoySGB2) {
            core.set_gb_palette_override(self.settings.game_boy_settings.custom_colors.palette_override());
        }

        if self.settings.replay.auto_decompress_replays_upfront {
            player.decompress_all_blobs();
        }
        let total_millis = player.get_total_milliseconds().0 as i64;
        match core.attach_replay_player(player, override_errors) {
            Ok(()) => {}
            Err(ReplayPlayerAttachError::Incompatible { description }) => {
                return Err(CompareAddError::Failed(format!("{name} is incompatible:\n\n{description}")))
            }
            Err(ReplayPlayerAttachError::MismatchedMetadata { issues }) => {
                let mut err = format!("{name} has mismatched data which may prevent it from playing correctly:");
                for issue in issues {
                    err += "\n\n";
                    err += &issue.to_string();
                }
                return Err(CompareAddError::Mismatch(err))
            }
            Err(ReplayPlayerAttachError::Failed { description }) => {
                return Err(CompareAddError::Failed(format!("{name} could not be loaded:\n\n{description}")))
            }
        }

        let target = Arc::new(ReplaySyncTarget::new());
        core.set_replay_sync_target(Some(target.clone()));

        let compare = &mut self.replay_compare;
        let mut id = compare.next_id.wrapping_add(1).max(1);
        while compare.replays.iter().any(|r| r.id == id) {
            id = id.wrapping_add(1).max(1);
        }
        compare.next_id = id;

        let video_scale = self.video_scale_for(emulator_type).min(NonZeroU8::new(PlayTogetherSettings::MAX_PEER_VIDEO_SCALE).unwrap());
        let replay = ComparedReplay {
            id,
            name,
            path: path.to_owned(),
            rom_name: metadata.rom_name.clone(),
            emulator_type,
            core,
            target,
            timer: ReplayTimer::from_metadata(&metadata),
            total_millis,
            last_target: None,
            last_stats: ElapsedTimeStats::default(),
            fps_window: None,
            fps: 0.0,
            video_scale,
            window_hidden: false,
            dead: false,
            status: CompareStatus::NotStarted
        };
        let infos: Vec<ScreenInfo> = replay.core.read_screens(|screens| {
            screens.iter().map(|s| ScreenInfo { width: s.width, height: s.height, encoding: s.encoding }).collect()
        });
        self.replay_compare.replays.push(replay);
        self.replay_compare.bump();
        self.callbacks.compare_change_video_mode(id, &infos, video_scale);
        // Line it up right away rather than at the next tick.
        let mut errors = String::new();
        self.tick_replay_compare(&mut errors);
        if !errors.is_empty() {
            self.report_later(errors);
        }
        Ok(id)
    }

    /// Stop comparing replay `id` (its window goes when the UI notices the generation change).
    pub fn replay_compare_remove(&mut self, id: CompareId) {
        let before = self.replay_compare.replays.len();
        self.replay_compare.replays.retain(|r| r.id != id);
        if self.replay_compare.replays.len() != before {
            self.replay_compare.bump();
        }
        if self.replay_compare.replays.is_empty() {
            self.replay_compare.overrun = None;
        }
    }

    /// Stop comparing every replay.
    pub fn replay_compare_clear(&mut self) {
        if self.replay_compare.replays.is_empty() {
            return
        }
        self.replay_compare.replays.clear();
        self.replay_compare.overrun = None;
        self.replay_compare.bump();
    }

    /// Set the display scale of replay `id`'s window.
    pub fn replay_compare_set_video_scale(&mut self, id: CompareId, scale: NonZeroU8) {
        let scale = scale.min(NonZeroU8::new(PlayTogetherSettings::MAX_PEER_VIDEO_SCALE).unwrap());
        let Some(replay) = self.replay_compare.replays.iter_mut().find(|r| r.id == id) else {
            return
        };
        if replay.video_scale == scale {
            return
        }
        replay.video_scale = scale;
        let infos: Vec<ScreenInfo> = replay.core.read_screens(|screens| {
            screens.iter().map(|s| ScreenInfo { width: s.width, height: s.height, encoding: s.encoding }).collect()
        });
        self.callbacks.compare_change_video_mode(id, &infos, scale);
        self.replay_compare.bump();
    }

    /// Remember that replay `id`'s window is hidden (or shown). Its screen is not uploaded while
    /// hidden; showing it again uploads the current one at the next tick.
    pub fn replay_compare_set_window_hidden(&mut self, id: CompareId, hidden: bool) {
        let Some(replay) = self.replay_compare.replays.iter_mut().find(|r| r.id == id) else {
            return
        };
        if replay.window_hidden != hidden {
            replay.window_hidden = hidden;
            if !hidden {
                replay.last_stats.screen_generation = replay.last_stats.screen_generation.wrapping_sub(1);
            }
            self.replay_compare.bump();
        }
    }

    /// The display scale the main window uses for `emulator_type`.
    fn video_scale_for(&self, emulator_type: SuperShuckieEmulatorType) -> NonZeroU8 {
        match emulator_type {
            SuperShuckieEmulatorType::GameBoy | SuperShuckieEmulatorType::GameBoySGB2 | SuperShuckieEmulatorType::GameBoyColor => self.settings.game_boy_settings.video_scale,
            SuperShuckieEmulatorType::GameBoyAdvance => self.settings.game_boy_advance_settings.video_scale,
            SuperShuckieEmulatorType::NintendoDS => self.settings.nintendo_ds_settings.video_scale,
            SuperShuckieEmulatorType::Nintendo3DS => self.settings.nintendo_3ds_settings.video_scale
        }
    }

    /// Line every compared replay up with the main one, hand over their new screens and keep
    /// their status. Runs every tick; like Play Together's tick, it never waits on a core thread.
    pub(crate) fn tick_replay_compare(&mut self, errors: &mut String) {
        if self.replay_compare.replays.is_empty() {
            return
        }
        // The main replay went (closed, replaced by live play, the ROM unloaded): so do they.
        if self.current_replay.is_none() || !self.core.has_replay_attached() {
            self.replay_compare_clear();
            return
        }

        let main = self.main_replay_timer();
        let stats = self.core.get_elapsed_time();
        let main_clock = main.clock_at(stats.replay_milliseconds as i64);
        let now = Instant::now();

        // The main replay has ended: the ones still going play on at its speed.
        let main_ended = self.core.is_replay_playback_finished() && !self.core.is_replay_playback_stopped();
        let others_going = self.replay_compare.replays.iter().any(|r| !r.dead && !r.core.is_replay_playback_finished());
        let clock = if main_ended && others_going {
            let speed = stats.speed.into_multiplier_float().max(0.0);
            let (clock, at) = self.replay_compare.overrun.get_or_insert((main_clock as f64, now));
            *clock += now.duration_since(*at).as_secs_f64() * 1000.0 * speed;
            *at = now;
            *clock as i64
        }
        else if main_ended {
            // Everyone has finished; keep where the overrun got to.
            self.replay_compare.overrun.map(|(clock, _)| clock as i64).unwrap_or(main_clock)
        }
        else {
            self.replay_compare.overrun = None;
            main_clock
        };

        let mut changed = false;
        for replay in self.replay_compare.replays.iter_mut() {
            if replay.dead {
                continue
            }
            if !replay.core.is_alive() {
                replay.dead = true;
                replay.status = CompareStatus::Error;
                errors.push_str(&format!("- COMPARED REPLAY ({}): its emulator thread stopped.\n", replay.name));
                changed = true;
                continue
            }

            let target = replay.timer.millis_at(clock);
            if replay.last_target != Some(target) {
                replay.target.set(target);
                replay.core.wake();
                replay.last_target = Some(target);
            }

            let stats = replay.core.get_elapsed_time();
            if stats.screen_generation != replay.last_stats.screen_generation && !replay.window_hidden {
                replay.core.read_screens(|screens| self.callbacks.compare_refresh_screens(replay.id, screens));
            }
            replay.last_stats = stats;

            let frames = replay.core.get_emulated_frame_count();
            match replay.fps_window {
                Some((at, count)) => {
                    let elapsed = now.duration_since(at).as_secs_f64();
                    if elapsed >= 1.0 {
                        replay.fps = frames.saturating_sub(count) as f64 / elapsed;
                        replay.fps_window = Some((now, frames));
                    }
                }
                None => replay.fps_window = Some((now, frames))
            }

            for e in replay.core.take_playback_errors() {
                errors.push_str(&format!("- COMPARED REPLAY ({}): {e}\n", replay.name));
            }

            let millis = stats.milliseconds as i64;
            let status = if replay.core.is_replay_playback_finished() && target >= millis {
                CompareStatus::Ended
            }
            else if target < 0 {
                CompareStatus::NotStarted
            }
            else if target - millis > CATCHING_UP_MILLIS {
                CompareStatus::CatchingUp
            }
            else {
                CompareStatus::InSync
            };
            if status != replay.status {
                replay.status = status;
                changed = true;
            }
        }
        if changed {
            self.replay_compare.bump();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timers_line_up_on_their_start_markers() {
        // Main: timer started at 30 s of replay time. Other: at 45 s, with the timer already at
        // 2 s then (an offset).
        let main = ReplayTimer::new(Some((1800, TimestampMillis(30_000))), Some((9000, TimestampMillis(150_000))), None);
        let other = ReplayTimer::new(Some((2700, TimestampMillis(45_000))), None, Some(TimestampMillis(2_000)));
        assert!(main.has_timer() && other.has_timer());

        // When the main timer reads 0, the other's reads 0 too: 2 s before its start marker.
        let clock = main.clock_at(30_000);
        assert_eq!(clock, 0);
        assert_eq!(other.millis_at(clock), 43_000);
        assert_eq!(other.reading_at(43_000), 0);

        // A minute in, both read 1:00.
        let clock = main.clock_at(90_000);
        assert_eq!(main.reading_at(90_000), 60_000);
        assert_eq!(other.reading_at(other.millis_at(clock)), 60_000);

        // The main timer stops at its end marker; the line-up clock does not.
        assert_eq!(main.reading_at(200_000), 120_000);
        assert_eq!(main.clock_at(200_000), 170_000);
        assert_eq!(main.final_reading(), Some(120_000));
        assert_eq!(other.final_reading(), None);
    }

    #[test]
    fn replay_time_stands_in_without_a_timer() {
        let main = ReplayTimer::new(Some((600, TimestampMillis(10_000))), None, None);
        let untimed = ReplayTimer::default();
        assert!(!untimed.has_timer());
        assert_eq!(untimed.base(), 0);
        // The main timer at 5 s puts an untimed replay at 5 s of replay time.
        assert_eq!(untimed.millis_at(main.clock_at(15_000)), 5_000);
        // Before the main timer starts, an untimed replay waits before its beginning.
        assert!(untimed.millis_at(main.clock_at(4_000)) < 0);
        // An end marker without a start marker is no timer.
        let end_only = ReplayTimer::new(None, Some((60, TimestampMillis(1_000))), None);
        assert_eq!(end_only.reading_at(5_000), 5_000);
        assert_eq!(end_only.final_reading(), None);
    }
}
