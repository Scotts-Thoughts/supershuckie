use crate::emulator::{EmulatorCore, GbPaletteOverride, Input, MemoryRegionInfo, PartialReplayRecordMetadata, ScreenData};
use crate::link::{LinkFailure, LinkInbox, LinkPublisherFns, LinkRunOutcome, LinkSettings};
use crate::live_replay::{FollowerStats, FollowerStatsSnapshot, LiveReplaySource};
use crate::memory_monitor::{MemoryMonitorLocal, MemoryMonitorShared};
use crate::stream::{SnapshotRequestReason, StreamPublisherFns};
use crate::export::{ExportRange, ScreenLayout, VideoExportError, VideoFrameSink};
use crate::{std_timestamp_provider, AudioOutput, BookmarkAnchor, BookmarkAnchorError, ReplayPlayerAttachError, Speed};
use crate::{SuperShuckieCore, SuperShuckieRapidFire};
use spin::RwLock;
use std::borrow::{Cow, ToOwned};
use std::boxed::Box;
use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::num::NonZeroU64;
use std::format;
use std::fs::File;
use std::string::{String, ToString};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, SendError, Sender, TryRecvError};
use std::sync::{Arc, Mutex, TryLockError, Weak};
use std::time::{Duration, Instant};
use std::vec::Vec;
use supershuckie_pokeabyte_integration::PokeAByteEmulatorCommand;
#[cfg(feature = "pokeabyte")]
use supershuckie_pokeabyte_integration::PokeAByteIntegrationServer;
use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::replay_file::record::{ReplayFileWriteError, ReplayResumeError, ResumeCropPolicy};
use supershuckie_replay_recorder::replay_file::{ReplayConsoleType, ReplayFileMetadata, ReplayHeaderBlake3Hash};
use supershuckie_replay_recorder::{BookmarkTable, ByteVec, InputBuffer, SignedInteger, TimestampMillis, UnsignedInteger};

/// What a core thread is for, which decides how it competes for the CPU.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum CoreThreadRole {
    /// The player's own game: the thread the user is waiting on (raised priority, exempt from
    /// power throttling on Windows).
    #[default]
    Primary,

    /// Another player's game being followed (Play Together): may lag a little, must never take
    /// the CPU from the primary thread (lowered priority).
    Follower
}

/// The core thread has exited (its command channel is disconnected, or a reply channel was
/// dropped without an answer). No further command reaches it; reload the ROM to get a working
/// core again. See [`ThreadedSuperShuckieCore::is_alive`].
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct CoreThreadDead;

impl Display for CoreThreadDead {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str("The emulator thread has stopped; reload the ROM.")
    }
}

impl std::error::Error for CoreThreadDead {}

/// A (mostly) non-blocking, threaded wrapper for [`SuperShuckieCore`].
pub struct ThreadedSuperShuckieCore {
    screens: Arc<Mutex<Vec<ScreenData>>>,
    sender: Sender<ThreadCommand>,
    receiver_close: Receiver<()>,

    /// Whether the core thread is (as far as this wrapper knows) still alive. Set to `false` the
    /// first time a send or a reply receive finds the channel disconnected; every method that
    /// talks to the thread checks/updates it instead of panicking. See [`Self::is_alive`].
    alive: AtomicBool,

    desired_replay_frame: Arc<AtomicU32>,
    delta_replay_frames: Arc<AtomicI32>,
    elapsed_time: Arc<RwLock<ElapsedTimeStats>>,
    frame_times: Arc<RwLock<FrameTimeStats>>,
    emulated_frames: Arc<AtomicU64>,
    playback_paused: Arc<AtomicBool>,
    replay_stalled: Arc<AtomicBool>,

    /// Whether a replay is attached (playing or stopped).
    playback: bool,
    /// Whether the attached replay is stopped (see [`Self::stop_replay_playback`]).
    playback_stopped: bool,
    playback_total_frames: UnsignedInteger,
    playback_total_milliseconds: TimestampMillis,
    replay_errors: Arc<Mutex<Vec<ReplayFileWriteError>>>,
    replay_counters: Arc<Mutex<BTreeMap<String, SignedInteger>>>,

    /// Errors from the atomics-driven seek path (`go_to_replay_frame`/`advance_playback_frames`
    /// are fire-and-forget, so a failed seek has nowhere else to report to) and from unstalling on
    /// `start()`; see [`Self::take_playback_errors`].
    playback_errors: Arc<Mutex<Vec<String>>>,

    /// The cancel flag of the currently running video export, if any, so [`Drop`] can abort it
    /// instead of the thread running the export to completion (or hanging) with nobody left to
    /// read the result.
    current_export_cancel: Mutex<Option<Arc<AtomicBool>>>,

    /// The core thread, to wake it early from a paused wait.
    thread: Option<std::thread::Thread>,

    /// The followed game's counters while following (see [`Self::attach_live_replay_source`]).
    follower_stats: Arc<Mutex<Option<Arc<FollowerStats>>>>,

    /// Problems the stream publisher reported, until [`Self::get_stream_errors`] takes them.
    /// Shared with the thread (like `replay_errors`) so that polling them from the UI never
    /// waits on the core thread: the UI polls every tick, and a round trip would sit through the
    /// core's pacing sleep (up to a frame) every time, jittering the player's own display.
    stream_errors: Arc<Mutex<Vec<String>>>,

    /// Problems following reported, until [`Self::get_follower_errors`] takes them; same reason
    /// as `stream_errors`, and a follower thread runs below normal priority on top of that.
    follower_errors: Arc<Mutex<Vec<String>>>,

    /// Where the core stands with a link cable (see [`Self::link_status`]); written by the thread,
    /// read from the UI tick without a round trip.
    link_status: Arc<Mutex<LinkStatus>>,

    /// Problems the link publisher reported, until [`Self::get_link_errors`] takes them.
    link_errors: Arc<Mutex<Vec<String>>>,

    /// Facts about the wrapped core that never change for its life.
    memory_regions: Vec<MemoryRegionInfo>,
    console_type: Option<ReplayConsoleType>,
    rom_checksum: ReplayHeaderBlake3Hash,
    core_name: String
}

/// Where a core stands with a link cable (see `crate::link`).
#[derive(Clone, Debug, PartialEq)]
pub enum LinkStatus {
    /// No cable.
    Idle,

    /// Held at a frame boundary for a link handshake.
    Holding,

    /// Plugging in: the partner's follower is being brought to the agreed frame.
    Starting,

    /// Linked and running.
    Linked {
        /// Frames the inputs are sent ahead.
        delay_frames: u64,
        /// Link frames completed by the local console.
        link_frame: u64,
        /// Whether the pair is waiting for the partner's next link frame.
        stalled: bool
    },

    /// The link ended on its own; the lent core is back with its thread. Cleared by
    /// [`ThreadedSuperShuckieCore::unlink`].
    Failed(LinkFailure)
}

/// A follower's core loop, taken off its own thread to be run in lockstep with the player's own
/// core (see [`ThreadedSuperShuckieCore::lend`]). Hand it to [`ThreadedSuperShuckieCore::link`]
/// on the player's own core; dropping it instead ends the follower's thread.
pub struct LentCore {
    loop_: Box<CoreLoop>,
    /// Commands the follower's wrapper sends while its loop is lent, forwarded by its thread.
    commands: Receiver<ThreadCommand>,
    /// Where the loop goes back to.
    return_to: Sender<ReturnedCore>
}

/// A lent loop coming back to its own thread.
struct ReturnedCore {
    loop_: Box<CoreLoop>,
    /// The wrapper closed while the loop was lent: finish the thread once it is back.
    then_close: bool
}

/// Current elapsed time, retrieved atomically (the frame count corresponds to milliseconds and vice versa).
#[derive(Copy, Clone, Debug, Default)]
#[expect(missing_docs)]
pub struct ElapsedTimeStats {
    pub milliseconds: u32,
    pub frames: u32,
    pub speed: Speed,

    /// The attached replay's position: the frame being played back (`frames`), or, while the
    /// replay is stopped, the frame playback resumes from while `frames` keeps counting the live
    /// play (see `SuperShuckieCore::replay_position`). 0 without a replay.
    pub replay_frame: u32,

    /// Incremented every time a newly drawn frame is published to `read_screens`. Frames that
    /// were emulated but not drawn (fast-forward) do not change it, so compare this rather than
    /// `frames` to decide whether the screens need re-reading.
    pub screen_generation: u32
}

/// How long emulated frames are taking on the core thread, for diagnostics.
///
/// Times cover the core's own work for one frame (emulation plus compositing), not the pacing
/// wait. A frame is "over budget" when it took longer than the period the current speed allows.
#[derive(Copy, Clone, Debug, Default)]
pub struct FrameTimeStats {
    /// Duration of the most recent frame, in microseconds.
    pub last_frame_micros: u32,
    /// Exponential moving average over roughly the last 64 frames, in microseconds.
    pub average_frame_micros: u32,
    /// Longest frame seen since the stats were last reset (speed change, ROM load).
    pub max_frame_micros: u32,
    /// Time one frame may take at the current speed, in microseconds (0 when the core does not
    /// pace itself).
    pub budget_micros: u32,
    /// Frames that took longer than `budget_micros` since the stats were last reset.
    pub frames_over_budget: u64,
    /// Frames measured since the stats were last reset.
    pub frames_measured: u64
}

impl FrameTimeStats {
    fn record(&mut self, elapsed: Duration, budget: Option<u64>) {
        let micros = elapsed.as_micros().min(u32::MAX as u128) as u32;
        self.last_frame_micros = micros;
        self.average_frame_micros = if self.frames_measured == 0 {
            micros
        }
        else {
            // EMA with alpha = 1/64
            (self.average_frame_micros as u64 * 63 + micros as u64).div_ceil(64) as u32
        };
        self.max_frame_micros = self.max_frame_micros.max(micros);
        self.budget_micros = budget.unwrap_or(0).min(u32::MAX as u64) as u32;
        if budget.is_some_and(|b| micros as u64 > b) {
            self.frames_over_budget += 1;
        }
        self.frames_measured += 1;
    }
}

impl ThreadedSuperShuckieCore {
    /// Wrap the given `core` as the player's own game (see [`CoreThreadRole::Primary`]).
    pub fn new(emulator_core: Box<dyn EmulatorCore>) -> Self {
        Self::new_with_role(emulator_core, CoreThreadRole::Primary)
    }

    /// Wrap the given `core`, with its thread scheduled for `role`.
    pub fn new_with_role(emulator_core: Box<dyn EmulatorCore>, role: CoreThreadRole) -> Self {
        let screens = Arc::new(Mutex::new(emulator_core.get_screens().to_vec()));
        let memory_regions = emulator_core.memory_regions().to_vec();
        let console_type = emulator_core.replay_console_type();
        let rom_checksum = *emulator_core.rom_checksum();
        let core_name = emulator_core.core_name().to_owned();
        let (sender, receiver) = channel();
        let (sender_close, receiver_close) = channel();

        let playback_total_frames = 0;
        let playback_total_milliseconds = TimestampMillis(0);
        let desired_replay_frame = Arc::new(AtomicU32::new(u32::MAX));
        let delta_replay_frames = Arc::new(AtomicI32::new(0));
        let replay_errors = Arc::new(Mutex::new(Vec::new()));
        let replay_counters = Arc::new(Mutex::new(BTreeMap::new()));
        let playback_paused = Arc::new(AtomicBool::new(false));
        let replay_stalled = Arc::new(AtomicBool::new(false));
        let playback_errors = Arc::new(Mutex::new(Vec::new()));
        let follower_stats: Arc<Mutex<Option<Arc<FollowerStats>>>> = Arc::new(Mutex::new(None));
        let stream_errors = Arc::new(Mutex::new(Vec::new()));
        let follower_errors = Arc::new(Mutex::new(Vec::new()));
        let link_status = Arc::new(Mutex::new(LinkStatus::Idle));
        let link_errors = Arc::new(Mutex::new(Vec::new()));

        let elapsed_time = Arc::new(RwLock::new(ElapsedTimeStats::default()));
        let frame_times = Arc::new(RwLock::new(FrameTimeStats::default()));
        let emulated_frames = Arc::new(AtomicU64::new(0));

        let thread = {
            let elapsed_time = elapsed_time.clone();
            let frame_times = frame_times.clone();
            let emulated_frames = emulated_frames.clone();
            let screens = Arc::downgrade(&screens);
            let desired_replay_frame = desired_replay_frame.clone();
            let delta_replay_frames = delta_replay_frames.clone();
            let replay_errors = replay_errors.clone();
            let replay_counters = replay_counters.clone();
            let playback_paused = playback_paused.clone();
            let replay_stalled = replay_stalled.clone();
            let playback_errors = playback_errors.clone();
            let follower_stats_thread = follower_stats.clone();
            let stream_errors = stream_errors.clone();
            let follower_errors = follower_errors.clone();
            let link_status = link_status.clone();
            let link_errors = link_errors.clone();
            let name = match role {
                CoreThreadRole::Primary => "ThreadedSuperShuckieCore",
                CoreThreadRole::Follower => "ThreadedSuperShuckieCore/follower"
            };
            std::thread::Builder::new().name(name.to_owned()).spawn(move || {
                mark_thread_role(role);
                let loop_ = CoreLoop {
                    screens,
                    is_null: emulator_core.is_null(),
                    screens_queued: emulator_core.get_screens().to_vec(),
                    screen_ready_for_copy: false,
                    screen_generation: 0,
                    published_run_serial: 0,
                    core: SuperShuckieCore::new(emulator_core, std_timestamp_provider()),
                    pokeabyte_integration: None,
                    desired_replay_frame,
                    elapsed_time,
                    frame_times,
                    emulated_frames,
                    delta_replay_frames,
                    replay_errors,
                    replay_counters,
                    replay_stalled,
                    playback_errors,
                    playback_frozen: false,
                    coarse_seek_while_frozen: true,
                    pending_exact_frame: None,
                    freezes: BTreeMap::new(),
                    last_pokeabyte_freeze: None,
                    last_pokeabyte_read: None,
                    memory_monitor: None,
                    follower: None,
                    follower_stats: follower_stats_thread,
                    stream_errors,
                    follower_errors,
                    link_errors,
                    playback_paused
                };
                CoreThread {
                    receiver,
                    sender_close,
                    loop_: Some(Box::new(loop_)),
                    linked: None,
                    link_status,
                    pending_command: None
                }.run_thread();
            }).ok().map(|handle| handle.thread().clone())
        };

        Self {
            sender,
            screens,
            receiver_close,
            alive: AtomicBool::new(true),
            playback_total_frames,
            playback_total_milliseconds,
            replay_errors,
            elapsed_time,
            frame_times,
            emulated_frames,
            replay_counters,
            playback_errors,
            current_export_cancel: Mutex::new(None),
            playback: false,
            playback_stopped: false,
            desired_replay_frame,
            delta_replay_frames,
            playback_paused,
            replay_stalled,
            thread,
            follower_stats,
            stream_errors,
            follower_errors,
            link_status,
            link_errors,
            memory_regions,
            console_type,
            rom_checksum,
            core_name
        }
    }

