//! TODO
#![no_std]
#![warn(missing_docs)]

extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

use crate::emulator::{EmulatorCore, Input, PartialReplayRecordMetadata, RunTime};
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::{Display, Formatter};
use core::num::NonZeroU64;
use alloc::collections::BTreeMap;
use supershuckie_replay_recorder::keyframe_masks::transient_ranges;
use supershuckie_replay_recorder::replay_file::playback::{ReplayFilePlayer, ReplaySeekError};
use supershuckie_replay_recorder::replay_file::record::{build_resumed_recorder, NonBlockingReplayFileRecorder, ReplayFileRecorder, ReplayFileRecorderFns, ReplayFileSink, ReplayFileWriteError, ReplayResumeError, ResumeCropPolicy};
use supershuckie_replay_recorder::replay_file::{blake3_hash_to_ascii, ReplayConsoleType, ReplayFileMetadata, ReplayHeaderBlake3Hash, ReplayPatchFormat};
use supershuckie_replay_recorder::{BookmarkTable, ByteVec, Packet, SignedInteger, TimestampMillis, UnsignedInteger, KEYFRAME_BOOKMARK_LEAD_FRAMES};

pub mod emulator;

pub mod export;
pub use export::{ExportRange, ScreenLayout, VideoExportError, VideoFrameSink};

pub use supershuckie_replay_recorder::Speed;

#[cfg(feature = "std")]
mod thread;

#[cfg(feature = "std")]
pub mod memory_monitor;

#[cfg(feature = "std")]
pub use thread::*;

#[cfg(feature = "std")]
pub mod audio;

#[cfg(feature = "std")]
pub use audio::AudioOutput;

/// Wrapper for [`EmulatorCore`] that provides useful desktop emulator functionality.
pub struct SuperShuckieCore {
    core: Box<dyn EmulatorCore>,
    replay_file_recorder: Option<Box<dyn ReplayFileRecorderFns>>,
    replay_counters: Option<BTreeMap<String, SignedInteger>>,

    timestamp_provider: Box<dyn MonotonicTimestampProvider>,

    replay_player: Option<ReplayFilePlayer>,

    /// The current user-defined input.
    base_input: Input,

    /// The input to apply next frame.
    next_input: Option<Input>,

    /// Rapid fire input, if any.
    ///
    /// This input is applied every interval for a set number of frames.
    rapid_fire_input: Option<SuperShuckieRapidFire>,

    /// Queued writes, if any
    writes: Vec<QueuedWrite>,

    /// Toggled input, if any.
    ///
    /// This input is always applied.
    toggled_input: Option<Input>,

    /// The "total" input that was actually applied.
    current_input: Input,

    replay_stalled: bool,

    /// Whether [`Self::handle_replay`] has already consumed the attached replay's packets for the
    /// next frame (up to and including its `NextFrame`). Cleared once a run actually emulates a
    /// frame. Without this, every poll of a paced core's `run` that turned out to be a pacing
    /// miss (or a sub-frame step of the Game Boy core) would advance the replay cursor by a
    /// frame, racing the recorded inputs ahead of the emulator and ending playback early.
    replay_frame_pending: bool,

    /// The live-side counterpart of [`Self::replay_frame_pending`]: whether
    /// [`Self::update_input`] has already applied (and recorded) the input for the next frame.
    /// Cleared once a run actually emulates a frame, or when the input to apply changes. Without
    /// this, every pacing-miss poll of a paced core's `run` re-encoded the input and sent the
    /// recorder a `ChangeInput` packet -- hundreds per frame while the thread loop spins through
    /// the last millisecond before a frame deadline.
    input_latched: bool,

    /// Whether the attached replay is stopped: still attached (so it can be seeked in and
    /// resumed) but no longer driving the emulator, which runs live under the user's input from
    /// wherever playback left off. See [`Self::stop_replay_playback`]. Meaningless without a
    /// player attached.
    replay_playback_stopped: bool,

    /// Where a stopped replay resumes from, as `(frame, replay time)`: the position playback was
    /// stopped at, or the last one seeked to since. Only meaningful while stopped.
    replay_resume_point: (UnsignedInteger, TimestampMillis),

    input_scratch_buffer: Vec<u8>,
    starting_milliseconds: TimestampMillis,
    total_milliseconds: TimestampMillis,
    paused_timer_at: Option<TimestampMillis>,
    game_speed: Speed,
    replay_playback_speed: Speed,

    frames_since_last_keyframe: u64,
    frames_per_keyframe: u64,
    total_frames: u64,

    /// A keyframe bookmark asked for a full keyframe while a frame was partly emulated; it is
    /// written when that frame completes (see [`Self::bookmark_anchor`]).
    full_keyframe_pending: bool,

    ignore_speed_changes_in_replays: bool,
    auto_resync_keyframes_in_replays: bool,

    /// What the last `run`/`run_unlocked` reported.
    last_run: RunTime,

    /// Incremented every time the core actually ran (see [`Self::run_serial`]).
    run_serial: u64,

    /// Incremented whenever memory is replaced wholesale (see [`Self::state_epoch`]).
    state_epoch: u64,

    /// `WriteMemory` packets in played-back replays that could not be applied.
    replay_write_failures: u64,

    /// Present (draw) one frame in this many while running paced; see [`Self::present_every`].
    present_every: u64,

    /// Save-state buffers handed back by the recorder, reused for the next keyframe.
    state_buffers: Vec<Vec<u8>>,

    /// Where audible frames' samples go, if anyone is listening.
    #[cfg(feature = "std")]
    audio_output: Option<alloc::sync::Arc<AudioOutput>>,

    /// Samples taken from the core after each run (kept to reuse the allocation).
    audio_scratch: Vec<i16>,

    /// Whether the core is rendering audio at all (see [`EmulatorCore::set_audio_enabled`]).
    audio_enabled: bool,

    /// Discard audio while the game runs at any speed other than 1x.
    audio_mute_when_sped_up: bool
}

/// Where a bookmark requested at the current moment goes (see
/// [`SuperShuckieCore::bookmark_anchor`]).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct BookmarkAnchor {
    /// The bookmark's in frame.
    pub in_frame: UnsignedInteger,

    /// Replay time at the in frame (for a keyframe bookmark, the time of its keyframe, a few frames
    /// earlier).
    pub in_millis: TimestampMillis,

    /// Whether the replay has a keyframe the bookmark can be reached from without re-emulation.
    pub keyframe: bool
}

/// Why no bookmark could be placed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum BookmarkAnchorError {
    /// No replay is being recorded or played back.
    NoReplay,

    /// The core thread did not answer in time (it is busy, e.g. exporting a video).
    Busy
}

impl Display for BookmarkAnchorError {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoReplay => f.write_str("Bookmarks need a replay that is recording or playing back."),
            Self::Busy => f.write_str("The emulator is busy; try again in a moment.")
        }
    }
}

#[derive(Clone, Debug)]
struct QueuedWrite {
    address: u32,
    data: ByteVec
}

/// Defines parameters for rapid fire.
#[derive(Copy, Clone, PartialEq, Debug)]
pub struct SuperShuckieRapidFire {
    /// Input state to use.
    pub input: Input,

    /// Number of frames the button(s) are held down between intervals.
    ///
    /// Note that when rapid fire is enabled, the button will be held down immediately for this many
    /// frames.
    pub hold_length: NonZeroU64,

    /// Number of frames the button(s) are released between intervals.
    pub interval: NonZeroU64,

    /// The current stage of the duty cycle.
    current_frame: u64,

    /// The sum of hold_length + interval.
    total_frames: u64,
}

impl Default for SuperShuckieRapidFire {
    fn default() -> Self {
        Self {
            input: Input::default(),
            hold_length: NonZeroU64::new(1).unwrap(),
            interval: NonZeroU64::new(1).unwrap(),
            current_frame: 0,
            total_frames: 0
        }
    }
}

impl SuperShuckieCore {
    /// Wrap `emulator_core`.
    pub fn new(emulator_core: Box<dyn EmulatorCore>, mut timestamp_provider: Box<dyn MonotonicTimestampProvider>) -> Self {
        Self {
            replay_file_recorder: None,
            base_input: Input::default(),
            next_input: None,
            rapid_fire_input: None,
            writes: Vec::new(),
            toggled_input: None,
            current_input: Default::default(),
            input_scratch_buffer: Vec::new(),
            total_milliseconds: 0.into(),
            starting_milliseconds: timestamp_provider.get_timestamp_milliseconds().into(),
            game_speed: Default::default(),
            replay_playback_speed: Default::default(),
            frames_since_last_keyframe: 0,
            frames_per_keyframe: 0,
            total_frames: 0,
            full_keyframe_pending: false,
            replay_player: None,
            replay_stalled: false,
            replay_frame_pending: false,
            input_latched: false,
            replay_playback_stopped: false,
            replay_resume_point: (0, 0.into()),
            paused_timer_at: None,
            replay_counters: None,
            core: emulator_core,
            timestamp_provider,
            ignore_speed_changes_in_replays: false,
            auto_resync_keyframes_in_replays: false,
            last_run: RunTime::NONE,
            run_serial: 0,
            state_epoch: 0,
            replay_write_failures: 0,
            present_every: 1,
            state_buffers: Vec::new(),
            #[cfg(feature = "std")]
            audio_output: None,
            audio_scratch: Vec::new(),
            audio_enabled: false,
            audio_mute_when_sped_up: true
        }
    }

    /// Most keyframe state buffers kept around for reuse (one being filled, one held by the
    /// recorder as the diff base, one in flight).
    const STATE_BUFFER_POOL: usize = 3;

    /// Frames over which one frame is drawn while running paced at the current speed.
    ///
    /// From 2x up nobody can see every frame (the display shows 60 a second), so only one frame
    /// in `floor(speed)` is composited; the rest are emulated but not drawn, which is markedly
    /// cheaper. Unpaced runs (seeks, export) always draw what they need to.
    pub fn present_every(&self) -> u64 {
        self.present_every
    }

    /// Whether the screens hold a newly drawn frame from the last run.
    pub fn last_frame_presented(&self) -> bool {
        self.last_run.presented
    }

    /// What the last run reported.
    pub fn last_run_time(&self) -> RunTime {
        self.last_run
    }

    /// A counter that changes whenever the core ran (including runs made on behalf of commands
    /// such as loading a save state), so a consumer can tell a fresh [`Self::last_run_time`] from
    /// one it has already acted on.
    pub fn run_serial(&self) -> u64 {
        self.run_serial
    }

    /// A counter that changes whenever the emulated memory is replaced wholesale rather than by
    /// the game running: a save state load, a reset, a replay seek, attaching or detaching a
    /// replay. Memory watchers use it to tell such a jump from the game changing a value.
    #[inline]
    pub fn state_epoch(&self) -> u64 {
        self.state_epoch
    }

    #[inline]
    fn bump_state_epoch(&mut self) {
        self.state_epoch = self.state_epoch.wrapping_add(1);
    }

    /// Whether the core is in the middle of a frame (only the Game Boy core steps in sub-frame
    /// slices).
    #[inline]
    pub fn is_mid_frame(&self) -> bool {
        self.core.is_mid_frame()
    }

    /// The wall-clock milliseconds elapsed since [`Self::restart_timer`] (or the frozen value at
    /// the moment the timer was paused, while it still is): [`Self::pause_timer`] stores an
    /// absolute time, so subtracting `starting_milliseconds` gives back exactly the
    /// `total_milliseconds` that was current when it was called; [`Self::unpause_timer`] then
    /// re-bases `starting_milliseconds` so the next live reading is never smaller than that.
    /// Together this keeps every reading monotone non-decreasing across a pause, which is all the
    /// replay recorder needs (it errors on a backwards timestamp).
    fn current_timer_millis(&mut self) -> TimestampMillis {
        let now = match self.paused_timer_at {
            Some(p) => p.0,
            None => self.timestamp_provider.get_timestamp_milliseconds()
        };
        now.wrapping_sub(self.starting_milliseconds.0).into()
    }

    /// Whether an attached replay is driving the emulator (attached and not stopped).
    ///
    /// Everything the user cannot do while a replay plays (input, RAM writes, resets, save
    /// states) is gated on this rather than on a player being attached: a stopped replay stays
    /// attached, but the emulator is the user's again.
    #[inline]
    pub fn is_playing_back(&self) -> bool {
        self.replay_player.is_some() && !self.replay_playback_stopped
    }

    /// Whether a replay is attached, playing or stopped.
    #[inline]
    pub fn has_replay_attached(&self) -> bool {
        self.replay_player.is_some()
    }

    /// Whether an attached replay is stopped (see [`Self::stop_replay_playback`]).
    #[inline]
    pub fn is_replay_playback_stopped(&self) -> bool {
        self.replay_player.is_some() && self.replay_playback_stopped
    }

    /// The attached replay's position as `(frame, replay time)`: where playback is, or where it
    /// resumes from while stopped (the live frame counter keeps going then, see
    /// [`Self::total_frames`]). `(0, 0)` without a replay.
    #[inline]
    pub fn replay_position(&self) -> (UnsignedInteger, TimestampMillis) {
        if self.replay_player.is_none() {
            (0, 0.into())
        }
        else if self.replay_playback_stopped {
            self.replay_resume_point
        }
        else {
            (self.total_frames, self.total_milliseconds)
        }
    }

    /// Stop the attached replay from driving the emulator without detaching it: the game keeps
    /// running from the current frame, live and under the user's input, while the replay stays
    /// attached so it can still be seeked in (the seek puts the emulator at the new frame and
    /// hands it back) and resumed from where it was stopped or last seeked to (see
    /// [`Self::resume_replay_playback`]). The wall-clock timer continues from the replay's time.
    ///
    /// Returns whether anything changed (`false` without a replay, or if already stopped).
    pub fn stop_replay_playback(&mut self) -> bool {
        if !self.is_playing_back() {
            return false
        }
        self.stop_replay_playback_here();
        // Like a detach: nothing pressed during playback carries over into live play.
        self.reset_input();
        true
    }

    /// Hand the emulator back to the user at the current frame, which becomes the resume point.
    fn stop_replay_playback_here(&mut self) {
        self.replay_playback_stopped = true;
        self.replay_resume_point = (self.total_frames, self.total_milliseconds);
        // A stopped replay reads nothing, so it cannot be stalled; a stall from before (playback
        // had reached the end) must not keep pausing the emulator.
        self.replay_stalled = false;
        // Whatever the RAM tools queued while the replay owned memory belongs to that timeline.
        self.writes.clear();
        // The replay set the console's input; the user's applies from the next run.
        self.input_latched = false;
        self.resume_timer(self.total_milliseconds, self.total_frames);
    }

    /// Resume playing back a stopped replay from its resume point (see
    /// [`Self::stop_replay_playback`]): whatever was played live since is discarded and the
    /// emulator is put back exactly where playback stopped or was last seeked to.
    ///
    /// Does nothing without a replay or if it is not stopped. Returns an error (leaving the core
    /// stalled, as any failed seek does) if the replay cannot be read there.
    pub fn resume_replay_playback(&mut self) -> Result<(), String> {
        if !self.is_replay_playback_stopped() {
            return Ok(())
        }
        let (frame, _) = self.replay_resume_point;
        self.replay_playback_stopped = false;
        self.next_input = None;
        self.writes.clear();
        self.go_to_replay_frame(frame)
    }

    /// Put a stopped replay's emulator back at the resume point (see
    /// [`Self::stop_replay_playback`]) without resuming playback: whatever was played live since
    /// is discarded, and the user stays in control from that frame, which remains the resume
    /// point. Does nothing unless a replay is stopped. Errors as a seek there would.
    pub fn go_to_replay_resume_point(&mut self) -> Result<(), String> {
        if !self.is_replay_playback_stopped() {
            return Ok(())
        }
        let (frame, _) = self.replay_resume_point;
        // A seek while stopped hands the emulator back at the target, which becomes the resume
        // point: the same frame, so it stays put.
        self.go_to_replay_frame(frame)
    }

    /// The attached replay player, if any (for diagnostics; the core drives its cursor).
    #[inline]
    pub fn replay_player(&self) -> Option<&ReplayFilePlayer> {
        self.replay_player.as_ref()
    }

    /// How many `WriteMemory` packets of played-back replays could not be applied (for example a
    /// replay made by a newer version writing to memory this version does not map).
    #[inline]
    pub fn replay_write_failures(&self) -> u64 {
        self.replay_write_failures
    }

    /// Run the emulator core for the shortest amount of time.
    pub fn run(&mut self) {
        let skip = self.present_every > 1 && !self.core.is_mid_frame() && self.total_frames % self.present_every != 0;
        self.core.set_skip_drawing(skip);
        self.do_run_fn(EmulatorCore::run, true);
    }

    /// Run the emulator core for the shortest amount of time without any timekeeping.
    ///
    /// Nobody is listening to unpaced runs (exports, finishing a frame), so their audio is
    /// dropped.
    pub fn run_unlocked(&mut self) {
        self.core.set_skip_drawing(false);
        self.do_run_fn(EmulatorCore::run_unlocked, false);
    }

    /// Like [`Self::run_unlocked`], but the frame need not be drawn (used while catching up to a
    /// target frame nobody will look at).
    ///
    /// Emulation is unaffected: only the core's presentation is skipped, so a frame walked past
    /// with this and then one drawn with [`Self::run_unlocked`] give exactly the pictures that
    /// drawing every frame would have. Headless consumers (the frame server) use it to step to
    /// a target frame in slices they can abandon between.
    pub fn run_unlocked_hidden(&mut self) {
        self.core.set_skip_drawing(true);
        self.do_run_fn(EmulatorCore::run_unlocked, false);
    }

    /// Like [`Self::run_unlocked`], but the samples the frame produces reach the audio output
    /// (see [`Self::set_audio_output`]) as they would from a paced [`Self::run`].
    ///
    /// For a consumer that wants the sound of a stretch of a replay without playing it in real
    /// time: install an [`AudioOutput`] whose latency covers the stretch, run it with this and
    /// read the ring afterwards. Whether sped-up frames are heard follows
    /// [`Self::set_audio_mute_when_sped_up`] exactly as for paced runs.
    #[cfg(feature = "std")]
    pub fn run_unlocked_audible(&mut self) {
        self.core.set_skip_drawing(false);
        self.do_run_fn(EmulatorCore::run_unlocked, true);
    }

    /// Get the current replay counters.
    pub fn get_replay_counters(&self) -> Option<&BTreeMap<String, SignedInteger>> {
        self.replay_counters.as_ref()
    }

    /// Run the core with `run_fn`. `audible` says whether the samples it produces should reach
    /// the audio output; they are always taken from the core either way so its buffers do not
    /// depend on who is listening.
    fn do_run_fn(&mut self, run_fn: fn(&mut dyn EmulatorCore) -> RunTime, audible: bool) {
        self.last_run = RunTime::NONE;

        if !self.replay_stalled {
            self.before_run();
        }

        if !self.replay_stalled {
            let time = run_fn(Box::as_mut(&mut self.core));
            self.after_run(&time);
            self.drain_audio(audible);
        }
    }

    /// Move the samples the core produced to the audio output, or throw them away.
    fn drain_audio(&mut self, audible: bool) {
        self.audio_scratch.clear();
        if !self.audio_enabled {
            return
        }
        self.core.take_audio(&mut self.audio_scratch);

        #[cfg(feature = "std")]
        if let Some(output) = self.audio_output.as_ref() {
            if audible && (!self.audio_mute_when_sped_up || self.is_normal_speed()) {
                output.push(&self.audio_scratch);
            }
        }
        let _ = audible;
        self.audio_scratch.clear();
    }

    #[inline]
    fn is_normal_speed(&self) -> bool {
        self.game_speed == Speed::from_multiplier_float(1.0)
    }

    /// Route audible frames' samples to `output` (`None` to stop). The output also learns
    /// whether this console's sped-up audio needs pitch scaling on the consumer side.
    #[cfg(feature = "std")]
    pub fn set_audio_output(&mut self, output: Option<alloc::sync::Arc<AudioOutput>>) {
        if let Some(o) = output.as_ref() {
            let scales_pitch = !matches!(
                self.core.replay_console_type(),
                Some(ReplayConsoleType::GameBoy | ReplayConsoleType::GameBoyColor | ReplayConsoleType::SuperGameBoy2)
            );
            o.set_fast_forward_scales_pitch(scales_pitch);
            o.set_speed(self.game_speed.into_multiplier_float() as f32);
        }
        self.audio_output = output;
    }

    /// Turn audio rendering in the core on or off.
    pub fn set_audio_enabled(&mut self, enabled: bool) {
        if self.audio_enabled == enabled {
            return
        }
        self.audio_enabled = enabled;
        self.core.set_audio_enabled(enabled);
        self.clear_audio();
    }

    /// Whether the core is rendering audio.
    #[inline]
    pub fn audio_enabled(&self) -> bool {
        self.audio_enabled
    }

    /// Discard audio while the game runs at any speed other than 1x (default: on).
    pub fn set_audio_mute_when_sped_up(&mut self, mute: bool) {
        if self.audio_mute_when_sped_up == mute {
            return
        }
        self.audio_mute_when_sped_up = mute;
        self.clear_audio();
    }

    /// Drop queued audio so nothing stale plays after a discontinuity.
    fn clear_audio(&mut self) {
        #[cfg(feature = "std")]
        if let Some(o) = self.audio_output.as_ref() {
            o.clear();
        }
    }

    /// Run unlocked until the next frame.
    pub fn finish_current_frame(&mut self) {
        while self.core.is_mid_frame() && !self.replay_stalled {
            self.run_unlocked();
        }
    }

    /// Write `data` at `address` (recorded into the replay being recorded, if any): right away
    /// between frames, or once the current frame finishes.
    ///
    /// Dropped, returning `false`, while a replay is being played back (not while it is merely
    /// attached but stopped): the replay owns the memory then, and a write held back until
    /// playback ends would land at an arbitrary moment.
    pub fn enqueue_write(&mut self, address: u32, data: ByteVec) -> bool {
        if self.is_playing_back() {
            return false
        }
        self.writes.push(QueuedWrite { address, data });
        self.flush_writes();
        true
    }

    /// [`enqueue_write`](Self::enqueue_write) `data` at `address` only if the bytes there differ,
    /// so holding a value in place (a freeze) writes (and records) only on frames where something
    /// actually changed it. Returns whether a write was enqueued; unmapped memory is never written.
    pub fn write_if_changed(&mut self, address: u32, data: &[u8]) -> bool {
        if self.is_playing_back() || data.is_empty() {
            return false
        }
        let unchanged = match crate::emulator::memory_slice(self.core.as_ref(), address, data.len()) {
            Some(current) => current == data,
            None => {
                // Not in a listed region; fall back to the core's own address decoding.
                let mut current = alloc::vec![0u8; data.len()];
                match self.core.read_ram(address, &mut current) {
                    Ok(()) => current == data,
                    Err(_) => return false
                }
            }
        };
        !unchanged && self.enqueue_write(address, ByteVec::from(data))
    }

    /// Pause the current timer.
    pub fn pause_timer(&mut self) {
        self.paused_timer_at = Some((self.total_milliseconds.0 + self.starting_milliseconds.0).into());
    }

    /// Unpause the current timer if it is currently paused.
    pub fn unpause_timer(&mut self) {
        let Some(paused_time) = self.paused_timer_at.take() else {
            return
        };
        let unpaused_time = self.timestamp_provider.get_timestamp_milliseconds();

        self.starting_milliseconds = self.starting_milliseconds.0.wrapping_add(unpaused_time.wrapping_sub(paused_time.0)).into();
    }

    fn restart_timer(&mut self) {
        self.paused_timer_at = None;
        self.starting_milliseconds = self.timestamp_provider.get_timestamp_milliseconds().into();
        self.total_milliseconds = 0.into();
        self.total_frames = 0;
    }

    /// Get an immutable reference to the underlying core.
    pub fn get_core(&self) -> &dyn EmulatorCore {
        self.core.as_ref()
    }

    /// Set the speed multiplier of the game.
    pub fn set_speed(&mut self, speed: Speed) {
        let multiplier = speed.into_multiplier_float();
        self.game_speed = Speed::from_multiplier_float(multiplier);
        self.core.set_speed(multiplier);
        self.present_every = if multiplier >= 2.0 { (multiplier.floor() as u64).clamp(1, 16) } else { 1 };
        self.with_recorder(|r| r.set_speed(speed));

        #[cfg(feature = "std")]
        if let Some(o) = self.audio_output.as_ref() {
            o.set_speed(multiplier as f32);
        }
        // Sped-up stretches start silent right away rather than after the queued tail plays.
        if self.audio_mute_when_sped_up && !self.is_normal_speed() {
            self.clear_audio();
        }
    }

    /// Mark the start of the replay, returning the timestamp.
    /// 
    /// Set the timer offset to the given offset.
    pub fn mark_start(&mut self, timer_offset: TimestampMillis) -> Option<(UnsignedInteger, TimestampMillis)> {
        if self.replay_file_recorder.is_none() {
            return None
        }

        self.with_recorder(|r| r.mark_start(timer_offset));
        Some((self.total_frames, self.total_milliseconds))
    }

    /// Mark the end of the replay, returning the timestamp.
    pub fn mark_end(&mut self) -> Option<(UnsignedInteger, TimestampMillis)> {
        if self.replay_file_recorder.is_none() {
            return None
        }

        self.with_recorder(|r| r.mark_end());
        Some((self.total_frames, self.total_milliseconds))
    }