    /// The wrapped core's memory regions (see [`EmulatorCore::memory_regions`]).
    #[inline]
    pub fn memory_regions(&self) -> &[MemoryRegionInfo] {
        &self.memory_regions
    }

    /// The wrapped core's console type.
    #[inline]
    pub fn console_type(&self) -> Option<ReplayConsoleType> {
        self.console_type
    }

    /// The wrapped core's ROM checksum.
    #[inline]
    pub fn rom_checksum(&self) -> &ReplayHeaderBlake3Hash {
        &self.rom_checksum
    }

    /// The wrapped core's name, including its version (see `EmulatorCore::core_name`).
    #[inline]
    pub fn core_name(&self) -> &str {
        &self.core_name
    }

    /// Whether the core thread is still running, as far as this wrapper has observed. Once this
    /// is `false` it never becomes `true` again; every method that talks to the thread degrades
    /// gracefully instead of panicking (see [`CoreThreadDead`]).
    #[inline]
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// Send `c` to the core thread without waiting for a reply. `Err(CoreThreadDead)` (and
    /// [`Self::is_alive`] flipping to `false`) means the thread has exited; `c` is dropped
    /// unsent.
    fn send(&self, c: ThreadCommand) -> Result<(), CoreThreadDead> {
        if self.sender.send(c).is_err() {
            self.alive.store(false, Ordering::Relaxed);
            return Err(CoreThreadDead)
        }
        Ok(())
    }

    /// Send a command built from a fresh reply channel (via `make`) and block for the answer.
    /// `Err(CoreThreadDead)` if the thread has exited, either before the command could be sent or
    /// while it was being processed (the reply channel closes when the thread's copy of the
    /// sender half is dropped without ever calling `send`).
    fn call<T>(&self, make: impl FnOnce(Sender<T>) -> ThreadCommand) -> Result<T, CoreThreadDead> {
        let (sender, receiver) = channel();
        self.send(make(sender))?;
        receiver.recv().map_err(|_| {
            self.alive.store(false, Ordering::Relaxed);
            CoreThreadDead
        })
    }

    /// Take (clearing) the errors accumulated from the atomics-driven seek path
    /// (`go_to_replay_frame`/`advance_playback_frames`) and from unstalling playback on
    /// [`Self::start`]. Those commands are fire-and-forget, so this is the only way to observe a
    /// failed seek.
    pub fn take_playback_errors(&self) -> Vec<String> {
        let mut errors = self.playback_errors.lock().unwrap_or_else(|p| p.into_inner());
        core::mem::take(&mut *errors)
    }

    /// Attach (or with `None`, detach) the RAM tools' memory monitor. The core thread services it
    /// between frames (see [`crate::memory_monitor`]).
    pub fn set_memory_monitor(&self, monitor: Option<Arc<MemoryMonitorShared>>) {
        let _ = self.send(ThreadCommand::SetMemoryMonitor(monitor));
        self.wake();
    }

    /// Wake the core thread if it is waiting while paused, so that a change made through shared
    /// state (such as the memory monitor's request) is picked up right away.
    #[inline]
    pub fn wake(&self) {
        if let Some(thread) = self.thread.as_ref() {
            thread.unpark();
        }
    }

    /// Get the elapsed time.
    pub fn get_elapsed_time(&self) -> ElapsedTimeStats {
        self.elapsed_time.read().to_owned()
    }

    /// Frame-time diagnostics for the core thread.
    pub fn get_frame_time_stats(&self) -> FrameTimeStats {
        self.frame_times.read().to_owned()
    }

    /// Total frames emulated by this core since it was created (drawn or not, any replay state).
    /// Compare successive readings to get the true emulation rate.
    pub fn get_emulated_frame_count(&self) -> u64 {
        self.emulated_frames.load(Ordering::Relaxed)
    }

    /// Read the screens.
    ///
    /// Note that while this function is running, the screen buffer will be blocked from being
    /// updated and may not be immediately updated until later.
    pub fn read_screens<T, F: FnOnce(&[ScreenData]) -> T>(&self, reader: F) -> T {
        let lock = self.screens.lock().unwrap_or_else(|p| p.into_inner());
        reader(lock.as_slice())
    }

    /// Start running continuously.
    ///
    /// NOTE: This is blocking (as long as the core thread is alive; returns immediately if it is
    /// not).
    pub fn start(&self) {
        if !self.playback_paused.load(Ordering::Relaxed) {
            return
        }

        let (sender, receiver) = channel();
        if self.send(ThreadCommand::Start(sender)).is_ok() {
            // A paused thread parks for up to 100 ms between command checks.
            self.wake();
            let _ = receiver.recv();
        }
    }

    /// Pause running.
    ///
    /// NOTE: This is blocking (as long as the core thread is alive; returns immediately if it is
    /// not).
    pub fn pause(&self) {
        if self.playback_paused.load(Ordering::Relaxed) {
            return
        }

        let (sender, receiver) = channel();
        if self.send(ThreadCommand::Pause(sender)).is_ok() {
            let _ = receiver.recv();
        }
    }

    /// Block until this command is reached.
    pub fn rendezvous(&self) {
        let (sender, receiver) = channel();
        if self.send(ThreadCommand::Rendezvous(sender)).is_ok() {
            let _ = receiver.recv();
        }
    }

    /// Pause running temporarily.
    pub fn set_playback_frozen(&self, paused: bool) {
        let _ = self.send(ThreadCommand::SetPlaybackFrozen(paused));
    }

    /// Attach a Poke-A-Byte integration server on UDP `port` (127.0.0.1), or detach it with
    /// `None`. Already listening on that port is a no-op; a different port replaces the server.
    pub fn set_pokeabyte_port(&self, port: Option<u16>) -> Result<(), String> {
        match self.call(|sender| ThreadCommand::SetPokeABytePort(port, sender)) {
            Ok(r) => r,
            Err(dead) => Err(dead.to_string()),
        }
    }

    /// Start recording a replay.
    pub fn start_recording_replay(&self, metadata: PartialReplayRecordMetadata<std::io::BufWriter<File>, std::io::BufWriter<File>>) -> Result<(), ReplayFileWriteError> {
        match self.call(|reply| ThreadCommand::StartRecordingReplay(metadata, reply)) {
            Ok(r) => r,
            Err(dead) => Err(ReplayFileWriteError::Other { explanation: Cow::Owned(dead.to_string()) }),
        }
    }

    /// Resume recording from an existing replay.
    ///
    /// The source replay must already be attached for playback (e.g. via `attach_replay_player`);
    /// the core thread consumes that attached player to build the new file's prefix.
    /// `resume_at_frame == None` resumes from the final frame.
    ///
    /// The wrapper's cached playback state (see [`Self::is_playing_back`]) is only cleared on
    /// success; a failed resume leaves the source player attached and playing back on the core
    /// thread (see `SuperShuckieCore::resume_recording_replay`), so the wrapper's view of it stays
    /// accurate too.
    pub fn resume_recording_replay(
        &mut self,
        resume_at_frame: Option<UnsignedInteger>,
        metadata: PartialReplayRecordMetadata<std::io::BufWriter<File>, std::io::BufWriter<File>>,
        crop_policy: ResumeCropPolicy,
        bookmarks: Option<BookmarkTable>,
    ) -> Result<(), ReplayResumeError> {
        let result = match self.call(|reply| ThreadCommand::ResumeRecordingReplay {
            resume_at_frame,
            metadata,
            crop_policy,
            bookmarks,
            reply,
        }) {
            Ok(r) => r,
            Err(dead) => Err(ReplayResumeError::BadSource { explanation: Cow::Owned(dead.to_string()) }),
        };

        if result.is_ok() {
            // Resuming transitions us out of playback and into live recording, so clear the
            // wrapper's playback state (mirrors detach).
            self.playback_total_frames = 0;
            self.playback_total_milliseconds = 0.into();
            self.playback = false;
            self.playback_stopped = false;
        }

        result
    }

    /// Stop recording replay.
    pub fn stop_recording_replay(&self) -> bool {
        self.call(|sender| ThreadCommand::StopRecordingReplay(sender)).unwrap_or(false)
    }

    /// Begin a blocking video export on the core thread.
    ///
    /// A replay must already be attached for playback (the export reuses the attached player).
    /// While the export runs, normal playback/stepping on the core thread is paused. Returns a
    /// [`VideoExportHandle`] to poll progress, cancel, and retrieve the result.
    ///
    /// If the core thread has already exited, the returned handle observes that immediately (see
    /// [`VideoExportHandle::poll_done`]).
    pub fn export_replay(
        &self,
        sink: Box<dyn VideoFrameSink>,
        range: ExportRange,
        layout: ScreenLayout,
    ) -> VideoExportHandle {
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(RwLock::new((0u64, 0u64)));
        let (done_sender, done_receiver) = channel();

        // So `Drop` can abort this export instead of waiting for it (or the whole thread) to run
        // to completion with nobody left to read the result.
        *self.current_export_cancel.lock().unwrap_or_else(|p| p.into_inner()) = Some(cancel.clone());

        let _ = self.send(ThreadCommand::ExportVideo {
            sink,
            range,
            layout,
            cancel: cancel.clone(),
            progress: progress.clone(),
            done: done_sender,
        });

        VideoExportHandle {
            cancel,
            progress,
            done: done_receiver,
        }
    }

    /// Enqueue an input.
    pub fn enqueue_input(&self, input: Input) {
        let _ = self.send(ThreadCommand::EnqueueInput(input));
    }

    /// Set the speed.
    pub fn set_speed(&self, speed: Speed) {
        let _ = self.send(ThreadCommand::SetSpeed(speed));
    }

    /// Reset the frame-time diagnostics (max, over-budget and measured counts).
    pub fn reset_frame_time_stats(&self) {
        *self.frame_times.write() = FrameTimeStats::default();
    }

    /// Set the speed.
    pub fn hard_reset(&self) {
        let _ = self.send(ThreadCommand::HardReset);
    }

    /// Set the rapid fire input.
    pub fn set_rapid_fire_input(&self, input: Option<SuperShuckieRapidFire>) {
        let _ = self.send(ThreadCommand::SetRapidFireInput(input));
    }

    /// Set the toggle input.
    pub fn set_toggled_input(&self, input: Option<Input>) {
        let _ = self.send(ThreadCommand::SetToggledInput(input));
    }

    /// Hold `input`'s buttons for exactly the next `frames` emulated frames (see
    /// [`SuperShuckieCore::press_for_frames`]).
    pub fn press_for_frames(&self, input: Input, frames: NonZeroU64) {
        let _ = self.send(ThreadCommand::PressForFrames(input, frames));
    }

    /// Create a save state.
    ///
    /// Returns `None` if no save state could be created for some unknown reason (including the
    /// core thread having exited).
    ///
    /// NOTE: This is blocking.
    pub fn create_save_state(&self) -> Option<Vec<u8>> {
        self.call(|sender| ThreadCommand::CreateSaveState(sender)).ok()
    }

    /// Load a save state.
    pub fn load_save_state(&self, state: Vec<u8>) {
        if self.send(ThreadCommand::LoadSaveState(state)).is_ok() {
            // A paused thread parks for up to 100 ms between command checks.
            self.wake();
        }
    }

    /// Get SRAM.
    ///
    /// Returns `None` if SRAM could not be read for some unknown reason (including the core
    /// thread having exited).
    ///
    /// NOTE: This is blocking.
    pub fn get_sram(&self) -> Option<Vec<u8>> {
        self.call(|sender| ThreadCommand::SaveSRAM(sender)).ok()
    }

    /// Get whether or not a replay is being played back (attached and not stopped).
    #[inline]
    pub fn is_playing_back(&self) -> bool {
        self.playback && !self.playback_stopped
    }

    /// Get whether a replay is attached, playing or stopped.
    #[inline]
    pub fn has_replay_attached(&self) -> bool {
        self.playback
    }

    /// Get whether the attached replay is stopped (see [`Self::stop_replay_playback`]).
    #[inline]
    pub fn is_replay_playback_stopped(&self) -> bool {
        self.playback && self.playback_stopped
    }

    /// Stop the attached replay from driving the emulator without detaching it; the game runs
    /// on live under the user's input from the current frame, and the replay can still be seeked
    /// in ([`Self::go_to_replay_frame`]) and resumed ([`Self::resume_replay_playback`]). See
    /// `SuperShuckieCore::stop_replay_playback`.
    ///
    /// NOTE: This is blocking (as long as the core thread is alive).
    pub fn stop_replay_playback(&mut self) {
        if !self.is_playing_back() {
            return
        }
        let _ = self.call(|reply| ThreadCommand::StopReplayPlayback(reply));
        self.playback_stopped = true;
    }