    /// Place a bookmark at the current moment of the replay being recorded or played back.
    ///
    /// A plain bookmark goes on the current frame (the last one completed, which is on screen). For
    /// a keyframe bookmark while recording, a full keyframe is written right away and the bookmark
    /// goes [`KEYFRAME_BOOKMARK_LEAD_FRAMES`] later, so that seeking to it loads exactly that
    /// keyframe; if a frame is partly emulated (the Game Boy core runs in slices), the keyframe is
    /// written when that frame completes instead, and the bookmark counts from it. Nothing is
    /// emulated here: finishing a frame while paused would record it at the wrong time.
    ///
    /// During playback no keyframe can be written; the bookmark goes the same distance after the
    /// replay's existing keyframe at or before that point. While the replay is stopped, "the
    /// current moment" is its resume point (see [`Self::replay_position`]).
    pub fn bookmark_anchor(&mut self, keyframe: bool) -> Result<BookmarkAnchor, BookmarkAnchorError> {
        if self.replay_file_recorder.is_some() {
            let ms = self.total_milliseconds;

            if !keyframe {
                return Ok(BookmarkAnchor { in_frame: self.total_frames, in_millis: ms, keyframe: false })
            }

            if self.core.is_mid_frame() {
                self.full_keyframe_pending = true;
                return Ok(BookmarkAnchor { in_frame: self.total_frames + 1 + KEYFRAME_BOOKMARK_LEAD_FRAMES, in_millis: ms, keyframe: true })
            }

            self.write_keyframe(true);
            return Ok(BookmarkAnchor { in_frame: self.total_frames + KEYFRAME_BOOKMARK_LEAD_FRAMES, in_millis: ms, keyframe: true })
        }

        // A stopped replay's "current moment" is where playback resumes from, not the live frame.
        let (position_frame, position_millis) = self.replay_position();
        let Some(player) = self.replay_player.as_ref() else {
            return Err(BookmarkAnchorError::NoReplay)
        };

        if !keyframe {
            return Ok(BookmarkAnchor { in_frame: position_frame, in_millis: position_millis, keyframe: false })
        }

        let latest = position_frame.saturating_sub(KEYFRAME_BOOKMARK_LEAD_FRAMES);
        let (&frame, metadata) = player.all_keyframes().range(..=latest).next_back().expect("replays always have a keyframe at frame 0");
        let in_millis = metadata.last().map(|m| m.elapsed_millis).unwrap_or_default();
        Ok(BookmarkAnchor { in_frame: frame + KEYFRAME_BOOKMARK_LEAD_FRAMES, in_millis, keyframe: true })
    }

    /// Estimate the replay time at `frame` of the replay being recorded or played back, for a
    /// bookmark placed at an explicit frame. Playback interpolates between the surrounding
    /// keyframes; recording counts back from now at the console's nominal frame rate. `None` without
    /// a replay.
    pub fn estimate_millis_at(&self, frame: UnsignedInteger) -> Option<TimestampMillis> {
        if let Some(player) = self.replay_player.as_ref() {
            let keyframes = player.all_keyframes();
            let before = keyframes.range(..=frame).next_back().and_then(|(&f, m)| Some((f, m.last()?.elapsed_millis.0)));
            let after = keyframes.range(frame..).next().and_then(|(&f, m)| Some((f, m.first()?.elapsed_millis.0)))
                .or(Some((player.get_total_frames(), player.get_total_milliseconds().0)));

            return Some(match (before, after) {
                (Some((f0, ms0)), Some((f1, ms1))) if f1 > f0 && frame >= f0 => {
                    let frame = frame.min(f1);
                    (ms0 + (ms1.saturating_sub(ms0)) * (frame - f0) / (f1 - f0)).into()
                }
                (Some((_, ms0)), _) => ms0.into(),
                _ => 0.into()
            })
        }

        if self.replay_file_recorder.is_some() {
            let frames_back = self.total_frames.saturating_sub(frame);
            let back_micros = frames_back.saturating_mul(self.nominal_frame_micros());
            return Some(self.total_milliseconds.0.saturating_sub(back_micros / 1000).into())
        }

        None
    }

    /// Length of one frame at 1x speed on the loaded console, in microseconds.
    fn nominal_frame_micros(&self) -> u64 {
        match self.core.replay_console_type() {
            // 4194304 Hz / 70224 cycles per frame = 59.7275 Hz (the GBA's refresh is the same)
            Some(ReplayConsoleType::GameBoy | ReplayConsoleType::SuperGameBoy2 | ReplayConsoleType::GameBoyColor | ReplayConsoleType::GameBoyAdvance) => 16_743,
            // 59.8261 Hz
            Some(ReplayConsoleType::NintendoDS) => 16_715,
            _ => 16_667
        }
    }

    /// Replace the bookmarks of the replay being recorded (written into it; see
    /// `ReplayFileRecorder::set_bookmark_table`). Does nothing when not recording.
    pub fn set_replay_bookmarks(&mut self, table: BookmarkTable) {
        self.with_recorder(|r| r.set_bookmark_table(table));
    }

    fn handle_replay(&mut self) {
        if self.replay_stalled || self.replay_frame_pending || self.replay_playback_stopped {
            return
        }

        if self.core.is_mid_frame() {
            return
        }

        let Some(mut player) = self.replay_player.take() else {
            return
        };

        loop {
            match player.next_packet() {
                Ok(None) => {
                    self.replay_stalled = true;
                    break;
                },
                Ok(Some(n)) => {
                    match n {
                        Packet::NoOp => {}
                        Packet::NextFrame { timestamp_delta } => {
                            self.total_milliseconds = self.total_milliseconds.0.wrapping_add(timestamp_delta.0).into();
                            // Nothing more is read from the replay until this frame has run.
                            self.replay_frame_pending = true;
                            break;
                        }
                        Packet::WriteMemory { address, data } => {
                            // Skipped rather than fatal: a replay may write to memory this version
                            // does not map (including an address that no longer fits in u32).
                            let wrote = u32::try_from(*address).is_ok_and(|address| self.core.write_ram(address, data.as_slice()).is_ok());
                            if !wrote {
                                self.replay_write_failures += 1;
                            }
                        }
                        Packet::ChangeInput { data } => {
                            self.core.set_input_encoded(data.as_slice());
                        }
                        Packet::ChangeSpeed { speed } => {
                            self.replay_playback_speed = *speed;
                            self.match_replay_playback_speed();
                        }
                        Packet::ResetConsole => {
                            self.core.hard_reset();
                            self.bump_state_epoch();
                        }
                        Packet::LoadSaveState { state } => {
                            let _ = self.core.load_save_state(state.as_slice());
                            self.bump_state_epoch();
                        },
                        Packet::Bookmark { .. } | Packet::BookmarkTable { .. } => {}
                        Packet::Keyframe { .. } => {
                            if self.auto_resync_keyframes_in_replays {
                                // The player is told not to copy states into packets (see
                                // `attach_replay_player`); read the reconstructed state directly.
                                let state = self.splice_live_transient_buffers(player.current_keyframe_state());
                                let _ = self.core.load_save_state(&state);
                            }
                        }
                        // The player materialises every delta variant into a Keyframe before
                        // handing it out; these arms are unreachable in practice.
                        Packet::DeltaKeyframe { .. } | Packet::RegionDeltaKeyframe { .. } => {},
                        Packet::CompressedBlob { .. } => unreachable!("compressed blob"),
                        Packet::IncrementCounter { name, delta } => {
                            self.change_replay_counter_map(&name, *delta);
                        }
                    }
                }
                Err(_) => {
                    self.replay_stalled = true;
                    break
                }
            }
        }

        self.replay_player = Some(player);
    }

    /// Copy the emulator's own regenerated output buffers (melonDS 3D vertex/polygon banks, mGBA
    /// mixed PCM; see [`transient_ranges`]) over the corresponding bytes of a keyframe `state` that
    /// is about to be loaded while the emulator is already at that frame.
    ///
    /// Delta keyframes may carry their chain restart's stale copy of those buffers
    /// (`ReplayFileRecorderSettings::mask_transient_buffers`); splicing the live ones in means a
    /// resync never presents a frame built from stale geometry or audio. Returns `state` unchanged
    /// when the console has no such buffers or the layouts differ.
    fn splice_live_transient_buffers<'a>(&self, state: &'a [u8]) -> alloc::borrow::Cow<'a, [u8]> {
        let Some(console) = self.core.replay_console_type() else {
            return alloc::borrow::Cow::Borrowed(state)
        };

        let ranges = transient_ranges(console, state);
        if ranges.is_empty() {
            return alloc::borrow::Cow::Borrowed(state)
        }

        let live = self.core.create_save_state();
        if live.len() != state.len() || transient_ranges(console, &live) != ranges {
            return alloc::borrow::Cow::Borrowed(state)
        }

        let mut spliced = state.to_vec();
        for range in ranges {
            spliced[range.clone()].copy_from_slice(&live[range]);
        }
        alloc::borrow::Cow::Owned(spliced)
    }

    fn before_run(&mut self) {
        self.handle_replay();
        self.update_input();
        self.flush_writes();
    }

    fn after_run(&mut self, time: &RunTime) {
        self.last_run = *time;
        self.run_serial = self.run_serial.wrapping_add(1);
        self.do_frame_timekeeping(time);
        self.push_keyframe_if_needed(time);
    }

    fn flush_writes(&mut self) {
        if self.is_playing_back() {
            return
        }

        if self.core.is_mid_frame() {
            return
        }

        let mut writes = core::mem::take(&mut self.writes);

        for write in writes.drain(..) {
            // Only record what was actually written: a replay must not carry a write it cannot
            // apply on playback.
            if self.core.write_ram(write.address, write.data.as_slice()).is_ok() {
                self.with_recorder(|recorder| recorder.write_memory(write.address as UnsignedInteger, write.data));
            }
        }

        // reuse the allocation
        self.writes = writes;
    }

    /// Enqueue an input for the next frame.
    pub fn enqueue_input(&mut self, input: Input) {
        self.next_input = Some(input);
    }

    /// Do a hard reset. Ignored while a replay is playing back (it owns the console); a stopped
    /// replay's resume seeks back to its resume point regardless of what was done live.
    pub fn hard_reset(&mut self) {
        if self.is_playing_back() {
            return;
        }
        self.finish_current_frame();
        self.core.hard_reset();
        self.bump_state_epoch();
        self.clear_audio();
        self.with_recorder(|r| r.reset_console());
        // The console's own input state was just replaced; apply ours again on the next run.
        self.input_latched = false;
    }

    /// Set the current rapid fire input.
    pub fn set_rapid_fire_input(&mut self, input: Option<SuperShuckieRapidFire>) {
        // The input to apply changed without a new `enqueue_input`; re-apply it on the next run.
        self.input_latched = false;

        let Some(mut input) = input else {
            self.rapid_fire_input = None;
            return
        };

        input.total_frames = input.hold_length.get().saturating_add(input.interval.get());

        if let Some(old_input) = self.rapid_fire_input.take() && input.hold_length == old_input.hold_length && input.interval == old_input.interval {
            // copy over the duty cycle
            input.current_frame = old_input.current_frame;
        }
        else {
            // reset the duty cycle so that the button is activated on the very next frame
            if self.core.is_mid_frame() {
                input.current_frame = input.total_frames - 1;
            }
            else {
                input.current_frame = 0;
            }
        }

        self.rapid_fire_input = Some(input);
    }

    /// Create a save state.
    pub fn create_save_state(&self) -> Vec<u8> {
        self.core.create_save_state()
    }

    /// Get the SRAM.
    pub fn save_sram(&self) -> Vec<u8> {
        self.core.save_sram()
    }

    /// Load a save state. Ignored while a replay is playing back (see [`Self::hard_reset`]).
    pub fn load_save_state(&mut self, state: &[u8]) {
        if self.is_playing_back() {
            return
        }

        let _ = self.core.load_save_state(state);
        self.bump_state_epoch();
        self.clear_audio();

        if self.replay_file_recorder.is_some() {
            self.with_recorder(|r| r.load_save_state(state.into()));
        }
        else {
            // Draw one frame from the loaded state (so the screens show something), then reload it
            // so the game does not appear to have run a frame it should not have.
            self.run_unlocked();
            self.finish_current_frame();
            let _ = self.core.load_save_state(state);
        }
        // See `hard_reset`.
        self.input_latched = false;
    }

    /// Set the current toggled input.
    ///
    /// Any activated buttons will be "stuck".
    pub fn set_toggled_input(&mut self, input: Option<Input>) {
        self.toggled_input = input;
        // See `set_rapid_fire_input`.
        self.input_latched = false;
    }

    /// Modify a counter, adding `delta`.
    pub fn change_replay_counter(&mut self, name: String, delta: SignedInteger) {
        if self.replay_file_recorder.is_none() {
            return
        }
        self.change_replay_counter_map(&name, delta);
        self.with_recorder(|r| r.change_counter(name, delta))
    }

    fn change_replay_counter_map(&mut self, name: &String, delta: SignedInteger) {
        let counters = self.replay_counters
            .as_mut()
            .expect("replay_counters is None even though we're recording a replay");
        if let Some(v) = counters.get_mut(name) {
            *v = v.wrapping_add(delta)
        }
        else {
            counters.insert(name.to_owned(), delta);
        }
    }

    /// Start recording a replay.
    pub fn start_recording_replay<
        FS: ReplayFileSink + Send + Sync + 'static,
        TS: ReplayFileSink + Send + Sync + 'static
    >(&mut self, partial_replay_record_metadata: PartialReplayRecordMetadata<FS, TS>) -> Result<(), ReplayFileWriteError> {
        let Some(console_type) = self.core.replay_console_type() else {
            return Err(ReplayFileWriteError::BadInput {
                explanation: alloc::borrow::Cow::Borrowed("this core cannot record replays (no console type)")
            })
        };

        self.stop_recording_replay();
        self.detach_replay_player();

        let rom_checksum = self.core.rom_checksum().to_owned();
        let bios_checksum = self.core.bios_checksum().to_owned();
        let emulator_core_name = self.core.core_name().to_owned();
        let initial_input = self.current_input;
        let initial_speed = self.game_speed;

        self.finish_current_frame();

        let initial_state = ByteVec::Heap(self.core.create_save_state());
        let mut initial_input_data = Vec::new();
        self.core.encode_input(initial_input, &mut initial_input_data);
        self.core.set_input_encoded(&initial_input_data);
        self.restart_timer();

        let recorder = NonBlockingReplayFileRecorder::new(ReplayFileRecorder::new_with_metadata(
            ReplayFileMetadata {
                console_type,
                rom_name: partial_replay_record_metadata.rom_name,
                rom_filename: partial_replay_record_metadata.rom_filename,
                rom_checksum,
                bios_checksum,
                emulator_core_name,
                patch_format: ReplayPatchFormat::Unpatched,
                patch_target_checksum: ReplayHeaderBlake3Hash::default(),
                crop_start: None,
                crop_end: None,
                timer_offset: None
            },

            ByteVec::new(),
            partial_replay_record_metadata.settings,
            self.total_milliseconds,

            ByteVec::Heap(initial_input_data),
            initial_speed,
            initial_state,
            partial_replay_record_metadata.final_file,
            partial_replay_record_metadata.temp_file
        )?);

        self.frames_per_keyframe = partial_replay_record_metadata.frames_per_keyframe.get();
        self.full_keyframe_pending = false;
        self.replay_file_recorder = Some(Box::new(recorder));
        self.replay_counters = Some(BTreeMap::new());
        // Record the input with the first frame rather than rely on the header's initial input.
        self.input_latched = false;

        Ok(())
    }

    /// Resume recording from an existing replay.
    ///
    /// The source replay must already be attached as the active replay player (e.g. via
    /// `attach_replay_player`); this method consumes that player to build the new file's prefix and
    /// then continues recording live. `resume_at_frame == None` resumes from the final frame. Never
    /// mutates the source.
    ///
    /// The new file starts with `bookmarks` (`None` = the source's) cut at the resume frame.
    pub fn resume_recording_replay<FS, TS>(
        &mut self,
        resume_at_frame: Option<UnsignedInteger>,
        partial: PartialReplayRecordMetadata<FS, TS>,
        crop_policy: ResumeCropPolicy,
        bookmarks: Option<BookmarkTable>,
    ) -> Result<(), ReplayResumeError>
    where
        FS: ReplayFileSink + Send + Sync + 'static,
        TS: ReplayFileSink + Send + Sync + 'static,
    {
        // Read the source's total frame count into a local before invoking other &mut self
        // methods (avoids overlapping borrows of `self.replay_player`).
        let total = self
            .replay_player
            .as_ref()
            .map(|p| p.get_total_frames())
            .ok_or(ReplayResumeError::BadSource {
                explanation: alloc::borrow::Cow::Borrowed("no replay player attached to resume from"),
            })?;
        let target_for_emulator = resume_at_frame.unwrap_or(total);

        // Position the emulator at the resume frame using the existing seek logic.
        if let Err(explanation) = self.go_to_replay_frame(target_for_emulator) {
            return Err(ReplayResumeError::BadSource { explanation: alloc::borrow::Cow::Owned(explanation) })
        }

        // Reuse the attached source player to build the prefix. Positioning the emulator above has
        // already finished with its cursor, and we are about to detach it anyway, so we take
        // ownership and feed it directly. This avoids parsing and holding a SECOND full copy of the
        // replay in RAM — critical for long Nintendo DS replays, which can be many gigabytes.
        let mut source_player = self.replay_player.take().ok_or(ReplayResumeError::BadSource {
            explanation: alloc::borrow::Cow::Borrowed("no replay player attached to resume from"),
        })?;
        // The resume builder reads keyframe states out of the packets it is handed.
        source_player.set_keyframe_states_wanted(true);
        let (recorder, info) = match build_resumed_recorder(
            &mut source_player,
            resume_at_frame,
            partial.settings,
            crop_policy,
            bookmarks,
            partial.final_file,
            partial.temp_file,
        ) {
            Ok(built) => built,
            Err(e) => {
                // The source replay is still good; restore it (and the emulator's position in it)
                // so a failed resume attempt does not cost the caller their attached player.
                source_player.set_keyframe_states_wanted(false);
                self.replay_player = Some(source_player);
                let _ = self.go_to_replay_frame(target_for_emulator);
                return Err(e)
            }
        };

        // The source player is taken out and dropped here, ending playback. We replicate the
        // input-preserving detach side effects inline (detach_replay_player_keep_input would now
        // early-return since replay_player is already None): clear playback state WITHOUT calling
        // reset_input(), so the input held at the resume frame survives into the first live frame.
        drop(source_player);
        self.replay_stalled = false;
        self.replay_frame_pending = false;
        self.input_latched = false;
        self.replay_playback_stopped = false;
        self.replay_counters = None;

        // Prime the live wall-clock timer to continue from the resume point.
        self.resume_timer(info.elapsed_millis, info.elapsed_frames);

        // Restore the counters captured at the resume point.
        self.replay_counters = Some(info.counters.iter().map(|c| (c.name.clone(), c.value)).collect());

        self.frames_since_last_keyframe = 0;

        // Prime the emulator's encoded input from the resume point. The logical `Input` bitflags
        // cannot be reconstructed without a decoder (none exists), so live input proceeds from the
        // user's current input on subsequent frames; only the emulator-side encoded state carries
        // continuity across the resume boundary.
        self.core.set_input_encoded(info.input.as_slice());

        // Install the resumed recorder.
        self.full_keyframe_pending = false;
        self.replay_file_recorder = Some(Box::new(NonBlockingReplayFileRecorder::new(recorder)));
        self.frames_per_keyframe = partial.frames_per_keyframe.get();

        // Restore speed (recorder dedups identical speed, so no spurious ChangeSpeed packet).
        self.set_speed(info.speed);

        Ok(())
    }

    /// Get number of milliseconds
    ///
    /// This will reset to 0 whenever a replay is started.
    pub fn get_recording_milliseconds(&self) -> TimestampMillis {
        self.total_milliseconds
    }

    /// Frames emulated since the recording or playback started (or since the core was created).
    pub fn total_frames(&self) -> u64 {
        self.total_frames
    }

    /// Whether an attached replay has run out of packets (or failed to read).
    pub fn is_replay_stalled(&self) -> bool {
        self.replay_stalled
    }

    /// Stop recording the current replay.
    ///
    /// Returns None if no replay was being recorded. Otherwise, returns Some(true) if successfully closed, or Some(false) if not.
    pub fn stop_recording_replay(&mut self) -> Option<bool> {
        if let Some(mut old_recorder) = self.replay_file_recorder.take() {
            self.replay_counters = None;
            return if !old_recorder.is_closed() {
                Some(old_recorder.close().is_ok())
            }
            else {
                Some(true)
            }
        }

        None
    }

    /// Forcibly stop recording the current replay.
    pub fn force_stop_recording_replay(&mut self) {
        self.replay_file_recorder = None;
    }

    fn with_recorder<F: FnOnce(&mut dyn ReplayFileRecorderFns) -> Result<(), ReplayFileWriteError>>(&mut self, what: F) {
        if let Some(n) = self.replay_file_recorder.as_mut() {
            let _ = what(Box::as_mut(n));
        }
    }

    fn update_input(&mut self) {
        if self.is_playing_back() {
            return
        }

        if self.core.is_mid_frame() {
            return
        }

        // A paced core's `run` is polled many times per emulated frame: apply the input once, on
        // the first poll after a frame, and again only if a new one has arrived since.
        if self.input_latched && self.next_input.is_none() {
            return
        }
        self.input_latched = true;

        if let Some(pending_input) = self.next_input.take() {
            self.base_input = pending_input;
        };

        let mut new_input = self.base_input;
        if let Some(rapid_fire_input) = self.rapid_fire_input && rapid_fire_input.current_frame < rapid_fire_input.hold_length.get() {
            new_input |= rapid_fire_input.input;
        }

        if let Some(toggled_input) = self.toggled_input {
            new_input |= toggled_input
        }

        self.current_input = new_input;
        self.input_scratch_buffer.clear();

        self.core.encode_input(self.current_input, &mut self.input_scratch_buffer);
        self.core.set_input_encoded(self.input_scratch_buffer.as_slice());

        if self.replay_file_recorder.is_some() {
            let mut data = ByteVec::with_capacity(self.input_scratch_buffer.len());
            data.extend_from_slice(self.input_scratch_buffer.as_slice());
            self.with_recorder(|f| f.set_input(data));
        }
    }

    fn do_frame_timekeeping(&mut self, time: &RunTime) {
        self.frames_since_last_keyframe += time.frames;
        self.total_frames = self.total_frames.wrapping_add(time.frames);

        if time.frames > 0 {
            self.replay_frame_pending = false;
            self.input_latched = false;

            if let Some(rf) = self.rapid_fire_input.as_mut() {
                // Advance the duty cycle once per emulated frame (not once per call: a paced core's
                // `run` is polled far more often than it actually advances a frame).
                rf.current_frame = (rf.current_frame + (time.frames % rf.total_frames)) % rf.total_frames;
            }

            if !self.is_playing_back() {
                let ms = self.current_timer_millis();
                self.total_milliseconds = ms;
                for _ in 0..time.frames {
                    self.with_recorder(|f| f.next_frame(ms));
                }
            }
        }
    }

    fn push_keyframe_if_needed(&mut self, time: &RunTime) {
        if time.frames == 0 || self.core.is_mid_frame() || self.replay_file_recorder.is_none() {
            return
        }

        let full = core::mem::take(&mut self.full_keyframe_pending);
        if full || self.frames_since_last_keyframe >= self.frames_per_keyframe {
            self.write_keyframe(full);
        }
    }

    /// Write a keyframe of the current state into the recording (always stored in full if `full`)
    /// and restart the keyframe interval.
    fn write_keyframe(&mut self, full: bool) {
        let ms = self.total_milliseconds;

        let mut buffer = self.take_state_buffer();
        self.core.create_save_state_into(&mut buffer);
        let result = self.replay_file_recorder.as_mut().map(|f| if full {
            f.insert_keyframe_full(ByteVec::Heap(buffer), ms)
        }
        else {
            f.insert_keyframe(ByteVec::Heap(buffer), ms)
        });

        // Only restart the interval when the keyframe actually went in (a temp-sink-only failure
        // still wrote it to the final file); otherwise the next frame tries again and a requested
        // full keyframe stays requested.
        match result {
            Some(Ok(_)) | Some(Err(ReplayFileWriteError::TempSink { .. })) => self.frames_since_last_keyframe = 0,
            _ => self.full_keyframe_pending |= full
        }
    }

    /// A buffer to create a keyframe state into: a recycled one when available, since a fresh
    /// multi-megabyte allocation costs milliseconds of page faults and a reused one is a plain copy.
    fn take_state_buffer(&mut self) -> Vec<u8> {
        while self.state_buffers.len() < Self::STATE_BUFFER_POOL
            && let Some(buffer) = self.replay_file_recorder.as_mut().and_then(|r| r.take_free_state_buffer())
        {
            self.state_buffers.push(buffer);
        }
        self.state_buffers.pop().unwrap_or_default()
    }

    /// Attach a replay file player to the core.
    ///
    /// The console-type and metadata compatibility checks happen before anything about the core
    /// changes: an incompatible or mismatched replay leaves any live recording/playback untouched
    /// (see [`ReplayPlayerAttachError`]). Only after those checks pass is the current recording
    /// stopped and any previously attached player detached.
    pub fn attach_replay_player(&mut self, mut player: ReplayFilePlayer, allow_mismatched: bool) -> Result<(), ReplayPlayerAttachError> {
        let metadata = player.get_replay_metadata();
        let core_console_type = self.core.replay_console_type();

        if Some(metadata.console_type) != core_console_type {
            return Err(ReplayPlayerAttachError::Incompatible {
                description: format!("Console types don't match! (replay: {:?}, rom: {core_console_type:?})", metadata.console_type)
            })
        }

        if !allow_mismatched {
            let mut mismatched_list = Vec::new();

            let rom_checksum = *self.core.rom_checksum();
            let bios_checksum = *self.core.bios_checksum();
            let core_name = self.core.core_name();

            if metadata.rom_checksum != rom_checksum {
                mismatched_list.push(ReplayPlayerMetadataMismatchKind::ROMChecksumMismatch { replay: metadata.rom_checksum, loaded: rom_checksum })
            }

            if metadata.bios_checksum != bios_checksum {
                mismatched_list.push(ReplayPlayerMetadataMismatchKind::BIOSChecksumMismatch { replay: metadata.bios_checksum, loaded: bios_checksum })
            }

            if metadata.emulator_core_name != core_name {
                mismatched_list.push(ReplayPlayerMetadataMismatchKind::CoreMismatch { replay: metadata.emulator_core_name.clone(), loaded: core_name.to_owned() })
            }

            if !mismatched_list.is_empty() {
                return Err(ReplayPlayerAttachError::MismatchedMetadata { issues: mismatched_list })
            }
        }

        self.stop_recording_replay();
        self.detach_replay_player();

        if let Err(e) = player.go_to_keyframe(0) {
            // Nothing was attached yet (the detach above already cleared any previous player), but
            // call it anyway defensively so this stays correct if that ordering ever changes.
            self.detach_replay_player();
            return Err(ReplayPlayerAttachError::Failed { description: format!("can't go to the first keyframe: {e:?}") })
        }

        // Every keyframe the cursor passes is reconstructed anyway; do not also copy it into the
        // packet (20 MB per keyframe on NDS). Seeks and resyncs read `current_keyframe_state`.
        player.set_keyframe_states_wanted(false);

        self.current_input = Input::new();
        self.next_input = None;
        // Writes still queued for the end of a frame belong to the live session, not the replay.
        self.writes.clear();
        self.replay_player = Some(player);
        self.replay_counters = Some(BTreeMap::new());
        self.replay_stalled = false;
        self.replay_frame_pending = false;
        self.input_latched = false;
        self.replay_playback_stopped = false;
        self.restart_timer();

        if let Err(e) = self.go_to_replay_frame_inner(0, 0) {
            self.detach_replay_player();
            return Err(ReplayPlayerAttachError::Failed { description: e })
        }

        Ok(())
    }

    /// Detach the current replay player.
    pub fn detach_replay_player(&mut self) {
        if self.replay_player.is_none() {
            return;
        }

        self.replay_stalled = false;
        self.replay_frame_pending = false;
        self.input_latched = false;
        self.replay_playback_stopped = false;
        self.replay_player = None;
        self.replay_counters = None;
        self.reset_input();
        self.clear_audio();
        self.bump_state_epoch();
    }

    fn resume_timer(&mut self, resume_millis: TimestampMillis, resume_frames: UnsignedInteger) {
        let now = self.timestamp_provider.get_timestamp_milliseconds();
        self.paused_timer_at = None;
        self.starting_milliseconds = now.wrapping_sub(resume_millis.0).into();
        self.total_milliseconds = resume_millis;
        self.total_frames = resume_frames;
    }

    /// Reset the current input.
    pub fn reset_input(&mut self) {
        self.enqueue_input(Input::new());
    }

    /// Minimum number of frames emulated after loading a keyframe when seeking.
    ///
    /// Keyframe bookmarks sit this many frames after their keyframe
    /// ([`KEYFRAME_BOOKMARK_LEAD_FRAMES`]), so a seek to one loads exactly that keyframe.
    ///
    /// A keyframe loaded from the middle of a delta chain may hold its chain restart's stale copy
    /// of regenerated output buffers (see [`transient_ranges`]); the game rebuilds them on its next
    /// frame (or the one after, for games that only resubmit 3D geometry every other frame), so a
    /// seek always emulates at least this many frames past the keyframe before a frame is shown.
    /// A consumer stepping from [`Self::go_to_replay_keyframe`] itself must do the same.
    pub const POST_LOAD_FRAMES: u64 = 3;

    const _KEYFRAME_BOOKMARKS_MATCH_SEEKS: () = assert!(Self::POST_LOAD_FRAMES == KEYFRAME_BOOKMARK_LEAD_FRAMES);

    /// Seek to the given frame (if a replay is attached).
    ///
    /// Afterwards the emulator has run `max(frame, 1)` frames (clamped to the replay's length) and
    /// the framebuffer holds the last of them.
    ///
    /// A stopped replay (see [`Self::stop_replay_playback`]) drives the emulator only for the
    /// duration of the seek: afterwards the user is back in control at the new frame, which
    /// becomes the resume point.
    ///
    /// Returns an error (and leaves the core stalled, unless stopped) if the replay could not be
    /// read at the target; see [`Self::load_replay_keyframe_at_or_before`].
    pub fn go_to_replay_frame(&mut self, frame: UnsignedInteger) -> Result<(), String> {
        // Load a keyframe at least POST_LOAD_FRAMES before the target, then run until the frame
        // before the target has been emulated so that the target itself is the one rendered.
        let keyframe_hint = frame.saturating_sub(Self::POST_LOAD_FRAMES);
        let desired = frame.saturating_sub(1);
        self.go_to_replay_frame_inner(keyframe_hint, desired)
    }

    fn go_to_replay_frame_inner(&mut self, frame: UnsignedInteger, desired: UnsignedInteger) -> Result<(), String> {
        if self.replay_player.is_none() {
            return Ok(())
        }

        if !self.replay_playback_stopped {
            return self.seek_in_replay(frame, desired)
        }

        self.replay_playback_stopped = false;
        let result = self.seek_in_replay(frame, desired);
        // Back to the user at the new position, keeping whatever they hold pressed (unlike an
        // explicit stop, nothing about their input changed).
        self.stop_replay_playback_here();
        result
    }

    /// The seek itself, with the replay driving; see [`Self::go_to_replay_frame`].
    fn seek_in_replay(&mut self, frame: UnsignedInteger, desired: UnsignedInteger) -> Result<(), String> {
        let Some(p) = self.replay_player.as_mut() else {
            return Ok(())
        };

        let desired = desired.min(p.get_total_frames().saturating_sub(1));
        if desired >= p.get_total_frames() {
            return Ok(())
        }

        self.load_replay_keyframe_at_or_before(frame)?;

        // Only the target frame is looked at; the ones on the way there need not be drawn.
        while self.total_frames <= desired && !self.replay_stalled {
            if self.total_frames < desired {
                self.run_unlocked_hidden();
            }
            else {
                self.run_unlocked();
            }
        }
        Ok(())
    }

    /// Load the attached replay's nearest keyframe at or before `frame` without emulating
    /// anything past it.
    ///
    /// Afterwards [`Self::total_frames`] is that keyframe's frame index (returned), the recorded
    /// input is restored and nothing has been drawn: the screens hold whatever the save state
    /// left there, so the caller must run at least one frame (see [`Self::run_unlocked_hidden`]
    /// and [`Self::run_unlocked`]) before showing anything. This is the first half of
    /// [`Self::go_to_replay_frame`], split out so a headless consumer can do the stepping half
    /// itself and abandon it part way. A seek should load a keyframe at least
    /// [`Self::POST_LOAD_FRAMES`] before the frame it will show, as `go_to_replay_frame` does.
    ///
    /// Returns an error (and leaves the core positioned wherever it was, marked stalled) when no
    /// replay is attached or the replay is unreadable at that keyframe.
    pub fn go_to_replay_keyframe(&mut self, frame: UnsignedInteger) -> Result<UnsignedInteger, String> {
        if self.replay_player.is_none() {
            return Err(String::from("no replay is attached"))
        }
        self.load_replay_keyframe_at_or_before(frame)?;
        Ok(self.total_frames)
    }

    /// Position the player at the nearest keyframe at or before `frame`, load its state and
    /// restore everything recorded alongside it. No frame is run.
    fn load_replay_keyframe_at_or_before(&mut self, mut frame: UnsignedInteger) -> Result<(), String> {
        let Some(p) = self.replay_player.as_mut() else {
            return Err(String::from("no replay is attached"))
        };

        loop {
            match p.go_to_keyframe(frame) {
                Ok(()) => break,
                Err(ReplaySeekError::NoSuchKeyframe { best, .. }) => {
                    if best >= frame {
                        self.replay_stalled = true;
                        return Err(format!("no keyframe at or before frame {frame}"))
                    }
                    frame = best;
                }
                Err(ReplaySeekError::ReadError { error }) => {
                    self.replay_stalled = true;
                    return Err(format!("cannot read the keyframe at frame {frame}: {error:?}"))
                }
            }
        }

        let Ok(Some(Packet::Keyframe { metadata, .. })) = p.next_packet() else {
            self.replay_stalled = true;
            return Err(format!("replay file is broken (no keyframe found at frame {frame})"))
        };

        let speed = metadata.speed;
        let elapsed_frames = metadata.elapsed_frames;
        let elapsed_millis = metadata.elapsed_millis;
        let counters = metadata.counters.iter().map(|c| (c.name.clone(), c.value)).collect();
        let input = metadata.input.clone();

        if let Err(e) = self.core.load_save_state(p.current_keyframe_state()) {
            self.replay_stalled = true;
            return Err(format!("replay file is broken (cannot load the save state at frame {frame}): {e}"))
        }
        self.state_epoch = self.state_epoch.wrapping_add(1);
        // Save states do not carry the buttons held (melonDS leaves KeyInput alone), and the
        // next ChangeInput packet may be far away, so restore the input recorded with the
        // keyframe; otherwise the frames after a seek depend on what was held before it.
        self.core.set_input_encoded(input.as_slice());

        self.total_frames = elapsed_frames;
        self.total_milliseconds = elapsed_millis;
        self.replay_stalled = false;
        // The cursor now sits right after the keyframe; its frame's packets are still to be read.
        self.replay_frame_pending = false;
        self.frames_since_last_keyframe = 0;
        self.replay_counters = Some(counters);
        self.replay_playback_speed = speed;
        self.clear_audio();

        self.match_replay_playback_speed();
        Ok(())
    }

    /// Get any errors for the replay writes.
    ///
    /// This should be called to ensure that it is still recording a replay.
    pub fn poll_replay_recording_errors(&mut self) -> Vec<ReplayFileWriteError> {
        self.replay_file_recorder
            .as_mut()
            .map(|r| r.get_errors())
            .unwrap_or(Vec::new())
    }

    /// Set whether or not to ignore speed changes in replays
    pub fn set_ignore_speed_changes_in_replays(&mut self, ignored: bool) {
        self.ignore_speed_changes_in_replays = ignored;
        if self.is_playing_back() {
            self.match_replay_playback_speed();
        }
    }

    /// Set whether or not to automatically resync keyframes on playback
    pub fn set_auto_resync_keyframes_in_replays(&mut self, resync: bool) {
        self.auto_resync_keyframes_in_replays = resync;
    }

    fn match_replay_playback_speed(&mut self) {
        if !self.ignore_speed_changes_in_replays {
            self.set_speed(self.replay_playback_speed);
        }
    }
}