    /// Resume playing back a stopped replay from where it was stopped or last seeked to (see
    /// `SuperShuckieCore::resume_replay_playback`). Does nothing unless stopped.
    ///
    /// NOTE: This is blocking (as long as the core thread is alive): the emulator has to be put
    /// back at the resume point first.
    pub fn resume_replay_playback(&mut self) -> Result<(), String> {
        if !self.is_replay_playback_stopped() {
            return Ok(())
        }
        // Playing back again either way: a failed seek leaves the core stalled in the replay,
        // which is how any other failed seek ends up too.
        self.playback_stopped = false;
        match self.call(|reply| ThreadCommand::ResumeReplayPlayback(reply)) {
            Ok(r) => r,
            Err(dead) => Err(dead.to_string()),
        }
    }

    /// Put a stopped replay's emulator back at its resume point without resuming playback (see
    /// `SuperShuckieCore::go_to_replay_resume_point`). Does nothing unless stopped. Like
    /// [`Self::go_to_replay_frame`] this is fire-and-forget; a failure surfaces through
    /// [`Self::take_playback_errors`].
    pub fn go_to_replay_resume_point(&self) {
        if !self.is_replay_playback_stopped() {
            return
        }
        let _ = self.send(ThreadCommand::GoToReplayResumePoint);
    }

    /// Get the total number of frames in the current playback.
    #[inline]
    pub fn get_playback_total_frames(&self) -> u32 {
        self.playback_total_frames as u32
    }

    /// Get the total number of frames in the current playback.
    #[inline]
    pub fn get_playback_total_milliseconds(&self) -> u32 {
        self.playback_total_milliseconds.0 as u32
    }

    /// Load the replay.
    ///
    /// On `Err(ReplayPlayerAttachError::Failed { .. })` the core detached itself (see
    /// `SuperShuckieCore::attach_replay_player`), so the wrapper's cached playback state is
    /// cleared to match. On any other error (including a dead core thread, which maps to
    /// `Failed`) or on success, the wrapper's playback state is updated/left as documented below.
    pub fn attach_replay_player(&mut self, mut player: ReplayFilePlayer, allow_mismatch: bool) -> Result<(), ReplayPlayerAttachError> {
        player.enable_threading();

        let total_milliseconds = player.get_total_milliseconds();
        let total_frames = player.get_total_frames();

        let result = match self.call(|reply| ThreadCommand::AttachReplayPlayer {
            player,
            allow_mismatched: allow_mismatch,
            reply
        }) {
            Ok(r) => r,
            Err(dead) => Err(ReplayPlayerAttachError::Failed { description: dead.to_string() }),
        };

        match &result {
            Ok(()) => {
                self.playback_total_frames = total_frames;
                self.playback_total_milliseconds = total_milliseconds;
                self.playback = true;
                self.playback_stopped = false;
            }
            Err(ReplayPlayerAttachError::Failed { .. }) => {
                self.playback_total_frames = 0;
                self.playback_total_milliseconds = 0.into();
                self.playback = false;
                self.playback_stopped = false;
            }
            // Incompatible / MismatchedMetadata: the core rejected the attach before touching
            // anything, so whatever was already attached (or not) is unchanged.
            Err(_) => {}
        }

        result
    }

    /// Detach a replay
    pub fn detach_replay_player(&mut self) {
        self.playback_total_frames = 0;
        self.playback_total_milliseconds = 0.into();
        self.playback = false;
        self.playback_stopped = false;
        let _ = self.send(ThreadCommand::DetachReplayPlayer);
    }

    /// Go to the desired frame.
    #[inline]
    pub fn go_to_replay_frame(&self, frame: u32) {
        // we use an AtomicU32 instead of just directly going to a frame
        // because we do not want to clog the queue with goto requests
        self.desired_replay_frame.store(frame, Ordering::Relaxed);
    }

    /// Advance or go back some frames.
    #[inline]
    pub fn advance_playback_frames(&self, amount: i32) {
        // similarly use AtomicI32 to avoid clogging the queue
        self.delta_replay_frames.store(amount, Ordering::Relaxed);
    }

    /// Get any replay recording errors.
    pub fn get_replay_recording_errors(&mut self) -> Vec<ReplayFileWriteError> {
        let mut errors = self.replay_errors.lock().unwrap_or_else(|p| p.into_inner());
        core::mem::take(&mut *errors)
    }

    /// Mark the start of the replay. `Err` when no replay is being recorded (or the thread has
    /// died).
    pub fn mark_start(&mut self, timer_offset: TimestampMillis) -> Result<(UnsignedInteger, TimestampMillis), ()> {
        self.call(|sender| ThreadCommand::MarkReplayStart(sender, timer_offset)).ok().flatten().ok_or(())
    }

    /// Mark the end of the replay. `Err` when no replay is being recorded (or the thread has
    /// died).
    pub fn mark_end(&mut self) -> Result<(UnsignedInteger, TimestampMillis), ()> {
        self.call(|sender| ThreadCommand::MarkReplayEnd(sender)).ok().flatten().ok_or(())
    }

    /// Longest a caller waits for the core thread to place a bookmark.
    const BOOKMARK_TIMEOUT: Duration = Duration::from_millis(500);

    /// Place a bookmark at the current moment (see [`SuperShuckieCore::bookmark_anchor`]).
    ///
    /// NOTE: This is blocking (for at most half a second).
    pub fn bookmark_anchor(&self, keyframe: bool) -> Result<BookmarkAnchor, BookmarkAnchorError> {
        let (sender, receiver) = channel();
        // The core thread must not write a bookmark (or the keyframe a keyframe bookmark forces)
        // after we have given up waiting for it; hand it the deadline so it can skip a late one.
        let deadline = Instant::now() + Self::BOOKMARK_TIMEOUT;
        if self.send(ThreadCommand::BookmarkAnchor(sender, keyframe, deadline)).is_err() {
            return Err(BookmarkAnchorError::Busy)
        }
        self.wake();
        receiver.recv_timeout(Self::BOOKMARK_TIMEOUT).unwrap_or(Err(BookmarkAnchorError::Busy))
    }

    /// Estimate the replay time at `frame` (see [`SuperShuckieCore::estimate_millis_at`]).
    ///
    /// NOTE: This is blocking (for at most half a second).
    pub fn estimate_millis_at(&self, frame: UnsignedInteger) -> Result<Option<TimestampMillis>, BookmarkAnchorError> {
        let (sender, receiver) = channel();
        if self.send(ThreadCommand::EstimateMillisAt(sender, frame)).is_err() {
            return Err(BookmarkAnchorError::Busy)
        }
        self.wake();
        receiver.recv_timeout(Self::BOOKMARK_TIMEOUT).map_err(|_| BookmarkAnchorError::Busy)
    }

    /// Replace the bookmarks of the replay being recorded (see
    /// [`SuperShuckieCore::set_replay_bookmarks`]).
    pub fn set_replay_bookmarks(&self, table: BookmarkTable) {
        let _ = self.send(ThreadCommand::SetReplayBookmarks(table));
    }

    /// Get the counters.
    #[inline]
    pub fn get_replay_counters(&self) -> BTreeMap<String, SignedInteger> {
        self.replay_counters.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Add an amount to a counter.
    #[inline]
    pub fn change_replay_counter(&mut self, name: String, delta: SignedInteger) {
        let _ = self.send(ThreadCommand::ChangeReplayCounter { name, delta });
    }

    /// Set whether or not speed changes from replays are ignored.
    #[inline]
    pub fn set_ignore_speed_changes_in_replay(&self, ignored: bool) {
        let _ = self.send(ThreadCommand::IgnoreSpeedChangesInReplay(ignored));
    }

    /// Set whether or not to resync keyframes in replay playback.
    #[inline]
    pub fn set_auto_resync_keyframes_in_replay(&self, resync: bool) {
        let _ = self.send(ThreadCommand::AutoResyncKeyframesInReplay(resync));
    }

    /// Start mirroring the session into `publisher` (see [`SuperShuckieCore::start_stream_publishing`]).
    ///
    /// NOTE: This is blocking.
    pub fn start_stream_publishing(&self, publisher: Box<dyn StreamPublisherFns>) -> Result<(), String> {
        match self.call(|reply| ThreadCommand::StartStreamPublishing(publisher, reply)) {
            Ok(r) => r,
            Err(dead) => Err(dead.to_string())
        }
    }

    /// Stop publishing the session (see [`SuperShuckieCore::stop_stream_publishing`]).
    ///
    /// NOTE: This is blocking, so that the publisher's `end` has been sent when it returns.
    pub fn stop_stream_publishing(&self) {
        let _ = self.call(|reply| ThreadCommand::StopStreamPublishing(reply));
    }

    /// Publish a snapshot at the next frame boundary (see
    /// [`SuperShuckieCore::request_stream_snapshot`]); served right away while paused.
    pub fn request_stream_snapshot(&self) {
        let _ = self.send(ThreadCommand::RequestStreamSnapshot);
        self.wake();
    }

    /// Problems the stream publisher reported since the last call. Never waits on the core
    /// thread (safe to poll every UI tick).
    pub fn get_stream_errors(&self) -> Vec<String> {
        let mut errors = self.stream_errors.lock().unwrap_or_else(|p| p.into_inner());
        core::mem::take(&mut *errors)
    }

    /// Follow another player's game (see [`SuperShuckieCore::attach_live_replay_source`]). The
    /// thread paces itself on what arrives from then on (see [`CoreThreadRole::Follower`] for
    /// how to schedule it).
    ///
    /// NOTE: This is blocking.
    pub fn attach_live_replay_source(&mut self, source: LiveReplaySource, metadata: ReplayFileMetadata, allow_mismatched: bool) -> Result<(), ReplayPlayerAttachError> {
        let result = match self.call(|reply| ThreadCommand::AttachLiveSource { source, metadata, allow_mismatched, reply }) {
            Ok(r) => r,
            Err(dead) => Err(ReplayPlayerAttachError::Failed { description: dead.to_string() })
        };
        if result.is_ok() {
            self.playback_total_frames = 0;
            self.playback_total_milliseconds = 0.into();
            self.playback = false;
            self.playback_stopped = false;
        }
        result
    }

    /// Stop following (see [`SuperShuckieCore::detach_live_source`]).
    ///
    /// NOTE: This is blocking, so that the follower's file is closed when it returns.
    pub fn detach_live_replay_source(&self) {
        let _ = self.call(|reply| ThreadCommand::DetachLiveSource(reply));
    }

    /// The followed game's counters, if following (see [`FollowerStats`]).
    pub fn follower_stats(&self) -> Option<FollowerStatsSnapshot> {
        self.follower_stats.lock().unwrap_or_else(|p| p.into_inner()).as_ref().map(|s| s.snapshot())
    }

    /// Also write the followed game to a replay file (see
    /// [`SuperShuckieCore::start_recording_follower_replay`]).
    ///
    /// NOTE: This is blocking.
    pub fn start_recording_follower_replay(&self, metadata: PartialReplayRecordMetadata<std::io::BufWriter<File>, std::io::BufWriter<File>>, publisher: ReplayFileMetadata) -> Result<(), ReplayFileWriteError> {
        match self.call(|reply| ThreadCommand::StartRecordingFollowerReplay(metadata, publisher, reply)) {
            Ok(r) => r,
            Err(dead) => Err(ReplayFileWriteError::Other { explanation: Cow::Owned(dead.to_string()) })
        }
    }

    /// Problems following reported since the last call (a state that would not load). Never
    /// waits on the core thread (safe to poll every UI tick).
    pub fn get_follower_errors(&self) -> Vec<String> {
        let mut errors = self.follower_errors.lock().unwrap_or_else(|p| p.into_inner());
        core::mem::take(&mut *errors)
    }

    /// Set whether seeks requested while playback is frozen (a timeline drag) land on the nearest
    /// keyframe instead of the exact frame, which is then sought when the freeze ends. See
    /// [`SuperShuckieCore::coarse_replay_frame`]. On by default.
    #[inline]
    pub fn set_coarse_seek_while_frozen(&self, coarse: bool) {
        let _ = self.send(ThreadCommand::SetCoarseSeekWhileFrozen(coarse));
    }

    /// Route the audio of audible frames to `output` (`None` to stop).
    #[inline]
    pub fn set_audio_output(&self, output: Option<Arc<AudioOutput>>) {
        let _ = self.send(ThreadCommand::SetAudioOutput(output));
    }

    /// Turn audio rendering in the core on or off.
    #[inline]
    pub fn set_audio_enabled(&self, enabled: bool) {
        let _ = self.send(ThreadCommand::SetAudioEnabled(enabled));
    }

    /// Discard audio while the game runs at any speed other than 1x.
    #[inline]
    pub fn set_audio_mute_when_sped_up(&self, mute: bool) {
        let _ = self.send(ThreadCommand::SetAudioMuteWhenSpedUp(mute));
    }

    /// Draw a Game Boy game with `colors` instead of its own palettes, or with its own again
    /// (`None`); see `EmulatorCore::set_gb_palette_override`. Shows on the next frame drawn.
    #[inline]
    pub fn set_gb_palette_override(&self, colors: Option<GbPaletteOverride>) {
        let _ = self.send(ThreadCommand::SetGbPaletteOverride(colors));
    }

    /// The colors the game's own palettes draw with right now (see `EmulatorCore::gb_palettes`);
    /// `None` where there are none, or if the thread is gone.
    ///
    /// NOTE: This is blocking.
    pub fn gb_palettes(&self) -> Option<GbPaletteOverride> {
        let (sender, receiver) = channel();
        self.send(ThreadCommand::GetGbPalettes(sender)).ok()?;
        // A paused thread parks for up to 100 ms between command checks.
        self.wake();
        receiver.recv().ok()?
    }

    /// Read `len` bytes of console memory at `address` (see `EmulatorCore::read_ram`); `None`
    /// when unmapped or the thread is gone.
    ///
    /// NOTE: This is blocking.
    pub fn read_ram(&self, address: u32, len: usize) -> Option<Vec<u8>> {
        self.call(|reply| ThreadCommand::ReadRam { address, len, reply }).ok().flatten()
    }

    /// Write `data` at `address` between frames, recorded and published like any external write
    /// (see `SuperShuckieCore::enqueue_write`); dropped during replay playback.
    pub fn enqueue_write(&self, address: u32, data: Vec<u8>) {
        let _ = self.send(ThreadCommand::EnqueueWrite { address, data });
        self.wake();
    }

    /// Stop at the next frame boundary and stay there for a link handshake (see
    /// `SuperShuckieCore::link_hold`): the replay frame the console is held at and the input it
    /// holds.
    ///
    /// NOTE: This is blocking (at most one frame).
    pub fn link_hold(&self) -> Result<(u64, InputBuffer), String> {
        match self.call(|reply| ThreadCommand::LinkHold(reply)) {
            Ok(r) => r,
            Err(dead) => Err(dead.to_string())
        }
    }

    /// Abandon a hold without linking.
    ///
    /// NOTE: This is blocking.
    pub fn link_release(&self) {
        let _ = self.call(|reply| ThreadCommand::LinkRelease(reply));
        self.wake();
    }

    /// Take this (follower) core's loop off its thread, to run it in lockstep with the player's
    /// own core (see [`Self::link`]). This wrapper keeps working meanwhile: its commands are
    /// forwarded to whichever thread holds the loop, and its screens, counters and stats keep
    /// being updated from there. `Err` if it is already lent.
    ///
    /// NOTE: This is blocking.
    pub fn lend(&self) -> Result<LentCore, String> {
        match self.call(|reply| ThreadCommand::Lend(reply)) {
            Ok(r) => r,
            Err(dead) => Err(dead.to_string())
        }
    }

    /// Plug a link cable in between this (held) core and `partner`, the lent follower of the
    /// other player, once the follower has reached `settings.partner_start_frame` from what its
    /// feeder holds (up to about five seconds). Returns at once; the outcome shows up in
    /// [`Self::link_status`] (`Linked`, or `Failed`). The follower's stream feeder should be
    /// unsubscribed meanwhile: the pair is driven by the link frames pushed into `inbox`.
    pub fn link(&self, partner: LentCore, settings: LinkSettings, inbox: Arc<LinkInbox>, publisher: Box<dyn LinkPublisherFns>) {
        let _ = self.send(ThreadCommand::Link { lent: partner, settings, inbox, publisher });
        self.wake();
    }

    /// Pull the cable (or abandon a link being started, or acknowledge a failed one): the
    /// partner's loop goes back to its own thread, where it waits for a fresh snapshot.
    ///
    /// NOTE: This is blocking.
    pub fn unlink(&self) {
        let _ = self.call(|reply| ThreadCommand::Unlink(reply));
        self.wake();
    }

    /// Where the core stands with a link cable. Never waits on the core thread (safe to poll
    /// every UI tick).
    pub fn link_status(&self) -> LinkStatus {
        self.link_status.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Problems the link publisher reported since the last call. Never waits on the core
    /// thread.
    pub fn get_link_errors(&self) -> Vec<String> {
        let mut errors = self.link_errors.lock().unwrap_or_else(|p| p.into_inner());
        core::mem::take(&mut *errors)
    }

    /// Transfer the given Poke-A-Byte integration if it is compatible.
    ///
    /// NOTE: This is blocking.
    pub fn transfer_pokeabyte_integration(&self, to: &ThreadedSuperShuckieCore) -> bool {
        self.call(|sender| ThreadCommand::TransferPokeAByteIntegrationExternal(sender, to.sender.clone())).unwrap_or(false)
    }

    /// Get whether or not playback is currently paused.
    pub fn is_paused(&self) -> bool {
        self.playback_paused.load(Ordering::Relaxed)
    }

    /// Get whether or not replay playback is finished or stalled.
    pub fn is_replay_playback_finished(&self) -> bool {
        self.replay_stalled.load(Ordering::Relaxed)
    }
}

/// Handle to an in-progress video export started by [`ThreadedSuperShuckieCore::export_replay`].
pub struct VideoExportHandle {
    cancel: Arc<AtomicBool>,
    progress: Arc<RwLock<(u64, u64)>>,
    done: Receiver<Result<(), VideoExportError>>,
}

impl VideoExportHandle {
    /// Current progress as `(frames_done, frames_total)`. `frames_total` is 0 until the export
    /// loop starts (or for an empty range).
    pub fn progress(&self) -> (u64, u64) {
        *self.progress.read()
    }

    /// Request cancellation. The export loop aborts at the next frame boundary.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Non-blocking check for completion. `None` while still running.
    ///
    /// If the core thread closes (or crashes) before sending a result, this reports it as a sink
    /// error rather than leaving the export looking perpetually in progress.
    pub fn poll_done(&self) -> Option<Result<(), VideoExportError>> {
        match self.done.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(VideoExportError::Sink {
                explanation: std::borrow::Cow::Borrowed("the emulator thread closed before the export finished"),
            })),
        }
    }