/// Returns when an error occurs.
#[derive(Clone, Debug)]
pub enum ReplayPlayerAttachError {
    /// Metadata is mismatched. It may desync.
    #[allow(missing_docs)]
    MismatchedMetadata {
        issues: Vec<ReplayPlayerMetadataMismatchKind>
    },

    /// Metadata is mismatched.
    #[allow(missing_docs)]
    Incompatible {
        description: String
    },

    /// The replay is otherwise compatible, but could not actually be positioned at its first
    /// keyframe (a truncated/corrupted file, an unreadable compressed blob, etc.). The core is
    /// left detached: nothing was left half-attached.
    #[allow(missing_docs)]
    Failed {
        description: String
    }
}

impl Display for ReplayPlayerAttachError {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        match self {
            ReplayPlayerAttachError::MismatchedMetadata { issues } => {
                f.write_str("This replay file has mismatched data which may prevent playback:")?;
                for issue in issues {
                    f.write_str("\n\n")?;
                    Display::fmt(issue, f)?;
                }
                Ok(())
            }
            ReplayPlayerAttachError::Incompatible { description } => {
                f.write_fmt(format_args!("This replay file is incompatible:\n\n{description}"))
            }
            ReplayPlayerAttachError::Failed { description } => {
                f.write_fmt(format_args!("This replay could not be loaded:\n\n{description}"))
            }
        }
    }
}

/// Describes a metadata mismatch.
#[derive(Clone, Debug)]
#[allow(missing_docs)]
pub enum ReplayPlayerMetadataMismatchKind {
    ROMChecksumMismatch {
        replay: ReplayHeaderBlake3Hash,
        loaded: ReplayHeaderBlake3Hash
    },

    BIOSChecksumMismatch {
        replay: ReplayHeaderBlake3Hash,
        loaded: ReplayHeaderBlake3Hash
    },

    CoreMismatch {
        replay: String,
        loaded: String
    }
}

impl Display for ReplayPlayerMetadataMismatchKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        match self {
            ReplayPlayerMetadataMismatchKind::ROMChecksumMismatch { replay, loaded } => {
                f.write_fmt(format_args!(
                    "ROM checksum mismatch! Either the wrong ROM is loaded, or it was modified.\n\n  Replay: {}\n  Loaded: {}\n\nThis can cause potential desyncs.",
                    blake3_hash_to_ascii(*replay), blake3_hash_to_ascii(*loaded)
                ))
            }
            ReplayPlayerMetadataMismatchKind::BIOSChecksumMismatch { replay, loaded } => {
                f.write_fmt(format_args!(
                    "BIOS checksum mismatch! Either the wrong BIOS is loaded, or it was modified.\n\n  Replay: {}\n  Loaded: {}\n\nThis can cause potential desyncs.",
                    blake3_hash_to_ascii(*replay), blake3_hash_to_ascii(*loaded)
                ))
            }
            ReplayPlayerMetadataMismatchKind::CoreMismatch { replay, loaded } => {
                f.write_fmt(format_args!(
                    "ROM core mismatch! Different cores or different versions of cores were used.\n\n  Replay: {}\n  Loaded: {}\n\nThis can cause potential desyncs UNLESS both cores have equal accuracy.",
                    replay, loaded
                ))
            }
        }
    }
}

#[allow(missing_docs)]
pub type TimestampMicros = u64;