    /// Block until the export completes and return its result.
    pub fn wait(self) -> Result<(), VideoExportError> {
        self.done.recv().unwrap_or(Err(VideoExportError::Sink {
            explanation: std::borrow::Cow::Borrowed("core thread closed before export finished"),
        }))
    }
}

impl Drop for ThreadedSuperShuckieCore {
    fn drop(&mut self) {
        // If a video export is running, the core thread will not even look at its command queue
        // (let alone `Close`) until the export's blocking loop returns; flip its cancel flag
        // first so it aborts at the next frame instead of running to completion (or hanging)
        // while dropping us blocks on `receiver_close.recv()` below.
        if let Some(cancel) = self.current_export_cancel.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
            cancel.store(true, Ordering::Relaxed);
        }

        // we couldn't really care less if these succeed or fail; we just want to ensure that
        // the replay file is closed, and it should be (if it didn't error)
        let _ = self.sender.send(ThreadCommand::Close);
        let _ = self.receiver_close.recv();
    }
}

// TODO: Option to run just a single frame? Maybe also skip around a replay file to a given
//       keyframe...
enum ThreadCommand {
    Start(Sender<()>),
    Pause(Sender<()>),
    SetPlaybackFrozen(bool),
    SetCoarseSeekWhileFrozen(bool),
    SetPokeABytePort(Option<u16>, Sender<Result<(), String>>),
    StartRecordingReplay(PartialReplayRecordMetadata<std::io::BufWriter<File>, std::io::BufWriter<File>>, Sender<Result<(), ReplayFileWriteError>>),
    ResumeRecordingReplay {
        resume_at_frame: Option<UnsignedInteger>,
        metadata: PartialReplayRecordMetadata<std::io::BufWriter<File>, std::io::BufWriter<File>>,
        crop_policy: ResumeCropPolicy,
        bookmarks: Option<BookmarkTable>,
        reply: Sender<Result<(), ReplayResumeError>>,
    },
    ExportVideo {
        sink: Box<dyn VideoFrameSink>,
        range: ExportRange,
        layout: ScreenLayout,
        cancel: Arc<AtomicBool>,
        progress: Arc<RwLock<(u64, u64)>>,
        done: Sender<Result<(), VideoExportError>>,
    },
    StopRecordingReplay(Sender<bool>),
    AttachReplayPlayer {
        player: ReplayFilePlayer,
        allow_mismatched: bool,
        reply: Sender<Result<(), ReplayPlayerAttachError>>
    },
    DetachReplayPlayer,
    StopReplayPlayback(Sender<()>),
    ResumeReplayPlayback(Sender<Result<(), String>>),
    GoToReplayResumePoint,
    EnqueueInput(Input),
    SetRapidFireInput(Option<SuperShuckieRapidFire>),
    SetToggledInput(Option<Input>),
    PressForFrames(Input, NonZeroU64),
    SetSpeed(Speed),
    HardReset,
    CreateSaveState(Sender<Vec<u8>>),
    LoadSaveState(Vec<u8>),
    SaveSRAM(Sender<Vec<u8>>),
    /// Answered with `None` when no replay is being recorded.
    MarkReplayStart(Sender<Option<(UnsignedInteger, TimestampMillis)>>, TimestampMillis),
    /// Answered with `None` when no replay is being recorded.
    MarkReplayEnd(Sender<Option<(UnsignedInteger, TimestampMillis)>>),
    /// `Instant` is the deadline the wrapper is willing to wait until; the handler skips placing
    /// the bookmark (and, for a keyframe bookmark, writing its keyframe) if it is reached, so a
    /// caller that gave up waiting never gets an orphan keyframe written later (see
    /// [`CoreLoop::handle_command`]).
    BookmarkAnchor(Sender<Result<BookmarkAnchor, BookmarkAnchorError>>, bool, Instant),
    EstimateMillisAt(Sender<Option<TimestampMillis>>, UnsignedInteger),
    SetReplayBookmarks(BookmarkTable),
    Close,
    ChangeReplayCounter { name: String, delta: SignedInteger },
    IgnoreSpeedChangesInReplay(bool),
    AutoResyncKeyframesInReplay(bool),
    SetAudioOutput(Option<Arc<AudioOutput>>),
    SetAudioEnabled(bool),
    SetAudioMuteWhenSpedUp(bool),
    SetGbPaletteOverride(Option<GbPaletteOverride>),
    GetGbPalettes(Sender<Option<GbPaletteOverride>>),
    TransferPokeAByteIntegrationExternal(Sender<bool>, Sender<ThreadCommand>),
    TransferPokeAByteIntegrationInternal(Sender<bool>, PokeAByteIntegrationServer, ReplayConsoleType, ReplayHeaderBlake3Hash),
    Rendezvous(Sender<()>),
    SetMemoryMonitor(Option<Arc<MemoryMonitorShared>>),
    StartStreamPublishing(Box<dyn StreamPublisherFns>, Sender<Result<(), String>>),
    StopStreamPublishing(Sender<()>),
    RequestStreamSnapshot,
    AttachLiveSource {
        source: LiveReplaySource,
        metadata: ReplayFileMetadata,
        allow_mismatched: bool,
        reply: Sender<Result<(), ReplayPlayerAttachError>>
    },
    DetachLiveSource(Sender<()>),
    StartRecordingFollowerReplay(PartialReplayRecordMetadata<std::io::BufWriter<File>, std::io::BufWriter<File>>, ReplayFileMetadata, Sender<Result<(), ReplayFileWriteError>>),
    ReadRam { address: u32, len: usize, reply: Sender<Option<Vec<u8>>> },
    EnqueueWrite { address: u32, data: Vec<u8> },
    /// Answered with `Err` when the console cannot be held (a replay is attached, already linked).
    LinkHold(Sender<Result<(u64, InputBuffer), String>>),
    LinkRelease(Sender<()>),
    /// Answered with `Err` when the loop is already lent.
    Lend(Sender<Result<LentCore, String>>),
    Link {
        lent: LentCore,
        settings: LinkSettings,
        inbox: Arc<LinkInbox>,
        publisher: Box<dyn LinkPublisherFns>
    },
    Unlink(Sender<()>),
}

/// What a primary thread keeps while it runs a partner's lent loop beside its own.
struct Linked {
    lent: LentCore,
    phase: LinkPhase,
    /// When the pair first stalled waiting for the partner's link frames (cleared when it runs).
    stalled_since: Option<Instant>,
    /// The link frame last written to the shared status.
    reported_frame: u64,
    reported_stalled: bool
}

enum LinkPhase {
    /// Bringing the lent follower to the agreed frame, then plugging in.
    Starting {
        settings: LinkSettings,
        inbox: Arc<LinkInbox>,
        publisher: Option<Box<dyn LinkPublisherFns>>,
        deadline: Instant
    },
    Running
}

/// Longest the lent follower gets to reach the agreed frame.
const LINK_START_TIMEOUT: Duration = Duration::from_secs(5);

/// Longest the pair waits for the partner's link frames before the cable is pulled. A partner
/// that merely paused looks the same as one that hung, so this is long: a partner that is gone
/// is caught much sooner by the network layer (its departure ends the link).
const LINK_STALL_TIMEOUT: Duration = Duration::from_secs(120);

/// How long the primary thread parks while the pair is stalled (an inbox push unparks it).
const LINK_STALL_WAIT: Duration = Duration::from_millis(1);

/// How often a lent-out follower thread checks for its loop coming back.
const LENT_POLL: Duration = Duration::from_millis(10);

/// What the thread keeps while its core follows another player's game.
struct FollowerLoopState {
    stats: Arc<FollowerStats>,
    last_draw: Instant
}

fn extend_counter_map(from: &BTreeMap<String, SignedInteger>, into: &mut BTreeMap<String, SignedInteger>) {
    into.retain(|k,_| from.contains_key(k));

    for (k, v) in from {
        if let Some(v2) = into.get_mut(k) {
            *v2 = *v;
        }
        else {
            into.insert(k.to_owned(), *v);
        }
    }
}

/// Everything a core thread runs: the core, its housekeeping and its stats. Normally owned by
/// its own thread ([`CoreThread`]); a follower's loop can be lent to the player's own thread for
/// a link cable (see [`ThreadedSuperShuckieCore::lend`]).
struct CoreLoop {
    screens: Weak<Mutex<Vec<ScreenData>>>,

    screens_queued: Vec<ScreenData>,
    screen_ready_for_copy: bool,
    /// See [`ElapsedTimeStats::screen_generation`].
    screen_generation: u32,
    /// `SuperShuckieCore::run_serial` of the last run whose frame was handed to `screens` (or
    /// deliberately not, because it was not drawn).
    published_run_serial: u64,
    desired_replay_frame: Arc<AtomicU32>,
    delta_replay_frames: Arc<AtomicI32>,
    replay_errors: Arc<Mutex<Vec<ReplayFileWriteError>>>,
    replay_counters: Arc<Mutex<BTreeMap<String, SignedInteger>>>,
    playback_errors: Arc<Mutex<Vec<String>>>,
    playback_paused: Arc<AtomicBool>,
    playback_frozen: bool,
    /// See [`ThreadedSuperShuckieCore::set_coarse_seek_while_frozen`].
    coarse_seek_while_frozen: bool,
    /// The exact frame the last coarse seek stood in for, sought once playback is unfrozen.
    pending_exact_frame: Option<u32>,

    core: SuperShuckieCore,
    pokeabyte_integration: Option<PokeAByteIntegrationServer>,
    is_null: bool,

    elapsed_time: Arc<RwLock<ElapsedTimeStats>>,
    frame_times: Arc<RwLock<FrameTimeStats>>,
    emulated_frames: Arc<AtomicU64>,

    freezes: BTreeMap<u64, ByteVec>,
    /// `(frame, state epoch)` the Poke-A-Byte freezes were last applied at.
    last_pokeabyte_freeze: Option<(u64, u64)>,
    /// `(frame, state epoch)` the Poke-A-Byte shared-memory reads were last taken at (see
    /// [`Self::handle_pokeabyte_integration`]: paced cores no longer report "mid-frame" on a
    /// pacing miss, so this is what keeps the reads to once per emulated frame while running).
    last_pokeabyte_read: Option<(u64, u64)>,
    replay_stalled: Arc<AtomicBool>,

    memory_monitor: Option<MemoryMonitorLocal>,

    /// Set while the core follows another player's game; `run_one_follower` runs instead of
    /// `run_one`.
    follower: Option<FollowerLoopState>,
    follower_stats: Arc<Mutex<Option<Arc<FollowerStats>>>>,

    /// Problems the stream publisher reported, for the wrapper to take (see
    /// [`ThreadedSuperShuckieCore::get_stream_errors`]).
    stream_errors: Arc<Mutex<Vec<String>>>,
    /// Problems following reported, for the wrapper to take (see
    /// [`ThreadedSuperShuckieCore::get_follower_errors`]).
    follower_errors: Arc<Mutex<Vec<String>>>,
    /// Problems the link publisher reported, for the wrapper to take (see
    /// [`ThreadedSuperShuckieCore::get_link_errors`]).
    link_errors: Arc<Mutex<Vec<String>>>,
}

/// The thread itself: its command channel and the loop it runs, unless that is lent out.
struct CoreThread {
    receiver: Receiver<ThreadCommand>,
    sender_close: Sender<()>,
    /// This thread's own loop; `None` while it is lent to another thread.
    loop_: Option<Box<CoreLoop>>,
    /// A partner's lent loop being run beside the own loop for a link cable.
    linked: Option<Linked>,
    /// See [`ThreadedSuperShuckieCore::link_status`].
    link_status: Arc<Mutex<LinkStatus>>,
    /// A command taken from the queue while the loop was lent that the borrower would no longer
    /// take: handled here, first thing, now that the loop is back.
    pending_command: Option<ThreadCommand>
}

impl CoreThread {
    fn set_link_status(&self, status: LinkStatus) {
        *self.link_status.lock().unwrap_or_else(|p| p.into_inner()) = status;
    }

    fn run_thread(mut self) {
        loop {
            let Some(loop_) = self.loop_.as_deref_mut() else {
                unreachable!("the thread runs without its loop")
            };

            let cmd = match self.pending_command.take() {
                Some(cmd) => Some(cmd),
                None => self.receiver.try_recv().ok()
            };
            if let Some(cmd) = cmd {
                match cmd {
                    ThreadCommand::Close => break,
                    ThreadCommand::Lend(reply) => {
                        // The loop goes to whoever asked; this thread forwards commands to it
                        // and waits for it to come back.
                        let (forward, commands) = channel();
                        let (return_to, returned) = channel();
                        let loop_ = self.loop_.take().expect("checked above");
                        let _ = reply.send(Ok(LentCore { loop_, commands, return_to }));
                        match self.forward_while_lent(forward, returned) {
                            Some(ReturnedCore { loop_, then_close }) => {
                                loop_.core.set_live_waker(std::thread::current());
                                self.loop_ = Some(loop_);
                                if then_close {
                                    break
                                }
                            }
                            // The borrower is gone with the loop: nothing left to run.
                            None => return self.finish_without_loop()
                        }
                        continue
                    }
                    ThreadCommand::Link { lent, settings, inbox, publisher } => {
                        self.begin_linking(lent, settings, inbox, publisher);
                        continue
                    }
                    ThreadCommand::Unlink(reply) => {
                        self.end_link(false);
                        self.set_link_status(LinkStatus::Idle);
                        let _ = reply.send(());
                        continue
                    }
                    ThreadCommand::LinkHold(reply) => {
                        let r = if self.linked.is_some() { Err(String::from("already linked")) } else { loop_.core.link_hold() };
                        if r.is_ok() {
                            self.set_link_status(LinkStatus::Holding);
                        }
                        let _ = reply.send(r);
                        continue
                    }
                    ThreadCommand::LinkRelease(reply) => {
                        loop_.core.link_release();
                        if self.linked.is_none() {
                            self.set_link_status(LinkStatus::Idle);
                        }
                        let _ = reply.send(());
                        continue
                    }
                    cmd => {
                        loop_.handle_command(cmd);
                        // counters can change without a frame running (REST while paused, seeks)
                        loop_.update_counters();
                        continue
                    }
                }
            }

            if self.linked.is_some() {
                // The partner's wrapper's commands, forwarded by its thread.
                let mut close_lent = false;
                while let Some(cmd) = self.linked.as_ref().and_then(|l| l.lent.commands.try_recv().ok()) {
                    if matches!(cmd, ThreadCommand::Close) {
                        close_lent = true;
                        break
                    }
                    if let Some(linked) = self.linked.as_mut() {
                        linked.lent.loop_.handle_command(cmd);
                        linked.lent.loop_.update_counters();
                    }
                }
                if close_lent {
                    self.end_link(true);
                    self.set_link_status(LinkStatus::Failed(LinkFailure::PartnerEnded));
                    continue
                }
                if let Some(linked) = self.linked.as_mut() {
                    linked.lent.loop_.housekeeping();
                }
            }

            let loop_ = self.loop_.as_deref_mut().expect("checked above");
            loop_.housekeeping();

            if self.linked.is_some() {
                self.run_one_linked();
                continue
            }

            let loop_ = self.loop_.as_deref_mut().expect("checked above");
            if loop_.is_running() {
                if !loop_.playback_frozen {
                    if loop_.follower.is_some() {
                        loop_.run_one_follower();
                    }
                    else {
                        loop_.run_one();
                    }
                }
            }
            else if loop_.core.stream_snapshot_pending() {
                // Paused, so no frame will complete to carry the snapshot somebody asked for.
                loop_.core.publish_pending_stream_snapshot();
            }
            else if loop_.core.replay_player.is_none() {
                // unfortunately we can't just block until we're running again because we still need
                // to handle pokeabyte writes (parked rather than slept so the RAM tools can wake us)
                std::thread::park_timeout(Duration::from_millis(100));
            }
            else {
                // wait for a reduced time so seeking can still be responsive
                std::thread::park_timeout(Duration::from_millis(10));
            }
        }

        // A link still up when the thread closes: the partner's loop goes home first.
        self.end_link(false);
        self.set_link_status(LinkStatus::Idle);

        if let Some(mut loop_) = self.loop_.take() {
            loop_.core.stop_stream_publishing();
            loop_.core.detach_live_source();
            loop_.core.stop_recording_replay();
            loop_.pokeabyte_integration = None;
            loop_.memory_monitor = None;
        }

        let _ = self.sender_close.send(());
    }

    /// The thread's loop went with a borrower that never returned it (the borrower's thread
    /// died): finish as if closed.
    fn finish_without_loop(self) {
        let _ = self.sender_close.send(());
    }