/// Function that monotonically produces a timestamp.
///
/// The timestamp must never go backwards, although it does not necessarily always have to go
/// forwards, either.
pub trait MonotonicTimestampProvider: Send {
    /// Get the timestamp in milliseconds.
    fn get_timestamp_microseconds(&mut self) -> TimestampMicros;

    /// Get the timestamp in microseconds.
    ///
    /// This does not need to be implemented.
    fn get_timestamp_milliseconds(&mut self) -> u64 {
        self.get_timestamp_microseconds() / 1000
    }
}

#[cfg(feature = "std")]
/// Generate a timestamp provider backed by [`std::time::Instant`]
pub fn std_timestamp_provider() -> Box<dyn MonotonicTimestampProvider> {
    Box::new(std_timestamp_provider::StdTimestampProvider::new())
}

#[cfg(feature = "std")]
mod std_timestamp_provider {
    use std::time::Instant;
    use supershuckie_replay_recorder::UnsignedInteger;
    use crate::MonotonicTimestampProvider;

    pub struct StdTimestampProvider {
        reference_time: Instant
    }

    impl StdTimestampProvider {
        pub fn new() -> Self {
            Self { reference_time: Instant::now() }
        }
    }

    impl MonotonicTimestampProvider for StdTimestampProvider {
        fn get_timestamp_microseconds(&mut self) -> u64 {
            (Instant::now() - self.reference_time).as_micros() as UnsignedInteger
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emulator::ScreenData;
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Mutex;
    use supershuckie_replay_recorder::replay_file::record::ReplayFileRecorderSettings;
    use supershuckie_replay_recorder::replay_file::ReplayHeaderBytes;

    /// A wall clock the test drives by hand, shared between the [`SuperShuckieCore`] and a fake
    /// paced core so both see the same time.
    #[derive(Clone)]
    struct FakeClock(Arc<AtomicU64>);

    impl FakeClock {
        fn new() -> Self {
            Self(Arc::new(AtomicU64::new(0)))
        }

        fn now(&self) -> u64 {
            self.0.load(Ordering::Relaxed)
        }

        fn advance(&self, micros: u64) {
            self.0.fetch_add(micros, Ordering::Relaxed);
        }
    }

    impl MonotonicTimestampProvider for FakeClock {
        fn get_timestamp_microseconds(&mut self) -> TimestampMicros {
            self.now()
        }
    }

    /// A sink that keeps its bytes reachable after being handed to the recorder: the
    /// `Box<dyn ReplayFileRecorderFns>` erasure `SuperShuckieCore` records through only returns
    /// `Result<(), ReplayFileWriteError>` from `close()`, discarding the sinks `close()` would
    /// otherwise hand back, so a plain `Vec<u8>` sink's bytes would be unreachable afterwards.
    #[derive(Clone, Default)]
    struct SharedSink(Arc<Mutex<Vec<u8>>>);

    impl ReplayFileSink for SharedSink {
        fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), ReplayFileWriteError> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(())
        }

        fn truncate(&mut self, size: u64) -> Result<(), ReplayFileWriteError> {
            self.0.lock().unwrap().truncate(size as usize);
            Ok(())
        }

        fn overwrite_header(&mut self, header_data: &ReplayHeaderBytes) -> Result<(), ReplayFileWriteError> {
            let mut buf = self.0.lock().unwrap();
            if buf.len() > header_data.len() {
                buf[..header_data.len()].copy_from_slice(header_data);
            }
            else {
                buf.clear();
                buf.extend_from_slice(header_data);
            }
            Ok(())
        }
    }

    /// A core modelled on [`crate::emulator::GameBoyAdvance`]/[`crate::emulator::NintendoDS`]:
    /// `run` paces itself off a clock and reports `RunTime::NONE` on every pacing miss, exactly
    /// like the real paced cores, while `run_unlocked` always advances one frame. It never
    /// overrides `is_mid_frame` (stays the default `false`).
    struct FakePacedCore {
        clock: FakeClock,
        period_micros: u64,
        last_frame_micros: u64,
        counter: u32,
        input_byte: u8,
        /// The input byte active on every frame that actually advanced, oldest first.
        frame_inputs: Arc<Mutex<Vec<u8>>>,
        rom_checksum: ReplayHeaderBlake3Hash,
        bios_checksum: ReplayHeaderBlake3Hash,
        /// A tiny fixed screen so consumers that need real geometry (e.g. video export) have
        /// something to composite; no test asserts on its pixel content.
        screens: Vec<ScreenData>
    }

    impl FakePacedCore {
        fn new(clock: FakeClock, period_micros: u64) -> Self {
            Self {
                clock,
                period_micros,
                last_frame_micros: 0,
                counter: 0,
                input_byte: 0,
                frame_inputs: Arc::new(Mutex::new(Vec::new())),
                rom_checksum: [0; 32],
                bios_checksum: [0; 32],
                screens: alloc::vec![ScreenData {
                    pixels: alloc::vec![0xFF112233; 4 * 4],
                    width: 4,
                    height: 4,
                    encoding: crate::emulator::ScreenDataEncoding::A8R8G8B8
                }]
            }
        }
    }

    impl EmulatorCore for FakePacedCore {
        fn run(&mut self) -> RunTime {
            let now = self.clock.now();
            let expected_next = self.last_frame_micros + self.period_micros;
            if now < expected_next {
                return RunTime::NONE
            }
            self.last_frame_micros = expected_next;
            self.run_unlocked()
        }

        fn run_unlocked(&mut self) -> RunTime {
            self.counter = self.counter.wrapping_add(1);
            self.frame_inputs.lock().unwrap().push(self.input_byte);
            RunTime::ONE_FRAME
        }

        fn read_ram(&self, _address: u32, _into: &mut [u8]) -> Result<(), &'static str> {
            Err("unsupported")
        }