    /// While the loop is lent: forward this thread's commands to the borrower until the loop
    /// comes back (`Some`) or the borrower is gone (`None`).
    fn forward_while_lent(&mut self, forward: Sender<ThreadCommand>, returned: Receiver<ReturnedCore>) -> Option<ReturnedCore> {
        loop {
            match self.receiver.recv_timeout(LENT_POLL) {
                Ok(cmd) => {
                    if let Err(std::sync::mpsc::SendError(cmd)) = forward.send(cmd) {
                        // The borrower no longer takes commands: it has sent the loop home (the
                        // unlink and the next command race through this thread) or it died with
                        // it. Either way this command is ours to handle once the loop is back.
                        return match returned.recv_timeout(LINK_START_TIMEOUT) {
                            Ok(back) => {
                                self.pending_command = Some(cmd);
                                Some(back)
                            }
                            Err(_) => None
                        }
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                // The wrapper is gone without a Close (it always sends one first, but be safe):
                // ask for the loop back with a Close and wait for it.
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = forward.send(ThreadCommand::Close);
                    return returned.recv().ok()
                }
            }
            match returned.try_recv() {
                Ok(back) => return Some(back),
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => return None
            }
        }
    }

    /// Start a link: the lent partner loop is brought to the agreed frame over the next
    /// iterations, then plugged in.
    fn begin_linking(&mut self, lent: LentCore, settings: LinkSettings, inbox: Arc<LinkInbox>, publisher: Box<dyn LinkPublisherFns>) {
        if self.linked.is_some() {
            // Already linked (or linking): the new loop goes straight back.
            let _ = lent.return_to.send(ReturnedCore { loop_: lent.loop_, then_close: false });
            return
        }
        // Data for the follower and link frames now wake this thread.
        lent.loop_.core.set_live_waker(std::thread::current());
        inbox.set_waker(std::thread::current());
        self.linked = Some(Linked {
            lent,
            phase: LinkPhase::Starting { settings, inbox, publisher: Some(publisher), deadline: Instant::now() + LINK_START_TIMEOUT },
            stalled_since: None,
            reported_frame: u64::MAX,
            reported_stalled: false
        });
        self.set_link_status(LinkStatus::Starting);
    }

    /// End the link (whatever its phase) and send the partner's loop home. With `then_close` the
    /// partner's thread finishes once it has its loop back (its wrapper closed meanwhile).
    fn end_link(&mut self, then_close: bool) {
        let Some(mut linked) = self.linked.take() else {
            return
        };
        if let Some(loop_) = self.loop_.as_deref_mut() {
            if loop_.core.is_linked() {
                loop_.core.end_link(&mut linked.lent.loop_.core);
            }
            loop_.core.link_release();
        }
        if let Some(stats) = linked.lent.loop_.follower.as_ref().map(|f| f.stats.clone()) {
            stats.frames_behind.store(0, Ordering::Relaxed);
            stats.waiting.store(true, Ordering::Relaxed);
        }
        let _ = linked.lent.return_to.send(ReturnedCore { loop_: linked.lent.loop_, then_close });
    }

    /// One iteration of the linked loop: bring the partner up to the start frame, or step the
    /// pair.
    fn run_one_linked(&mut self) {
        let Some(linked) = self.linked.as_mut() else { return };
        let Some(loop_) = self.loop_.as_deref_mut() else { return };

        match &mut linked.phase {
            LinkPhase::Starting { settings, inbox, publisher, deadline } => {
                // Most follower frames run per iteration on the way to the start frame.
                const CATCH_UP_FRAMES: u64 = 32;
                let partner = &mut linked.lent.loop_;
                let target = settings.partner_start_frame;
                let mut ran = 0;
                while partner.core.total_frames() < target && ran < CATCH_UP_FRAMES {
                    partner.core.run_unlocked_presenting(true);
                    let frames = partner.core.last_run_time().frames;
                    ran += frames;
                    if frames > 0 {
                        partner.emulated_frames.fetch_add(frames, Ordering::Relaxed);
                    }
                    if partner.core.is_replay_waiting() || partner.core.is_replay_stalled() {
                        break
                    }
                }
                partner.update_counters();
                let at = partner.core.total_frames();
                if at == target && !partner.core.is_mid_frame() {
                    let settings = settings.clone();
                    let inbox = inbox.clone();
                    let publisher = publisher.take().expect("taken once");
                    match loop_.core.begin_link(&mut partner.core, settings.clone(), inbox, publisher) {
                        Ok(()) => {
                            *loop_.frame_times.write() = FrameTimeStats::default();
                            *partner.frame_times.write() = FrameTimeStats::default();
                            linked.phase = LinkPhase::Running;
                            self.set_link_status(LinkStatus::Linked { delay_frames: settings.delay_frames, link_frame: 0, stalled: false });
                        }
                        Err(e) => {
                            self.end_link(false);
                            self.set_link_status(LinkStatus::Failed(LinkFailure::Emulator(e)));
                        }
                    }
                }
                else if at > target || partner.core.is_replay_stalled() {
                    let why = if at > target { alloc::format!("the other player's game is at frame {at}, past the agreed frame {target}") } else { String::from("the other player's stream ended") };
                    self.end_link(false);
                    self.set_link_status(LinkStatus::Failed(LinkFailure::Emulator(why)));
                }
                else if Instant::now() > *deadline {
                    self.end_link(false);
                    self.set_link_status(LinkStatus::Failed(LinkFailure::Timeout));
                }
                else if ran == 0 {
                    std::thread::park_timeout(LINK_STALL_WAIT);
                }
            }
            LinkPhase::Running => {
                if !loop_.is_running() {
                    // Paused: the pair stalls for the partner too. Still answer snapshot requests.
                    if loop_.core.stream_snapshot_pending() {
                        loop_.core.publish_pending_stream_snapshot();
                    }
                    std::thread::park_timeout(Duration::from_millis(10));
                    return
                }
                let started = Instant::now();
                let outcome = loop_.core.run_linked(&mut linked.lent.loop_.core);
                match outcome {
                    LinkRunOutcome::Ran { local, partner_frames } => {
                        linked.stalled_since = None;
                        let partner = &mut linked.lent.loop_;
                        if partner_frames > 0 {
                            partner.emulated_frames.fetch_add(partner_frames, Ordering::Relaxed);
                            if let Some(follower) = partner.follower.as_ref() {
                                follower.stats.emulated_frames.fetch_add(partner_frames, Ordering::Relaxed);
                                follower.stats.frames_behind.store(0, Ordering::Relaxed);
                                follower.stats.waiting.store(false, Ordering::Relaxed);
                                if partner.core.last_frame_presented() {
                                    follower.stats.drawn_frames.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            partner.update_counters();
                        }
                        if local.frames > 0 {
                            loop_.emulated_frames.fetch_add(local.frames, Ordering::Relaxed);
                            loop_.frame_times.write().record(started.elapsed(), loop_.core.core.frame_period_microseconds());
                            loop_.update_counters();
                            let frame = loop_.core.link_progress().map(|(f, _)| f).unwrap_or(0);
                            if frame != linked.reported_frame || linked.reported_stalled {
                                linked.reported_frame = frame;
                                linked.reported_stalled = false;
                                let delay = loop_.core.link_progress().map(|(_, d)| d).unwrap_or(0);
                                self.set_link_status(LinkStatus::Linked { delay_frames: delay, link_frame: frame, stalled: false });
                            }
                        }
                        else if !loop_.core.is_mid_frame() {
                            // A paced console whose frame is not due yet (the Game Boy paces
                            // itself inside its step; this is the Game Boy Advance's wait).
                            if let Some(until) = loop_.core.core.microseconds_until_next_frame() {
                                let until = Duration::from_micros(until);
                                if until > CoreLoop::WAKE_EARLY {
                                    std::thread::sleep((until - CoreLoop::WAKE_EARLY).min(CoreLoop::MAX_FRAME_WAIT));
                                }
                            }
                        }
                    }
                    LinkRunOutcome::Stalled => {
                        let since = *linked.stalled_since.get_or_insert_with(Instant::now);
                        if let Some(follower) = linked.lent.loop_.follower.as_ref() {
                            follower.stats.waiting.store(true, Ordering::Relaxed);
                        }
                        if !linked.reported_stalled {
                            linked.reported_stalled = true;
                            let (frame, delay) = loop_.core.link_progress().unwrap_or((0, 0));
                            self.set_link_status(LinkStatus::Linked { delay_frames: delay, link_frame: frame, stalled: true });
                        }
                        if since.elapsed() > LINK_STALL_TIMEOUT {
                            self.end_link(false);
                            self.set_link_status(LinkStatus::Failed(LinkFailure::Timeout));
                            return
                        }
                        std::thread::park_timeout(LINK_STALL_WAIT);
                    }
                    LinkRunOutcome::Failed(failure) => {
                        self.end_link(false);
                        self.set_link_status(LinkStatus::Failed(failure));
                    }
                }
            }
        }
    }
}

impl CoreLoop {
    /// The between-runs work of a loop iteration: errors, seeks, screens, Poke-A-Byte, the RAM
    /// tools' monitor and the replay's stall flag.
    fn housekeeping(&mut self) {
        self.handle_replay_recording_errors();
        self.handle_stream_errors();
        self.handle_follower_errors();
        self.handle_link_errors();
        self.go_to_desired_frame();
        self.refresh_screen_data();
        self.update_queued_screens();
        self.handle_pokeabyte_integration();
        self.handle_memory_monitor();
        self.check_if_replay_stalled();
    }

    fn handle_link_errors(&mut self) {
        let errors = self.core.poll_link_errors();
        if !errors.is_empty() {
            self.link_errors.lock().unwrap_or_else(|p| p.into_inner()).extend(errors);
        }
    }

    /// Frames a follower sits behind the newest one received, to absorb network jitter.
    const FOLLOWER_TARGET_LAG_FRAMES: u64 = 2;

    /// Most frames a follower runs in one pass before the loop looks at commands again.
    const FOLLOWER_MAX_FRAMES_PER_PASS: u64 = 8;

    /// Longest one catch-up pass may take.
    const FOLLOWER_MAX_PASS: Duration = Duration::from_millis(4);

    /// A follower this far behind asks for a snapshot instead of emulating the backlog (5 s of
    /// the publisher's output at 60 fps).
    const FOLLOWER_RESYNC_BEHIND_FRAMES: u64 = 300;

    /// A follower draws at most this often; frames in between run hidden.
    const FOLLOWER_DRAW_INTERVAL: Duration = Duration::from_millis(16);

    /// Longest a follower parks when nothing has arrived (a push unparks it sooner).
    const FOLLOWER_WAIT: Duration = Duration::from_millis(4);

    /// Run a follower: paced by what has arrived from the publisher, never by a clock. Runs up to
    /// a bounded pass of frames when behind, parks briefly when caught up, and asks for a
    /// snapshot when hopelessly behind.
    fn run_one_follower(&mut self) {
        let Some(follower) = self.follower.as_ref() else {
            return
        };
        let stats = follower.stats.clone();
        let last_draw = follower.last_draw;

        let newest = self.core.live_newest_publisher_frame();
        let behind = newest.saturating_sub(self.core.total_frames());
        stats.frames_behind.store(behind, Ordering::Relaxed);
        if behind > Self::FOLLOWER_RESYNC_BEHIND_FRAMES {
            self.core.live_request_snapshot(SnapshotRequestReason::TooFarBehind);
        }

        let mut want = behind.saturating_sub(Self::FOLLOWER_TARGET_LAG_FRAMES).min(Self::FOLLOWER_MAX_FRAMES_PER_PASS);
        let stalled = self.core.is_replay_stalled();
        if !stalled && want == 0 && self.core.live_frames_available() == 0 && self.core.live_has_items() {
            // A snapshot with no frame after it yet (the publisher is paused or held): apply it
            // now so their screen shows where they are, rather than once they move again.
            want = 1;
        }
        if stalled || want == 0 || (self.core.live_frames_available() == 0 && !self.core.live_has_items()) {
            if !stalled {
                stats.waiting.store(true, Ordering::Relaxed);
            }
            std::thread::park_timeout(Self::FOLLOWER_WAIT);
            return
        }

        let pass_started = Instant::now();
        let mut ran = 0;
        let mut drew = false;
        while ran < want && pass_started.elapsed() < Self::FOLLOWER_MAX_PASS {
            // Draw the last frame of the pass, at most every FOLLOWER_DRAW_INTERVAL.
            let draw = !drew && ran + 1 >= want && last_draw.elapsed() >= Self::FOLLOWER_DRAW_INTERVAL;
            let started = Instant::now();
            self.core.run_unlocked_presenting(draw);
            let frames = self.core.last_run_time().frames;
            if frames > 0 {
                ran += frames;
                self.emulated_frames.fetch_add(frames, Ordering::Relaxed);
                stats.emulated_frames.fetch_add(frames, Ordering::Relaxed);
                self.frame_times.write().record(started.elapsed(), None);
                if self.core.last_frame_presented() {
                    drew = true;
                    stats.drawn_frames.fetch_add(1, Ordering::Relaxed);
                }
                self.update_counters();
            }
            if self.core.is_replay_waiting() || self.core.is_replay_stalled() {
                break
            }
            // A sliced (Game Boy) core returns without a frame many times per frame; keep going.
        }
        if drew && let Some(follower) = self.follower.as_mut() {
            follower.last_draw = Instant::now();
        }
    }

    fn handle_stream_errors(&mut self) {
        let errors = self.core.poll_stream_errors();
        if !errors.is_empty() {
            self.stream_errors.lock().unwrap_or_else(|p| p.into_inner()).extend(errors);
        }
    }

    fn handle_follower_errors(&mut self) {
        let errors = self.core.poll_follower_errors();
        if !errors.is_empty() {
            self.follower_errors.lock().unwrap_or_else(|p| p.into_inner()).extend(errors);
        }
    }

    /// Longest single wait before the next frame; keeps commands and Poke-A-Byte reads responsive.
    const MAX_FRAME_WAIT: Duration = Duration::from_millis(8);

    /// `thread::sleep` overshoots by a fraction of a millisecond, so stop sleeping this far
    /// before the deadline and let the core's own timestamp check take the last stretch.
    const WAKE_EARLY: Duration = Duration::from_micros(1000);

    /// Run the core once and, if it was not yet time for a frame, wait for most of the remaining
    /// interval instead of spinning through the loop (which burned a core at any speed).
    fn run_one(&mut self) {
        let started = Instant::now();
        self.core.run();
        let ran = self.core.last_run_time();

        if ran.frames > 0 {
            self.emulated_frames.fetch_add(ran.frames, Ordering::Relaxed);
            self.frame_times.write().record(started.elapsed(), self.core.core.frame_period_microseconds());
            self.update_counters();
            return;
        }

        if self.core.is_replay_stalled() || self.core.is_mid_frame() {
            // Not a pacing wait (Game Boy mid-frame stepping, or nothing to run): keep going.
            return;
        }

        match self.core.core.microseconds_until_next_frame() {
            Some(until) => {
                let until = Duration::from_micros(until);
                // A frame still waiting to be handed over (the UI held the screen buffers when
                // `update_queued_screens` tried) must not sit through a whole wait: at 1x that is
                // up to 8 ms, half a refresh of a 120 Hz display. Retry every millisecond instead.
                let max_wait = if self.screen_ready_for_copy { Duration::from_millis(1) } else { Self::MAX_FRAME_WAIT };
                if until > Self::WAKE_EARLY {
                    std::thread::sleep((until - Self::WAKE_EARLY).min(max_wait));
                }
            }
            None => {
                // No pacing information at all (e.g. a null core, if it ever got here despite
                // `is_running` excluding it): sleep a bounded amount rather than spinning.
                std::thread::sleep(Self::MAX_FRAME_WAIT);
            }
        }
    }

    fn check_if_replay_stalled(&mut self) {
        self.replay_stalled.store(self.core.replay_stalled, Ordering::Relaxed);
        if self.core.replay_stalled {
            self.playback_paused.store(true, Ordering::Relaxed);
        }
    }

    fn go_to_desired_frame(&mut self) {
        let delta = self.delta_replay_frames.swap(0, Ordering::Relaxed);
        let frame = self.desired_replay_frame.swap(u32::MAX, Ordering::Relaxed);

        let mut seeked = false;

        // An absolute seek (if any) applies first, THEN the delta relative to the resulting
        // frame -- both may be set in the same loop iteration (an app frame that both jumps and
        // steps), and applying only one used to silently drop the other.
        if frame != u32::MAX {
            // While frozen (the timeline is being dragged) land on the nearest keyframe and
            // remember the exact frame for when the drag ends; see `coarse_replay_frame`.
            let target = if self.playback_frozen && self.coarse_seek_while_frozen {
                let coarse = u32::try_from(self.core.coarse_replay_frame(frame as UnsignedInteger)).unwrap_or(frame);
                self.pending_exact_frame = (coarse != frame).then_some(frame);
                coarse
            }
            else {
                self.pending_exact_frame = None;
                frame
            };
            if let Err(e) = self.core.go_to_replay_frame(target as UnsignedInteger) {
                self.playback_errors.lock().unwrap_or_else(|p| p.into_inner()).push(e);
            }
            seeked = true;
        }

        if delta != 0 {
            let target = self.core.total_frames.saturating_add_signed(delta as i64);
            if let Err(e) = self.core.go_to_replay_frame(target) {
                self.playback_errors.lock().unwrap_or_else(|p| p.into_inner()).push(e);
            }
            seeked = true;
        }

        if !seeked {
            return
        }

        // A seek in a stopped replay hands the emulator back live at the new frame, which restarts
        // the wall-clock timer; keep it in step with the pause state (harmless while playing back,
        // where the timer is not consulted).
        if !self.is_running() {
            self.core.pause_timer();
        }

        // We aren't really too focused on smooth playback as opposed to updating the buffer now!
        self.force_refresh_screen_data();
        self.update_counters();
    }

    fn update_counters(&mut self) {
        let Some(c) = self.core.get_replay_counters() else {
            self.replay_counters.lock().expect("can't get replay counters to clear").clear();
            return;
        };
        extend_counter_map(c, &mut self.replay_counters.lock().expect("can't get replay counters"));
    }

    /// If the mutex was blocked, we can copy it in when it's no longer blocked.
    fn update_queued_screens(&mut self) {
        if !self.screen_ready_for_copy {
            return
        }

        let Some(screen_data) = self.screens.upgrade() else {
            panic!("update_queued_screens Can't get screen_data: owning thread must have crashed");
        };

        let mut out_screens = match screen_data.try_lock() {
            Ok(n) => n,
            Err(TryLockError::WouldBlock) => return,
            Err(e) => panic!("update_queued_screens Can't get screens mutex: {e}")
        };

        self.screen_ready_for_copy = false;

        let in_screens = &mut self.screens_queued;
        core::mem::swap(in_screens, &mut *out_screens);

        self.screen_generation = self.screen_generation.wrapping_add(1);
        self.update_elapsed_time();
    }

    fn update_elapsed_time(&self) {
        *self.elapsed_time.write() = ElapsedTimeStats {
            milliseconds: self.core.get_recording_milliseconds().0 as u32,
            frames: self.core.total_frames as u32,
            speed: self.core.game_speed,
            replay_frame: self.core.replay_position().0 as u32,
            screen_generation: self.screen_generation
        };
    }

    fn is_running(&self) -> bool {
        !self.is_null && !self.playback_paused.load(Ordering::Relaxed)
    }

    /// Publish the most recently drawn frame to the screen buffer (or queue it if the reader
    /// holds the buffer right now). Frames that were emulated but not drawn only update the
    /// elapsed-time stats.
    fn refresh_screen_data(&mut self) {
        if self.is_running() && self.core.is_mid_frame() {
            return
        }

        // Only a run that drew something has anything to publish; runs made inside commands
        // (loading a state, a seek's last frame) count too, hence the serial rather than a flag.
        let run_serial = self.core.run_serial();
        if run_serial == self.published_run_serial {
            self.update_elapsed_time();
            return
        }
        self.published_run_serial = run_serial;
        if !self.core.last_frame_presented() {
            self.update_elapsed_time();
            return
        }

        let Some(screen_data) = self.screens.upgrade() else {
            panic!("refresh_screen_data Can't get screen_data: owning thread must have crashed");
        };

        let mut out_screens_maybe = screen_data.try_lock();

        let out_screens_result = match out_screens_maybe.as_mut() {
            Ok(n) => {
                self.screen_ready_for_copy = false;
                self.screen_generation = self.screen_generation.wrapping_add(1);
                self.update_elapsed_time();
                &mut *n
            },
            Err(TryLockError::WouldBlock) => {
                self.screen_ready_for_copy = true;
                self.update_elapsed_time();
                &mut self.screens_queued
            },
            Err(e) => panic!("refresh_screen_data Can't get screens mutex: {e}")
        };

        self.core.core.swap_screen_data(out_screens_result.as_mut_slice());
    }

    fn handle_replay_recording_errors(&mut self) {
        let errors = self.core.poll_replay_recording_errors();
        if errors.is_empty() {
            return;
        }

        // A `TempSink` error means only the crash-safe temp copy failed; the final file (and thus
        // the recording) is still fine, so it is reported but does not stop the recording. Any
        // other error means the final file itself is in trouble.
        if errors.iter().any(|e| !matches!(e, ReplayFileWriteError::TempSink { .. })) {
            self.core.force_stop_recording_replay();
        }
        self.replay_errors.lock().expect("could not get replay errors mutex").extend(errors.into_iter());
    }

    fn force_refresh_screen_data(&mut self) {
        let Some(screen_data) = self.screens.upgrade() else {
            panic!("force_refresh_screen_data Can't get screen_data: owning thread must have crashed");
        };

        let mut out_screens = screen_data
            .lock()
            .expect("can't get screens mutex force_get_screen_data");

        self.screen_generation = self.screen_generation.wrapping_add(1);
        self.published_run_serial = self.core.run_serial();
        self.update_elapsed_time();
        self.screen_ready_for_copy = false;

        for (screen_from, screen_to) in self.core.core.get_screens().iter().zip(out_screens.iter_mut()) {
            screen_to.pixels.copy_from_slice(screen_from.pixels.as_slice());
        }
    }

    /// Freeze entries held at once; a Poke-A-Byte client that asks for more than this just stops
    /// gaining new freezes (existing ones, and the one-shot write that came with a refused
    /// request, are unaffected).
    const MAX_POKEABYTE_FREEZES: usize = 256;

    /// Update RAM read/writes
    fn handle_pokeabyte_integration(&mut self) {
        let Some(integration) = self.pokeabyte_integration.as_ref() else {
            return
        };

        let mut session_lock = integration.get_session();
        let Some(session) = session_lock.as_mut() else {
            return;
        };

        for write in &mut session.writes {
            match write {
                PokeAByteEmulatorCommand::Write { address, data } => {
                    // A replay's `WriteMemory` packets are u32-addressed; an address that does not
                    // fit is simply not something this core can write.
                    let Ok(narrow_address) = u32::try_from(address) else { continue };
                    self.core.enqueue_write(narrow_address, data);
                },
                PokeAByteEmulatorCommand::Freeze { address, data } => {
                    let Ok(narrow_address) = u32::try_from(address) else { continue };
                    self.core.enqueue_write(narrow_address, data.clone());
                    if self.freezes.contains_key(&address) || self.freezes.len() < Self::MAX_POKEABYTE_FREEZES {
                        self.freezes.insert(address, data);
                    }
                },
                PokeAByteEmulatorCommand::Unfreeze { address } => {
                    self.freezes.remove(&address);
                },
                PokeAByteEmulatorCommand::Reset => {
                    self.freezes.clear();
                }
            }
        }

        // don't update reads or apply freezes mid-frame; it's too slow
        let is_running = self.is_running();
        if self.core.is_mid_frame() && is_running {
            return;
        }

        let frame_key = (self.core.total_frames(), self.core.state_epoch());

        // apply freezes once per emulated frame regardless of frame skipping setting, and only when
        // the game changed the value (every write is recorded into a replay being recorded)
        if is_running && self.last_pokeabyte_freeze != Some(frame_key) {
            self.last_pokeabyte_freeze = Some(frame_key);
            for (address, data) in &self.freezes {
                self.core.write_if_changed(*address as u32, data.as_slice());
            }
        }

        // Reads are cheap to poll every loop while paused, but a paced core's thread loop calls
        // this many times per emulated frame while running (it no longer reports "mid-frame" on a
        // pacing miss -- see `EmulatorCore::is_mid_frame`); keep them to once per emulated frame,
        // the same way the freeze restore above already does.
        if is_running {
            if self.last_pokeabyte_read == Some(frame_key) {
                return;
            }
            self.last_pokeabyte_read = Some(frame_key);
        }

        // handle frame skipping unless we're paused (or we haven't set up yet)
        if !session.is_first_frame() && is_running && let Some(skipping) = session.config.frame_skip && self.core.total_frames % ((skipping as u64) + 1) != 0 {
            return
        }

        // SAFETY: "Only one way to find out"
        let ram = unsafe { session.shared_memory.get_memory_mut() };
        for read in &session.config.blocks {
            let into = ram.get_mut(read.range.clone()).expect("read range was wrong (this should have been checked!)");
            let _ = self.core.get_core().read_ram(read.game_address, into); // TODO: handle this?
        }

        session.finish_frame();
    }

    /// Service the RAM tools' memory monitor, if attached.
    fn handle_memory_monitor(&mut self) {
        let running = self.is_running();
        let Some(monitor) = self.memory_monitor.as_mut() else {
            return
        };
        let outcome = monitor.service(&mut self.core, running);
        if outcome.pause && !self.playback_paused.swap(true, Ordering::Relaxed) {
            self.core.pause_timer();
        }
    }

    fn handle_command(&mut self, command: ThreadCommand) {
        match command {
            ThreadCommand::Start(sender) => {
                if self.playback_paused.swap(false, Ordering::Relaxed) {
                    if self.core.replay_stalled {
                        if let Err(e) = self.core.go_to_replay_frame(0) {
                            self.playback_errors.lock().unwrap_or_else(|p| p.into_inner()).push(e);
                        }
                    }
                    self.core.unpause_timer();
                }
                let _ = sender.send(());
            }
            ThreadCommand::Pause(sender) => {
                if !self.playback_paused.swap(true, Ordering::Relaxed) {
                    self.core.pause_timer();
                }
                let _ = sender.send(());
            }
            ThreadCommand::SetPokeABytePort(port, err) => {
                match port {
                    None => {
                        self.pokeabyte_integration = None;
                        let _ = err.send(Ok(()));
                    }
                    Some(port) if self.pokeabyte_integration.as_ref().is_some_and(|i| i.port() == port) => {
                        let _ = err.send(Ok(()));
                    }
                    Some(port) => {
                        // Release the old port before binding the new one (the old one may be it).
                        self.pokeabyte_integration = None;
                        match PokeAByteIntegrationServer::begin_listen(port) {
                            Ok(n) => {
                                self.pokeabyte_integration = Some(n);
                                let _ = err.send(Ok(()));
                            },
                            Err(e) => {
                                let _ = err.send(Err(format!("{e:?}")));
                            }
                        }
                    }
                }
            }
            ThreadCommand::StartRecordingReplay(metadata, reply) => {
                self.replay_errors.lock().unwrap_or_else(|p| p.into_inner()).clear();

                let r = self.core.start_recording_replay(metadata);
                if r.is_ok() && !self.is_running() {
                    self.core.pause_timer();
                }
                let _ = reply.send(r);
            }
            ThreadCommand::ResumeRecordingReplay { resume_at_frame, metadata, crop_policy, bookmarks, reply } => {
                self.replay_errors.lock().unwrap_or_else(|p| p.into_inner()).clear();

                let r = self.core.resume_recording_replay(resume_at_frame, metadata, crop_policy, bookmarks);
                if r.is_ok() && !self.is_running() {
                    self.core.pause_timer();
                }
                let _ = reply.send(r);
            }
            ThreadCommand::StopRecordingReplay(sender) => {
                let _ = sender.send(self.core.stop_recording_replay() == Some(true));
            }
            ThreadCommand::ExportVideo { mut sink, range, layout, cancel, progress, done } => {
                let result = self.core.export_frames(range, layout, sink.as_mut(), &cancel, |d, t| {
                    *progress.write() = (d, t);
                });
                let _ = done.send(result);
            }
            ThreadCommand::EnqueueInput(input) => {
                self.core.enqueue_input(input);
            }
            ThreadCommand::SetSpeed(speed) => {
                self.core.set_speed(speed);
                *self.frame_times.write() = FrameTimeStats::default();
            }
            ThreadCommand::SetRapidFireInput(input) => {
                self.core.set_rapid_fire_input(input);
            }
            ThreadCommand::SetToggledInput(input) => {
                self.core.set_toggled_input(input);
            }
            ThreadCommand::PressForFrames(input, frames) => {
                self.core.press_for_frames(input, frames);
            }
            ThreadCommand::HardReset => {
                self.core.hard_reset();
            }
            ThreadCommand::CreateSaveState(sender) => {
                self.core.finish_current_frame();
                let _ = sender.send(self.core.create_save_state());
            }
            ThreadCommand::LoadSaveState(state) => {
                self.core.load_save_state(&state);
            }
            ThreadCommand::SetPlaybackFrozen(paused) => {
                self.playback_frozen = paused;
                if !paused {
                    if let Some(frame) = self.pending_exact_frame.take() {
                        // Settle on the frame the drag ended at, unless a newer request is
                        // already waiting (it wins, as always).
                        let _ = self.desired_replay_frame.compare_exchange(u32::MAX, frame, Ordering::Relaxed, Ordering::Relaxed);
                    }
                }
            }
            ThreadCommand::SetCoarseSeekWhileFrozen(coarse) => {
                self.coarse_seek_while_frozen = coarse;
            }
            ThreadCommand::SaveSRAM(sender) => {
                let _ = sender.send(self.core.save_sram());
            }
            ThreadCommand::Close => {
                unreachable!("handle_command(ThreadCommand::Close) should not happen")
            },
            ThreadCommand::AttachReplayPlayer { player, allow_mismatched, reply } => {
                let r = self.core.attach_replay_player(player, allow_mismatched);
                if r.is_ok() && !self.is_running() {
                    self.core.pause_timer();
                }
                let _ = reply.send(r);
            }
            ThreadCommand::DetachReplayPlayer => {
                self.core.detach_replay_player();
                self.pending_exact_frame = None;
            }
            ThreadCommand::StopReplayPlayback(reply) => {
                self.core.stop_replay_playback();
                // Live from here on, so the wall-clock timer matters again: keep it in step with
                // the pause state (stop resumes it from the replay's time).
                if !self.is_running() {
                    self.core.pause_timer();
                }
                self.update_elapsed_time();
                let _ = reply.send(());
            }
            ThreadCommand::ResumeReplayPlayback(reply) => {
                let r = self.core.resume_replay_playback();
                // Whatever was played live since stopping is gone; show the resume point now.
                self.force_refresh_screen_data();
                self.update_counters();
                let _ = reply.send(r);
            }
            ThreadCommand::GoToReplayResumePoint => {
                // A seek in a stopped replay (see go_to_desired_frame): fire-and-forget, so a
                // failure is reported the same way.
                if let Err(e) = self.core.go_to_replay_resume_point() {
                    self.playback_errors.lock().unwrap_or_else(|p| p.into_inner()).push(e);
                }
                if !self.is_running() {
                    self.core.pause_timer();
                }
                self.force_refresh_screen_data();
                self.update_counters();
            }
            // Always answered, with `None` when not recording: `call` reads a dropped reply
            // sender as the thread having died, so "no answer" must never be used to mean "no".
            ThreadCommand::MarkReplayStart(timestamp, timer_offset) => {
                let _ = timestamp.send(self.core.mark_start(timer_offset));
            }
            ThreadCommand::MarkReplayEnd(timestamp) => {
                let _ = timestamp.send(self.core.mark_end());
            }
            ThreadCommand::ChangeReplayCounter { name, delta } => {
                self.core.change_replay_counter(name, delta);
            }
            ThreadCommand::BookmarkAnchor(sender, keyframe, deadline) => {
                // The caller has already given up waiting past `deadline`; placing the bookmark
                // (and, for a keyframe bookmark, writing its keyframe) now would only leave an
                // orphan nobody asked for any more.
                if Instant::now() <= deadline {
                    let _ = sender.send(self.core.bookmark_anchor(keyframe));
                }
            }
            ThreadCommand::EstimateMillisAt(sender, frame) => {
                let _ = sender.send(self.core.estimate_millis_at(frame));
            }
            ThreadCommand::SetReplayBookmarks(table) => {
                self.core.set_replay_bookmarks(table);
            }
            ThreadCommand::IgnoreSpeedChangesInReplay(ignored) => {
                self.core.set_ignore_speed_changes_in_replays(ignored)
            }
            ThreadCommand::AutoResyncKeyframesInReplay(resync) => {
                self.core.set_auto_resync_keyframes_in_replays(resync)
            },
            ThreadCommand::SetAudioOutput(output) => {
                self.core.set_audio_output(output)
            },
            ThreadCommand::SetAudioEnabled(enabled) => {
                self.core.set_audio_enabled(enabled)
            },
            ThreadCommand::SetGbPaletteOverride(colors) => {
                self.core.set_gb_palette_override(colors)
            },
            ThreadCommand::GetGbPalettes(sender) => {
                let _ = sender.send(self.core.gb_palettes());
            },
            ThreadCommand::SetAudioMuteWhenSpedUp(mute) => {
                self.core.set_audio_mute_when_sped_up(mute)
            },
            ThreadCommand::TransferPokeAByteIntegrationExternal(sender, core_sender) => {
                let Some(server) = self.pokeabyte_integration.take() else {
                    let _ = sender.send(false);
                    return;
                };
                let Some(replay_console_type) = self.core.core.replay_console_type() else {
                    let _ = sender.send(false);
                    return;
                };
                let rom_checksum = *self.core.core.rom_checksum();
                // The other core's thread answers; if it is gone, answer here rather than drop the
                // reply sender, which `call` would take to mean *this* thread has died.
                if let Err(SendError(ThreadCommand::TransferPokeAByteIntegrationInternal(sender, ..))) = core_sender.send(ThreadCommand::TransferPokeAByteIntegrationInternal(sender, server, replay_console_type, rom_checksum)) {
                    let _ = sender.send(false);
                }
            },
            ThreadCommand::TransferPokeAByteIntegrationInternal(sender, server, replay_console_type, rom_checksum) => {
                if self.core.core.rom_checksum() != &rom_checksum || self.core.core.replay_console_type() != Some(replay_console_type) {
                    let _ = sender.send(false);
                    return;
                }
                self.pokeabyte_integration = Some(server);
                let _ = sender.send(true);
            },
            ThreadCommand::Rendezvous(sender) => {
                let _ = sender.send(());
            }
            ThreadCommand::SetMemoryMonitor(monitor) => {
                self.memory_monitor = monitor.map(MemoryMonitorLocal::new);
            }
            ThreadCommand::StartStreamPublishing(publisher, reply) => {
                let _ = reply.send(self.core.start_stream_publishing(publisher));
            }
            ThreadCommand::StopStreamPublishing(reply) => {
                self.core.stop_stream_publishing();
                let _ = reply.send(());
            }
            ThreadCommand::RequestStreamSnapshot => {
                self.core.request_stream_snapshot();
                if !self.is_running() {
                    self.core.publish_pending_stream_snapshot();
                }
            }
            ThreadCommand::AttachLiveSource { source, metadata, allow_mismatched, reply } => {
                let stats = source.stats().clone();
                let r = self.core.attach_live_replay_source(source, &metadata, allow_mismatched);
                if r.is_ok() {
                    self.core.set_live_waker(std::thread::current());
                    *self.follower_stats.lock().unwrap_or_else(|p| p.into_inner()) = Some(stats.clone());
                    self.follower = Some(FollowerLoopState { stats, last_draw: Instant::now() - Self::FOLLOWER_DRAW_INTERVAL });
                    self.pending_exact_frame = None;
                    *self.frame_times.write() = FrameTimeStats::default();
                }
                let _ = reply.send(r);
            }
            ThreadCommand::DetachLiveSource(reply) => {
                self.core.detach_live_source();
                self.follower = None;
                *self.follower_stats.lock().unwrap_or_else(|p| p.into_inner()) = None;
                if !self.is_running() {
                    self.core.pause_timer();
                }
                let _ = reply.send(());
            }
            ThreadCommand::StartRecordingFollowerReplay(metadata, publisher, reply) => {
                self.replay_errors.lock().unwrap_or_else(|p| p.into_inner()).clear();
                let _ = reply.send(self.core.start_recording_follower_replay(metadata, publisher));
            }
            ThreadCommand::ReadRam { address, len, reply } => {
                let mut data = alloc::vec![0u8; len];
                let _ = reply.send(self.core.get_core().read_ram(address, &mut data).ok().map(|_| data));
            }
            ThreadCommand::EnqueueWrite { address, data } => {
                let mut bytes = ByteVec::with_capacity(data.len());
                bytes.extend_from_slice(&data);
                self.core.enqueue_write(address, bytes);
            }
            // Handled by the thread (they concern which thread runs the loop), never here. A
            // forwarded one (a lent loop's wrapper asking) is answered as refused.
            ThreadCommand::LinkHold(reply) => {
                let _ = reply.send(Err(String::from("this console is lent to another")));
            }
            ThreadCommand::LinkRelease(reply) => {
                let _ = reply.send(());
            }
            ThreadCommand::Lend(reply) => {
                let _ = reply.send(Err(String::from("this console is already lent")));
            }
            ThreadCommand::Link { lent, .. } => {
                let _ = lent.return_to.send(ReturnedCore { loop_: lent.loop_, then_close: false });
            }
            ThreadCommand::Unlink(reply) => {
                let _ = reply.send(());
            }
        }
    }
}

/// Tell the OS this thread is the one the user is waiting on.
///
/// On Windows 11 with a hybrid CPU, a busy thread in a process that is not the foreground window
/// is fair game for the efficiency cores and for EcoQoS clock limits, which is a 30-40 % loss
/// that shows up as lost speed. Opting the thread out of power throttling and raising its
/// priority slightly keeps it on a performance core at full clock. No-op elsewhere.
#[cfg(windows)]
fn mark_thread_role(role: CoreThreadRole) {
    match role {
        CoreThreadRole::Primary => mark_thread_latency_sensitive(),
        CoreThreadRole::Follower => mark_thread_background()
    }
}

#[cfg(not(windows))]
fn mark_thread_role(_role: CoreThreadRole) {}

/// The opposite of [`mark_thread_latency_sensitive`]: a follower of another player's game may
/// run on an efficiency core and yields to the player's own game whenever both want the CPU.
#[cfg(windows)]
fn mark_thread_background() {
    const THREAD_PRIORITY_BELOW_NORMAL: i32 = -1;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThread() -> *mut core::ffi::c_void;
        fn SetThreadPriority(thread: *mut core::ffi::c_void, priority: i32) -> i32;
    }

    // SAFETY: plain Win32 calls on the current thread.
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
}

#[cfg(windows)]
fn mark_thread_latency_sensitive() {
    #[repr(C)]
    struct ThreadPowerThrottlingState {
        version: u32,
        control_mask: u32,
        state_mask: u32,
    }

    const THREAD_POWER_THROTTLING_CURRENT_VERSION: u32 = 1;
    const THREAD_POWER_THROTTLING_EXECUTION_SPEED: u32 = 0x1;
    const THREAD_INFORMATION_CLASS_POWER_THROTTLING: i32 = 6; // ThreadPowerThrottling
    const THREAD_PRIORITY_ABOVE_NORMAL: i32 = 1;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThread() -> *mut core::ffi::c_void;
        fn SetThreadInformation(thread: *mut core::ffi::c_void, class: i32, info: *const core::ffi::c_void, size: u32) -> i32;
        fn SetThreadPriority(thread: *mut core::ffi::c_void, priority: i32) -> i32;
    }

    let state = ThreadPowerThrottlingState {
        version: THREAD_POWER_THROTTLING_CURRENT_VERSION,
        control_mask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
        state_mask: 0,
    };

    // SAFETY: plain Win32 calls on the current thread with a correctly sized, initialised struct.
    unsafe {
        let thread = GetCurrentThread();
        let _ = SetThreadInformation(
            thread,
            THREAD_INFORMATION_CLASS_POWER_THROTTLING,
            (&state as *const ThreadPowerThrottlingState).cast(),
            core::mem::size_of::<ThreadPowerThrottlingState>() as u32,
        );
        let _ = SetThreadPriority(thread, THREAD_PRIORITY_ABOVE_NORMAL);
    }
}

#[cfg(not(windows))]
fn mark_thread_latency_sensitive() {}

#[cfg(test)]
mod tests {
    use super::*;
    use core::num::NonZeroU64;
    use supershuckie_replay_recorder::replay_file::record::ReplayFileRecorderSettings;
    use supershuckie_replay_recorder::replay_file::ReplayPatchFormat;

    /// (g) Once the core thread has exited, the wrapper must report errors instead of panicking,
    /// and `is_alive()` must reflect that once a call has observed the disconnected channel.
    ///
    /// The thread is made to exit by sending it `Close` directly (bypassing `Drop`) and waiting
    /// for its close acknowledgment, which is a deterministic stand-in for a genuine crash: by the
    /// time the acknowledgment is sent, the thread is committed to returning (and dropping its
    /// receiver) with nothing left to race against a subsequent send from this test.
    #[test]
    fn dead_core_thread_reports_errors_instead_of_panicking() {
        let core = ThreadedSuperShuckieCore::new(Box::new(crate::emulator::NullEmulatorCore));
        assert!(core.is_alive(), "a freshly constructed core should be alive");

        let _ = core.sender.send(ThreadCommand::Close);
        let _ = core.receiver_close.recv();

        let dir = std::env::temp_dir().join("supershuckie-core-thread-dead-test");
        let _ = std::fs::create_dir_all(&dir);
        let final_file = File::create(dir.join("final.replay")).expect("create temp final file");
        let temp_file = File::create(dir.join("temp.replay")).expect("create temp temp file");

        let metadata = PartialReplayRecordMetadata {
            rom_name: "dead".into(),
            rom_filename: "dead".into(),
            settings: ReplayFileRecorderSettings::default(),
            patch_format: ReplayPatchFormat::Unpatched,
            patch_target_checksum: Default::default(),
            patch_data: ByteVec::new(),
            frames_per_keyframe: NonZeroU64::new(1000).unwrap(),
            final_file: std::io::BufWriter::new(final_file),
            temp_file: std::io::BufWriter::new(temp_file),
        };

        let result = core.start_recording_replay(metadata);
        assert!(result.is_err(), "start_recording_replay should report an error on a dead core thread, not panic");
        assert!(!core.is_alive(), "the wrapper should now know the thread is dead");

        assert_eq!(core.create_save_state(), None, "create_save_state should return None on a dead core thread, not panic");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A refused request must not look like a dead thread: `/mark-start` and `/mark-end` while a
    /// replay is playing back (not recording) used to make the handler drop its reply sender,
    /// which `call` reads as the thread having exited -- the frontend then reported "The emulator
    /// thread has stopped" and unloaded the ROM the moment the overlay marked the run's start or
    /// end.
    #[test]
    fn refused_mark_start_and_end_do_not_declare_the_thread_dead() {
        let mut core = ThreadedSuperShuckieCore::new(Box::new(crate::emulator::NullEmulatorCore));

        assert_eq!(core.mark_start(TimestampMillis(0)), Err(()), "not recording, so the mark is refused");
        assert!(core.is_alive(), "a refused mark_start must not be mistaken for a dead thread");

        assert_eq!(core.mark_end(), Err(()), "not recording, so the mark is refused");
        assert!(core.is_alive(), "a refused mark_end must not be mistaken for a dead thread");

        // The thread really is still there: a round trip still gets answered.
        assert!(core.create_save_state().is_some(), "the thread should still answer after refused marks");
        assert!(core.is_alive());
    }

    /// A lent loop keeps answering its wrapper through whoever holds it, comes home when the
    /// borrower returns it, and a `Close` sent while lent still ends the thread cleanly.
    #[test]
    fn lend_and_return() {
        let core = ThreadedSuperShuckieCore::new_with_role(Box::new(crate::emulator::NullEmulatorCore), CoreThreadRole::Follower);
        let lent = core.lend().expect("lend");
        assert!(core.is_alive());

        // A borrower thread that runs the wrapper's commands against the lent loop, like the
        // primary thread does for a linked partner.
        let (done_tx, done_rx) = channel();
        let borrower = std::thread::spawn(move || {
            let mut lent = lent;
            loop {
                match lent.commands.recv() {
                    Ok(ThreadCommand::Close) => {
                        let _ = lent.return_to.send(ReturnedCore { loop_: lent.loop_, then_close: true });
                        break
                    }
                    Ok(cmd) => lent.loop_.handle_command(cmd),
                    Err(_) => break
                }
            }
            let _ = done_tx.send(());
        });

        // The wrapper's calls are answered by the borrower.
        assert!(core.create_save_state().is_some(), "a round trip reaches the borrowed loop");
        assert!(core.get_sram().is_some());
        assert!(core.lend().is_err(), "a lent loop cannot be lent again");
        assert!(core.is_alive());

        // Closing while lent: the loop comes home and the thread finishes.
        drop(core);
        done_rx.recv_timeout(Duration::from_secs(5)).expect("the borrower saw the Close");
        borrower.join().unwrap();
    }

    /// A borrower that goes away with the loop (its thread died) ends the follower's thread
    /// rather than leaving it forwarding into the void.
    #[test]
    fn a_lost_borrower_ends_the_lender_cleanly() {
        let core = ThreadedSuperShuckieCore::new_with_role(Box::new(crate::emulator::NullEmulatorCore), CoreThreadRole::Follower);
        let lent = core.lend().expect("lend");
        drop(lent);
        // The thread notices the loop is never coming back and exits; the wrapper sees it dead.
        let started = Instant::now();
        while core.is_alive() && started.elapsed() < Duration::from_secs(5) {
            let _ = core.create_save_state();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!core.is_alive(), "the lender's thread should have exited");
        // Dropping it must not hang.
        drop(core);
    }
}