        fn write_ram(&mut self, _address: u32, _from: &[u8]) -> Result<(), &'static str> {
            Err("unsupported")
        }

        fn set_speed(&mut self, _speed: f64) {}

        fn save_sram(&self) -> Vec<u8> {
            Vec::new()
        }

        fn create_save_state(&self) -> Vec<u8> {
            self.counter.to_le_bytes().to_vec()
        }

        fn microseconds_until_next_frame(&mut self) -> Option<u64> {
            Some((self.last_frame_micros + self.period_micros).saturating_sub(self.clock.now()))
        }

        fn frame_period_microseconds(&self) -> Option<u64> {
            Some(self.period_micros)
        }

        fn load_save_state(&mut self, state: &[u8]) -> Result<(), String> {
            let bytes: [u8; 4] = state.try_into().map_err(|_| String::from("bad state"))?;
            self.counter = u32::from_le_bytes(bytes);
            Ok(())
        }

        fn encode_input(&self, input: Input, into: &mut Vec<u8>) {
            into.push(input.a as u8);
        }

        fn set_input_encoded(&mut self, input: &[u8]) {
            self.input_byte = input.first().copied().unwrap_or(0);
        }

        fn get_screens(&self) -> &[ScreenData] {
            &self.screens
        }

        fn swap_screen_data(&mut self, _screens: &mut [ScreenData]) {}

        fn hard_reset(&mut self) {
            self.counter = 0;
        }

        fn replay_console_type(&self) -> Option<ReplayConsoleType> {
            Some(ReplayConsoleType::GameBoyAdvance)
        }

        fn rom_checksum(&self) -> &ReplayHeaderBlake3Hash {
            &self.rom_checksum
        }

        fn bios_checksum(&self) -> &ReplayHeaderBlake3Hash {
            &self.bios_checksum
        }

        fn core_name(&self) -> &'static str {
            "fake-paced"
        }

        fn frame_rate(&self) -> (u32, u32) {
            (60, 1)
        }
    }

    /// A core modelled on the Game Boy core's sub-frame stepping: `run`/`run_unlocked` report
    /// `RunTime::NONE` (and `is_mid_frame() == true`) a fixed number of times, then a whole frame.
    struct FakeSlicedCore {
        steps_before_frame: u8,
        step: u8,
        mid_frame: bool,
        rom_checksum: ReplayHeaderBlake3Hash,
        bios_checksum: ReplayHeaderBlake3Hash
    }

    impl FakeSlicedCore {
        fn new(steps_before_frame: u8) -> Self {
            Self { steps_before_frame, step: 0, mid_frame: false, rom_checksum: [0; 32], bios_checksum: [0; 32] }
        }
    }

    impl EmulatorCore for FakeSlicedCore {
        fn run(&mut self) -> RunTime {
            self.run_unlocked()
        }

        fn run_unlocked(&mut self) -> RunTime {
            if self.step < self.steps_before_frame {
                self.step += 1;
                self.mid_frame = true;
                RunTime::NONE
            }
            else {
                self.step = 0;
                self.mid_frame = false;
                RunTime::ONE_FRAME
            }
        }

        fn read_ram(&self, _address: u32, _into: &mut [u8]) -> Result<(), &'static str> {
            Err("unsupported")
        }

        fn write_ram(&mut self, _address: u32, _from: &[u8]) -> Result<(), &'static str> {
            Err("unsupported")
        }

        fn set_speed(&mut self, _speed: f64) {}

        fn save_sram(&self) -> Vec<u8> {
            Vec::new()
        }

        fn create_save_state(&self) -> Vec<u8> {
            Vec::new()
        }

        fn load_save_state(&mut self, _state: &[u8]) -> Result<(), String> {
            self.mid_frame = false;
            Ok(())
        }

        fn encode_input(&self, _input: Input, into: &mut Vec<u8>) {
            into.clear();
        }

        fn set_input_encoded(&mut self, _input: &[u8]) {}

        fn get_screens(&self) -> &[ScreenData] {
            &[]
        }

        fn swap_screen_data(&mut self, _screens: &mut [ScreenData]) {}

        fn hard_reset(&mut self) {
            self.step = 0;
            self.mid_frame = false;
        }

        fn replay_console_type(&self) -> Option<ReplayConsoleType> {
            Some(ReplayConsoleType::GameBoy)
        }

        fn rom_checksum(&self) -> &ReplayHeaderBlake3Hash {
            &self.rom_checksum
        }

        fn bios_checksum(&self) -> &ReplayHeaderBlake3Hash {
            &self.bios_checksum
        }

        fn core_name(&self) -> &'static str {
            "fake-sliced"
        }

        fn frame_rate(&self) -> (u32, u32) {
            (60, 1)
        }

        fn is_mid_frame(&self) -> bool {
            self.mid_frame
        }
    }

    fn metadata(final_file: SharedSink, temp_file: SharedSink) -> PartialReplayRecordMetadata<SharedSink, SharedSink> {
        PartialReplayRecordMetadata {
            rom_name: "fake".into(),
            rom_filename: "fake".into(),
            settings: ReplayFileRecorderSettings::default(),
            patch_format: ReplayPatchFormat::Unpatched,
            patch_target_checksum: Default::default(),
            patch_data: ByteVec::new(),
            frames_per_keyframe: NonZeroU64::new(1000).unwrap(),
            final_file,
            temp_file
        }
    }

    /// Advance the clock by exactly one frame period and run the one paced frame that unlocks.
    fn run_one_frame(core: &mut SuperShuckieCore, clock: &FakeClock, period_micros: u64) {
        let target = core.total_frames() + 1;
        while core.total_frames() < target {
            clock.advance(period_micros);
            core.run();
        }
    }

    const PERIOD_MICROS: u64 = 16_667;

    /// Regression test for the C2 root cause: a save state / bookmark taken while paused used to
    /// emulate a hidden frame stamped with a wall-clock timestamp inflated by the pause length,
    /// which made the next live frame's timestamp go backwards and silently stopped the recording.
    #[test]
    fn paused_save_state_and_bookmark_do_not_break_monotone_timestamps() {
        let clock = FakeClock::new();
        let fake_core = FakePacedCore::new(clock.clone(), PERIOD_MICROS);
        let mut core = SuperShuckieCore::new(Box::new(fake_core), Box::new(clock.clone()));

        let final_buf = SharedSink::default();
        let temp_buf = SharedSink::default();
        core.start_recording_replay(metadata(final_buf.clone(), temp_buf.clone())).expect("start recording");

        for _ in 0..10 {
            run_one_frame(&mut core, &clock, PERIOD_MICROS);
        }
        assert_eq!(core.total_frames(), 10);

        // Pause, let 5 seconds of wall-clock time pass, and exercise exactly the paused-state
        // paths that used to emulate a hidden frame (finish_current_frame, a save state, and a
        // keyframe bookmark).
        core.pause_timer();
        clock.advance(5_000_000);
        core.finish_current_frame();
        let _ = core.create_save_state();
        core.bookmark_anchor(true).expect("keyframe anchor while paused");
        core.unpause_timer();

        for _ in 0..10 {
            run_one_frame(&mut core, &clock, PERIOD_MICROS);
        }
        assert_eq!(core.total_frames(), 20);

        assert!(core.poll_replay_recording_errors().is_empty(), "the recording hit an error");
        assert_eq!(core.stop_recording_replay(), Some(true));

        let bytes = final_buf.0.lock().unwrap().clone();
        let mut player = ReplayFilePlayer::new(&bytes, false).expect("parse the recorded replay");
        assert_eq!(player.get_total_frames(), 20);

        let mut elapsed = 0u64;
        let mut frame_count = 0u64;
        while let Some(packet) = player.next_packet().expect("read packet") {
            if let Packet::NextFrame { timestamp_delta } = packet {
                let next = elapsed + timestamp_delta.0;
                assert!(next >= elapsed, "NextFrame timestamps must be monotone non-decreasing");
                elapsed = next;
                frame_count += 1;
            }
        }
        assert_eq!(frame_count, 20);
    }

    /// M1: the duty cycle must advance once per emulated frame, not once per call to `run` -- a
    /// paced core's `run` is polled far more often than it actually advances a frame.
    #[test]
    fn rapid_fire_toggles_once_per_emulated_frame_on_a_paced_core() {
        let clock = FakeClock::new();
        let fake_core = FakePacedCore::new(clock.clone(), PERIOD_MICROS);
        let log = fake_core.frame_inputs.clone();
        let mut core = SuperShuckieCore::new(Box::new(fake_core), Box::new(clock.clone()));

        let rf = SuperShuckieRapidFire {
            input: Input { a: true, ..Input::default() },
            hold_length: NonZeroU64::new(3).unwrap(),
            interval: NonZeroU64::new(3).unwrap(),
            ..Default::default()
        };
        core.set_rapid_fire_input(Some(rf));

        for _ in 0..12 {
            run_one_frame(&mut core, &clock, PERIOD_MICROS);
        }

        let seen = log.lock().unwrap().clone();
        let expected: Vec<u8> = (0..12u8).map(|i| u8::from((i % 6) < 3)).collect();
        assert_eq!(seen, expected, "rapid fire should hold for 3 frames then release for 3, repeating");
    }

    /// The thread loop polls a paced core's `run` many times per emulated frame (it wakes 1 ms
    /// before the deadline and spins through the loop until the core lets the frame through).
    /// Recording must write the input once per emulated frame, not once per poll: each extra
    /// `ChangeInput` packet is an allocation, a channel send and a temp-sink write on the hot
    /// path, and it bloats the replay.
    #[test]
    fn recording_writes_one_change_input_per_emulated_frame_on_a_paced_core() {
        const FRAMES: u64 = 20;
        const POLLS_PER_FRAME: u64 = 50;

        let clock = FakeClock::new();
        let fake_core = FakePacedCore::new(clock.clone(), PERIOD_MICROS);
        let log = fake_core.frame_inputs.clone();
        let mut core = SuperShuckieCore::new(Box::new(fake_core), Box::new(clock.clone()));

        let final_buf = SharedSink::default();
        let temp_buf = SharedSink::default();
        core.start_recording_replay(metadata(final_buf.clone(), temp_buf.clone())).expect("start recording");

        for frame in 0..FRAMES {
            // Pacing misses: the deadline has not arrived, so every one of these is a poll that
            // emulates nothing.
            for poll in 0..POLLS_PER_FRAME {
                // An input that arrives late in the interval (after the first poll has already
                // applied the previous one) must still reach the very next frame.
                if poll == POLLS_PER_FRAME / 2 {
                    core.enqueue_input(Input { a: frame % 2 == 0, ..Input::default() });
                }
                core.run();
                assert_eq!(core.last_run_time().frames, 0);
            }
            run_one_frame(&mut core, &clock, PERIOD_MICROS);
        }
        assert_eq!(core.total_frames(), FRAMES);

        let seen = log.lock().unwrap().clone();
        let expected: Vec<u8> = (0..FRAMES).map(|frame| u8::from(frame % 2 == 0)).collect();
        assert_eq!(seen, expected, "an input enqueued between frames should apply to the next frame");

        assert!(core.poll_replay_recording_errors().is_empty(), "the recording hit an error");
        assert_eq!(core.stop_recording_replay(), Some(true));

        let bytes = final_buf.0.lock().unwrap().clone();
        let mut player = ReplayFilePlayer::new(&bytes, false).expect("parse the recorded replay");
        assert_eq!(player.get_total_frames(), FRAMES);

        let mut change_inputs = 0u64;
        let mut next_frames = 0u64;
        while let Some(packet) = player.next_packet().expect("read packet") {
            match packet {
                Packet::ChangeInput { .. } => change_inputs += 1,
                Packet::NextFrame { .. } => next_frames += 1,
                _ => {}
            }
        }
        assert_eq!(next_frames, FRAMES);
        // One per frame from the first poll after the previous frame, plus one for the input
        // that arrived mid-interval.
        assert!(
            change_inputs <= 2 * FRAMES,
            "{change_inputs} ChangeInput packets for {FRAMES} frames: the input is being written on every pacing poll"
        );
    }

    /// C2 / general sanity: `finish_current_frame` must run a mid-frame-stepping core (the only
    /// kind `is_mid_frame` is ever true for) all the way to the next frame boundary.
    #[test]
    fn finish_current_frame_reaches_the_frame_boundary_on_a_sliced_core() {
        let mut core = SuperShuckieCore::new(Box::new(FakeSlicedCore::new(2)), Box::new(FakeClock::new()));

        // Start a partial step (mimics the Game Boy core's own sub-frame stepping).
        core.run_unlocked();
        assert!(core.is_mid_frame());
        assert_eq!(core.total_frames(), 0);

        core.finish_current_frame();

        assert!(!core.is_mid_frame());
        assert_eq!(core.total_frames(), 1);
    }

    /// (d) `start_recording_replay` on a core with no console type (the null core) must be
    /// rejected up front, before any side effect, rather than `expect`-panicking.
    #[test]
    fn start_recording_replay_on_a_null_core_is_rejected_without_side_effects() {
        let mut core = SuperShuckieCore::new(Box::new(crate::emulator::NullEmulatorCore), Box::new(FakeClock::new()));

        let final_buf = SharedSink::default();
        let temp_buf = SharedSink::default();
        let err = core.start_recording_replay(metadata(final_buf, temp_buf)).expect_err("a null core cannot record replays");
        assert!(matches!(err, ReplayFileWriteError::BadInput { .. }), "expected BadInput, got {err:?}");
        assert_eq!(core.stop_recording_replay(), None, "no recorder should have been installed");
    }

    /// (e) M3: attaching a replay whose console type is incompatible with the running core must
    /// leave an active recording untouched (the core used to stop/detach before validating).
    #[test]
    fn attach_replay_player_rejects_incompatible_console_without_touching_active_recording() {
        let clock = FakeClock::new();

        // Build a tiny GameBoy-console replay to attach.
        let mut source = SuperShuckieCore::new(Box::new(FakeSlicedCore::new(0)), Box::new(clock.clone()));
        let gb_final = SharedSink::default();
        let gb_temp = SharedSink::default();
        source.start_recording_replay(metadata(gb_final.clone(), gb_temp.clone())).expect("start recording gb");
        for _ in 0..5 {
            source.run_unlocked();
        }
        assert_eq!(source.stop_recording_replay(), Some(true));
        let gb_bytes = gb_final.0.lock().unwrap().clone();

        // A live GameBoyAdvance recording that must stay untouched by a failed attach.
        let mut core = SuperShuckieCore::new(Box::new(FakePacedCore::new(clock.clone(), PERIOD_MICROS)), Box::new(clock.clone()));
        let gba_final = SharedSink::default();
        let gba_temp = SharedSink::default();
        core.start_recording_replay(metadata(gba_final, gba_temp)).expect("start recording gba");

        let player = ReplayFilePlayer::new(&gb_bytes, false).expect("parse gb replay");
        let err = core.attach_replay_player(player, false).expect_err("console types differ, attach must fail");
        assert!(matches!(err, ReplayPlayerAttachError::Incompatible { .. }), "expected Incompatible, got {err:?}");

        // The live recording must still be active and closeable.
        assert_eq!(core.stop_recording_replay(), Some(true));
    }

    /// (f) A seek into a replay blob that fails to decompress/apply must return `Err` and mark
    /// the core stalled, never panic (the `todo!`s this fixes).
    #[test]
    fn go_to_replay_frame_into_a_corrupted_blob_returns_an_error_without_panicking() {
        let clock = FakeClock::new();
        let mut recorder = SuperShuckieCore::new(Box::new(FakePacedCore::new(clock.clone(), PERIOD_MICROS)), Box::new(clock.clone()));

        let final_buf = SharedSink::default();
        let temp_buf = SharedSink::default();

        // Tiny blobs (one keyframe each) so a later blob can be corrupted without touching the
        // blob frame 0's keyframe lives in (which `attach_replay_player` always reads).
        let settings = ReplayFileRecorderSettings {
            max_frames_per_blob: 3,
            minimum_uncompressed_bytes_per_blob: 1,
            ..ReplayFileRecorderSettings::default()
        };

        recorder.start_recording_replay(PartialReplayRecordMetadata {
            rom_name: "fake".into(),
            rom_filename: "fake".into(),
            settings,
            patch_format: ReplayPatchFormat::Unpatched,
            patch_target_checksum: Default::default(),
            patch_data: ByteVec::new(),
            frames_per_keyframe: NonZeroU64::new(3).unwrap(),
            final_file: final_buf.clone(),
            temp_file: temp_buf.clone()
        }).expect("start recording");

        for _ in 0..12 {
            run_one_frame(&mut recorder, &clock, PERIOD_MICROS);
        }
        assert_eq!(recorder.stop_recording_replay(), Some(true));

        let bytes = final_buf.0.lock().unwrap().clone();

        // Find a blob that does not hold frame 0 (attach only ever reads the first blob).
        let victim = {
            let player = ReplayFilePlayer::new(&bytes, false).expect("parse recording");
            player.all_uncompressed_packets().iter().find_map(|p| match p {
                Packet::CompressedBlob { compressed_data, elapsed_frames_start, .. } if *elapsed_frames_start > 0 => {
                    Some(compressed_data.as_slice().to_vec())
                }
                _ => None
            }).expect("the recording should have produced more than one blob")
        };
        assert!(victim.len() >= 8, "blob too small to reliably corrupt: {} bytes", victim.len());

        let mut corrupted = bytes.clone();
        let offset = corrupted.windows(victim.len()).position(|w| w == victim.as_slice())
            .expect("could not locate the victim blob's bytes in the file");
        for b in &mut corrupted[offset..offset + victim.len()] {
            *b = 0xFF;
        }

        let corrupted_player = ReplayFilePlayer::new(&corrupted, true).expect("outer structure is untouched by the corruption");

        let mut playback = SuperShuckieCore::new(Box::new(FakePacedCore::new(clock.clone(), PERIOD_MICROS)), Box::new(clock.clone()));
        playback.attach_replay_player(corrupted_player, true).expect("attach should succeed: only a later blob is corrupted");

        let result = playback.go_to_replay_frame(8);
        assert!(result.is_err(), "seeking into the corrupted blob should return an error");
        assert!(playback.is_replay_stalled(), "the core should be marked stalled after the failed seek");
    }

    /// A `VideoFrameSink` that only counts pushed frames; used by the export test below.
    #[derive(Default)]
    struct CountingSink {
        frames: u64
    }

    impl VideoFrameSink for CountingSink {
        fn begin(&mut self, _width: u32, _height: u32, _fps_num: u32, _fps_den: u32) -> Result<(), VideoExportError> {
            Ok(())
        }

        fn push_frame(&mut self, _argb: &[u32]) -> Result<(), VideoExportError> {
            self.frames += 1;
            Ok(())
        }

        fn finish(&mut self) -> Result<(), VideoExportError> {
            Ok(())
        }

        fn abort(&mut self) {}
    }

    /// (h) L2: exporting from frame 0 must emit exactly `span` pictures, ending with a
    /// `progress(span, span)` callback (it used to emit one frame too few).
    #[test]
    fn export_frames_from_zero_emits_exactly_span_pictures() {
        let clock = FakeClock::new();
        let mut recorder = SuperShuckieCore::new(Box::new(FakePacedCore::new(clock.clone(), PERIOD_MICROS)), Box::new(clock.clone()));

        let final_buf = SharedSink::default();
        let temp_buf = SharedSink::default();
        recorder.start_recording_replay(metadata(final_buf.clone(), temp_buf.clone())).expect("start recording");
        for _ in 0..10 {
            run_one_frame(&mut recorder, &clock, PERIOD_MICROS);
        }
        assert_eq!(recorder.stop_recording_replay(), Some(true));

        let bytes = final_buf.0.lock().unwrap().clone();
        let player = ReplayFilePlayer::new(&bytes, false).expect("parse recording");
        assert_eq!(player.get_total_frames(), 10);

        let mut playback = SuperShuckieCore::new(Box::new(FakePacedCore::new(clock.clone(), PERIOD_MICROS)), Box::new(clock.clone()));
        playback.attach_replay_player(player, true).expect("attach");

        let mut sink = CountingSink::default();
        let cancel = AtomicBool::new(false);
        let mut progress_calls = Vec::new();
        let range = ExportRange { start_frame: 0, end_frame: None };
        let result = playback.export_frames(range, ScreenLayout::default(), &mut sink, &cancel, |d, t| progress_calls.push((d, t)));

        assert!(result.is_ok(), "export should succeed: {result:?}");
        assert_eq!(sink.frames, 10, "expected exactly `span` pushed frames");
        assert_eq!(progress_calls.last().copied(), Some((10, 10)), "the last progress callback should report (span, span)");
    }

    /// Regression test: playback must consume exactly one frame's worth of replay packets per
    /// emulated frame. A paced core's `run` is polled more often than it advances a frame (the
    /// core thread wakes early and polls until the frame is due, see `thread.rs`), and a pacing
    /// miss must not advance the replay cursor: doing so races the recorded inputs ahead of the
    /// emulator, desyncing playback and ending it early.
    #[test]
    fn playback_consumes_one_replay_frame_per_emulated_frame_on_a_paced_core() {
        const FRAMES: u64 = 24;

        // Record: hold A for two frames, release for two, and so on.
        let clock = FakeClock::new();
        let fake = FakePacedCore::new(clock.clone(), PERIOD_MICROS);
        let recorded_log = fake.frame_inputs.clone();
        let mut core = SuperShuckieCore::new(Box::new(fake), Box::new(clock.clone()));
        let final_buf = SharedSink::default();
        core.start_recording_replay(metadata(final_buf.clone(), SharedSink::default())).expect("start recording");
        for i in 0..FRAMES {
            core.enqueue_input(Input { a: (i / 2) % 2 == 0, ..Input::default() });
            run_one_frame(&mut core, &clock, PERIOD_MICROS);
        }
        assert_eq!(core.stop_recording_replay(), Some(true));
        let recorded = recorded_log.lock().unwrap().clone();
        assert_eq!(recorded.len() as u64, FRAMES);
        let bytes = final_buf.0.lock().unwrap().clone();

        // Play back on a fresh core, with several pacing misses before every frame.
        let clock = FakeClock::new();
        let fake = FakePacedCore::new(clock.clone(), PERIOD_MICROS);
        let played_log = fake.frame_inputs.clone();
        let mut core = SuperShuckieCore::new(Box::new(fake), Box::new(clock.clone()));
        let player = ReplayFilePlayer::new(&bytes, false).expect("parse the recorded replay");
        core.attach_replay_player(player, true).expect("attach");

        while core.total_frames() < FRAMES {
            let before = core.total_frames();
            for _ in 0..4 {
                core.run(); // not yet due: a pacing miss
            }
            assert_eq!(core.total_frames(), before, "a pacing miss must not emulate a frame");
            assert!(!core.is_replay_stalled(), "playback stalled early at frame {before}");
            run_one_frame(&mut core, &clock, PERIOD_MICROS);
        }

        let played = played_log.lock().unwrap().clone();
        assert_eq!(played, recorded, "every frame must be emulated with the input that was recorded for it");
    }

    /// Record `frames` frames on a fresh paced core, holding A for two frames then releasing for
    /// two, and so on. Returns the replay bytes and the input byte of every recorded frame.
    fn record_alternating_replay(frames: u64) -> (Vec<u8>, Vec<u8>) {
        let clock = FakeClock::new();
        let fake = FakePacedCore::new(clock.clone(), PERIOD_MICROS);
        let recorded_log = fake.frame_inputs.clone();
        let mut core = SuperShuckieCore::new(Box::new(fake), Box::new(clock.clone()));
        let final_buf = SharedSink::default();
        core.start_recording_replay(metadata(final_buf.clone(), SharedSink::default())).expect("start recording");
        for i in 0..frames {
            core.enqueue_input(Input { a: (i / 2) % 2 == 0, ..Input::default() });
            run_one_frame(&mut core, &clock, PERIOD_MICROS);
        }
        assert_eq!(core.stop_recording_replay(), Some(true));
        let recorded = recorded_log.lock().unwrap().clone();
        assert_eq!(recorded.len() as u64, frames);
        (final_buf.0.lock().unwrap().clone(), recorded)
    }

    /// A fresh paced core with `bytes` attached for playback, plus its clock and input log.
    fn playback_core(bytes: &[u8]) -> (SuperShuckieCore, FakeClock, Arc<Mutex<Vec<u8>>>) {
        let clock = FakeClock::new();
        let fake = FakePacedCore::new(clock.clone(), PERIOD_MICROS);
        let played_log = fake.frame_inputs.clone();
        let mut core = SuperShuckieCore::new(Box::new(fake), Box::new(clock.clone()));
        let player = ReplayFilePlayer::new(bytes, false).expect("parse the recorded replay");
        core.attach_replay_player(player, true).expect("attach");
        (core, clock, played_log)
    }

    /// Stopping a replay keeps it attached but hands the emulator to the user: their input is
    /// what runs, the frame counter keeps counting, the resume point does not move, and resuming
    /// puts the emulator back at that point and plays the recording on from there.
    #[test]
    fn stopped_replay_runs_the_users_input_and_resumes_where_it_stopped() {
        const FRAMES: u64 = 24;
        const STOP_AT: u64 = 8;
        const LIVE_FRAMES: u64 = 5;

        let (bytes, recorded) = record_alternating_replay(FRAMES);
        let (mut core, clock, played_log) = playback_core(&bytes);

        while core.total_frames() < STOP_AT {
            run_one_frame(&mut core, &clock, PERIOD_MICROS);
        }
        assert!(core.is_playing_back());
        assert!(!core.is_replay_playback_stopped());

        assert!(core.stop_replay_playback());
        assert!(!core.stop_replay_playback(), "stopping twice must be a no-op");
        assert!(!core.is_playing_back(), "a stopped replay is not driving the emulator");
        assert!(core.has_replay_attached(), "...but it is still attached");
        assert!(core.is_replay_playback_stopped());
        assert_eq!(core.replay_position().0, STOP_AT);
        let stopped_millis = core.get_recording_milliseconds().0;

        // The recording has A released on frames 8..10 (and held on 12..14); the user holds A
        // throughout, and that is what must run.
        core.enqueue_input(Input { a: true, ..Input::default() });
        for _ in 0..LIVE_FRAMES {
            run_one_frame(&mut core, &clock, PERIOD_MICROS);
        }
        assert_eq!(core.total_frames(), STOP_AT + LIVE_FRAMES, "the live frame counter keeps counting");
        assert_eq!(core.replay_position().0, STOP_AT, "...but the resume point stays put");
        assert!(!core.is_replay_stalled());
        {
            let played = played_log.lock().unwrap();
            assert!(played[STOP_AT as usize..].iter().all(|&a| a == 1), "live frames must run the user's input: {played:?}");
        }
        let live_millis = core.get_recording_milliseconds().0;
        assert!(live_millis >= stopped_millis + LIVE_FRAMES * (PERIOD_MICROS / 1000), "the timer runs on from the replay's time while stopped ({stopped_millis} -> {live_millis})");

        core.resume_replay_playback().expect("resume");
        assert!(core.is_playing_back());
        assert!(!core.is_replay_playback_stopped());
        assert_eq!(core.total_frames(), STOP_AT, "resuming puts the emulator back at the resume point");

        // The user still "holds" A, which playback must ignore from here on.
        played_log.lock().unwrap().truncate(STOP_AT as usize);
        while core.total_frames() < FRAMES {
            run_one_frame(&mut core, &clock, PERIOD_MICROS);
        }
        let played = played_log.lock().unwrap().clone();
        assert_eq!(played, recorded, "after resuming, the recording plays on from the resume point");
    }

    /// Seeking a stopped replay moves the resume point and hands the emulator straight back to
    /// the user at the new frame.
    #[test]
    fn seeking_a_stopped_replay_moves_the_resume_point_and_keeps_the_user_in_control() {
        const FRAMES: u64 = 24;
        const SEEK_TO: u64 = 12;

        let (bytes, recorded) = record_alternating_replay(FRAMES);
        let (mut core, clock, played_log) = playback_core(&bytes);

        while core.total_frames() < 4 {
            run_one_frame(&mut core, &clock, PERIOD_MICROS);
        }
        assert!(core.stop_replay_playback());
        core.enqueue_input(Input { a: true, ..Input::default() });
        run_one_frame(&mut core, &clock, PERIOD_MICROS);

        core.go_to_replay_frame(SEEK_TO).expect("seek");
        assert!(core.is_replay_playback_stopped(), "a seek does not resume playback");
        assert!(!core.is_playing_back());
        assert_eq!(core.total_frames(), SEEK_TO);
        assert_eq!(core.replay_position().0, SEEK_TO, "the seek target is the new resume point");

        // Frames 12 and 13 have A held in the recording, 14 and 15 released; the user (still
        // holding A) is in control, so all four must run with A held.
        let before = played_log.lock().unwrap().len();
        for _ in 0..4 {
            run_one_frame(&mut core, &clock, PERIOD_MICROS);
        }
        {
            let played = played_log.lock().unwrap();
            assert!(played[before..].iter().all(|&a| a == 1), "the user stays in control after a seek: {played:?}");
        }
        assert_eq!(core.replay_position().0, SEEK_TO);

        core.resume_replay_playback().expect("resume");
        assert_eq!(core.total_frames(), SEEK_TO);
        let before = played_log.lock().unwrap().len();
        for _ in SEEK_TO..FRAMES {
            run_one_frame(&mut core, &clock, PERIOD_MICROS);
        }
        let played = played_log.lock().unwrap().clone();
        assert_eq!(&played[before..], &recorded[SEEK_TO as usize..], "playback resumes from the seek target");
    }

    /// Jumping back to the resume point of a stopped replay discards the live play since, keeps
    /// the user in control there, and leaves the resume point where it was; it follows seeks and
    /// does nothing while playing back.
    #[test]
    fn jumping_to_the_resume_point_rewinds_live_play_without_resuming_playback() {
        const FRAMES: u64 = 24;
        const STOP_AT: u64 = 8;
        const SEEK_TO: u64 = 14;

        let (bytes, recorded) = record_alternating_replay(FRAMES);
        let (mut core, clock, played_log) = playback_core(&bytes);

        while core.total_frames() < STOP_AT {
            run_one_frame(&mut core, &clock, PERIOD_MICROS);
        }
        core.go_to_replay_resume_point().expect("a no-op while playing back");
        assert!(core.is_playing_back());
        assert_eq!(core.total_frames(), STOP_AT);

        assert!(core.stop_replay_playback());
        core.enqueue_input(Input { a: true, ..Input::default() });
        for _ in 0..6 {
            run_one_frame(&mut core, &clock, PERIOD_MICROS);
        }
        assert_eq!(core.total_frames(), STOP_AT + 6);

        core.go_to_replay_resume_point().expect("jump");
        assert!(core.is_replay_playback_stopped(), "jumping back does not resume playback");
        assert!(!core.is_playing_back());
        assert_eq!(core.total_frames(), STOP_AT, "the emulator is back at the resume point");
        assert_eq!(core.replay_position().0, STOP_AT, "...which stays the resume point");

        // Still the user's game: the recording releases A on frames 8..10, the user holds it.
        let before = played_log.lock().unwrap().len();
        for _ in 0..3 {
            run_one_frame(&mut core, &clock, PERIOD_MICROS);
        }
        {
            let played = played_log.lock().unwrap();
            assert!(played[before..].iter().all(|&a| a == 1), "the user stays in control after jumping back: {played:?}");
        }

        // The jump follows the resume point when a seek moves it.
        core.go_to_replay_frame(SEEK_TO).expect("seek");
        run_one_frame(&mut core, &clock, PERIOD_MICROS);
        run_one_frame(&mut core, &clock, PERIOD_MICROS);
        core.go_to_replay_resume_point().expect("jump after a seek");
        assert_eq!(core.total_frames(), SEEK_TO);
        assert_eq!(core.replay_position().0, SEEK_TO);
        assert!(core.is_replay_playback_stopped());

        // And resuming from there plays the recording on from that frame.
        core.resume_replay_playback().expect("resume");
        let before = played_log.lock().unwrap().len();
        for _ in SEEK_TO..FRAMES {
            run_one_frame(&mut core, &clock, PERIOD_MICROS);
        }
        let played = played_log.lock().unwrap().clone();
        assert_eq!(&played[before..], &recorded[SEEK_TO as usize..]);
    }

    /// While stopped, the things playback takes away from the user (resets, save states, RAM
    /// writes) are theirs again, and detaching clears the stopped state.
    #[test]
    fn stopped_replay_allows_reset_and_save_states_and_detach_clears_it() {
        let (bytes, _) = record_alternating_replay(8);
        let (mut core, clock, _) = playback_core(&bytes);
        run_one_frame(&mut core, &clock, PERIOD_MICROS);

        // Playing back: refused.
        let state = core.create_save_state();
        core.hard_reset();
        assert_eq!(core.create_save_state(), state, "a reset is ignored during playback");
        assert!(!core.enqueue_write(0, ByteVec::from(&[1u8][..])), "writes are dropped during playback");

        assert!(core.stop_replay_playback());
        core.hard_reset();
        assert_eq!(core.create_save_state(), 0u32.to_le_bytes().to_vec(), "a reset works while stopped");
        core.load_save_state(&state);
        assert_eq!(core.create_save_state(), state, "loading a save state works while stopped");
        assert!(core.enqueue_write(0, ByteVec::from(&[1u8][..])), "writes are accepted while stopped");

        core.detach_replay_player();
        assert!(!core.has_replay_attached());
        assert!(!core.is_replay_playback_stopped());
        assert!(!core.is_playing_back());
        assert_eq!(core.replay_position(), (0, 0.into()));
    }
}
