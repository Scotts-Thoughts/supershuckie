use crate::emulator::{EmulatorCore, Input, PartialReplayRecordMetadata, ScreenData};
use crate::export::{ExportRange, ScreenLayout, VideoExportError, VideoFrameSink};
use crate::{std_timestamp_provider, AudioOutput, ReplayPlayerAttachError, Speed};
use crate::{SuperShuckieCore, SuperShuckieRapidFire};
use spin::RwLock;
use std::borrow::ToOwned;
use std::boxed::Box;
use std::collections::BTreeMap;
use std::format;
use std::fs::File;
use std::string::String;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, TryLockError, Weak};
use std::time::{Duration, Instant};
use std::vec::Vec;
use supershuckie_pokeabyte_integration::PokeAByteEmulatorCommand;
#[cfg(feature = "pokeabyte")]
use supershuckie_pokeabyte_integration::PokeAByteIntegrationServer;
use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::replay_file::record::{ReplayFileWriteError, ResumeCropPolicy};
use supershuckie_replay_recorder::replay_file::{ReplayConsoleType, ReplayHeaderBlake3Hash};
use supershuckie_replay_recorder::{ByteVec, SignedInteger, TimestampMillis, UnsignedInteger};

/// A (mostly) non-blocking, threaded wrapper for [`SuperShuckieCore`].
pub struct ThreadedSuperShuckieCore {
    screens: Arc<Mutex<Vec<ScreenData>>>,
    sender: Sender<ThreadCommand>,
    receiver_close: Receiver<()>,

    desired_replay_frame: Arc<AtomicU32>,
    delta_replay_frames: Arc<AtomicI32>,
    elapsed_time: Arc<RwLock<ElapsedTimeStats>>,
    frame_times: Arc<RwLock<FrameTimeStats>>,
    emulated_frames: Arc<AtomicU64>,
    playback_paused: Arc<AtomicBool>,
    replay_stalled: Arc<AtomicBool>,

    playback: bool,
    playback_total_frames: UnsignedInteger,
    playback_total_milliseconds: TimestampMillis,
    replay_errors: Arc<Mutex<Vec<ReplayFileWriteError>>>,
    replay_counters: Arc<Mutex<BTreeMap<String, SignedInteger>>>
}

/// Current elapsed time, retrieved atomically (the frame count corresponds to milliseconds and vice versa).
#[derive(Copy, Clone, Debug, Default)]
#[expect(missing_docs)]
pub struct ElapsedTimeStats {
    pub milliseconds: u32,
    pub frames: u32,
    pub speed: Speed,

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
    /// Wrap the given `core`.
    pub fn new(emulator_core: Box<dyn EmulatorCore>) -> Self {
        let screens = Arc::new(Mutex::new(emulator_core.get_screens().to_vec()));
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

        let elapsed_time = Arc::new(RwLock::new(ElapsedTimeStats::default()));
        let frame_times = Arc::new(RwLock::new(FrameTimeStats::default()));
        let emulated_frames = Arc::new(AtomicU64::new(0));

        {
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
            let _ = std::thread::Builder::new().name("ThreadedSuperShuckieCore".to_owned()).spawn(move || {
                mark_thread_latency_sensitive();
                ThreadedSuperShuckieCoreThread {
                    screens,
                    is_null: emulator_core.is_null(),
                    screens_queued: emulator_core.get_screens().to_vec(),
                    screen_ready_for_copy: false,
                    screen_generation: 0,
                    published_run_serial: 0,
                    core: SuperShuckieCore::new(emulator_core, std_timestamp_provider()),
                    pokeabyte_integration: None,
                    receiver,
                    sender_close,
                    desired_replay_frame,
                    elapsed_time,
                    frame_times,
                    emulated_frames,
                    delta_replay_frames,
                    replay_errors,
                    replay_counters,
                    replay_stalled,
                    playback_frozen: false,
                    freezes: BTreeMap::new(),
                    playback_paused
                }.run_thread();
            });
        }

        Self {
            sender,
            screens,
            receiver_close,
            playback_total_frames,
            playback_total_milliseconds,
            replay_errors,
            elapsed_time,
            frame_times,
            emulated_frames,
            replay_counters,
            playback: false,
            desired_replay_frame,
            delta_replay_frames,
            playback_paused,
            replay_stalled
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
        let lock = self.screens.lock().expect("screen mutex is poisoned");
        reader(lock.as_slice())
    }

    /// Start running continuously.
    ///
    /// NOTE: This is blocking.
    pub fn start(&self) {
        if !self.playback_paused.load(Ordering::Relaxed) {
            return
        }

        let (sender, receiver) = channel();
        self.sender.send(ThreadCommand::Start(sender))
            .expect("Start - the core thread has crashed");
        let _ = receiver.recv();
    }

    /// Pause running.
    ///
    /// NOTE: This is blocking.
    pub fn pause(&self) {
        if self.playback_paused.load(Ordering::Relaxed) {
            return
        }

        let (sender, receiver) = channel();
        self.sender.send(ThreadCommand::Pause(sender))
            .expect("Pause - the core thread has crashed");
        let _ = receiver.recv();
    }

    /// Block until this command is reached.
    pub fn rendezvous(&self) {
        let (sender, receiver) = channel();
        self.sender.send(ThreadCommand::Rendezvous(sender))
            .expect("Pause - the core thread has crashed");
        let _ = receiver.recv();
    }

    /// Pause running temporarily.
    pub fn set_playback_frozen(&self, paused: bool) {
        self.sender.send(ThreadCommand::SetPlaybackFrozen(paused))
            .expect("SetPlaybackFrozen - the core thread has crashed");
    }

    /// Attach/detach a Poke-A-Byte integration server.
    pub fn set_pokeabyte_enabled(&self, enabled: bool) -> Result<(), String> {
        let (sender, receiver) = channel();

        self.sender.send(ThreadCommand::SetPokeAByteEnabled(enabled, sender))
            .expect("SetPokeAByteEnabled - the core thread has crashed");

        receiver.recv().ok().unwrap_or(Ok(()))
    }

    /// Stop recording replay.
    pub fn start_recording_replay(&self, metadata: PartialReplayRecordMetadata<std::io::BufWriter<File>, std::io::BufWriter<File>>) {
        self.sender.send(ThreadCommand::StartRecordingReplay(metadata))
            .expect("StopRecordingReplay - the core thread has crashed");
    }

    /// Resume recording from an existing replay.
    ///
    /// The source replay must already be attached for playback (e.g. via `attach_replay_player`);
    /// the core thread consumes that attached player to build the new file's prefix.
    /// `resume_at_frame == None` resumes from the final frame.
    pub fn resume_recording_replay(
        &mut self,
        resume_at_frame: Option<UnsignedInteger>,
        metadata: PartialReplayRecordMetadata<std::io::BufWriter<File>, std::io::BufWriter<File>>,
        crop_policy: ResumeCropPolicy,
    ) {
        // The source replay was attached for positioning; resuming transitions us out of playback
        // and into live recording, so clear the wrapper's playback state (mirrors detach).
        self.playback_total_frames = 0;
        self.playback_total_milliseconds = 0.into();
        self.playback = false;
        self.sender.send(ThreadCommand::ResumeRecordingReplay {
            resume_at_frame,
            metadata,
            crop_policy,
        }).expect("ResumeRecordingReplay - the core thread has crashed");
    }

    /// Stop recording replay.
    pub fn stop_recording_replay(&self) -> bool {
        let (sender, receiver) = channel();

        self.sender.send(ThreadCommand::StopRecordingReplay(sender))
            .expect("StopRecordingReplay - the core thread has crashed");

        receiver.recv().ok().unwrap_or(false)
    }

    /// Begin a blocking video export on the core thread.
    ///
    /// A replay must already be attached for playback (the export reuses the attached player).
    /// While the export runs, normal playback/stepping on the core thread is paused. Returns a
    /// [`VideoExportHandle`] to poll progress, cancel, and retrieve the result.
    pub fn export_replay(
        &self,
        sink: Box<dyn VideoFrameSink>,
        range: ExportRange,
        layout: ScreenLayout,
    ) -> VideoExportHandle {
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(RwLock::new((0u64, 0u64)));
        let (done_sender, done_receiver) = channel();

        self.sender.send(ThreadCommand::ExportVideo {
            sink,
            range,
            layout,
            cancel: cancel.clone(),
            progress: progress.clone(),
            done: done_sender,
        }).expect("ExportVideo - the core thread has crashed");

        VideoExportHandle {
            cancel,
            progress,
            done: done_receiver,
        }
    }

    /// Enqueue an input.
    pub fn enqueue_input(&self, input: Input) {
        self.sender.send(ThreadCommand::EnqueueInput(input))
            .expect("EnqueueInput - the core thread has crashed");
    }

    /// Set the speed.
    pub fn set_speed(&self, speed: Speed) {
        self.sender.send(ThreadCommand::SetSpeed(speed))
            .expect("SetSpeed - the core thread has crashed");
    }

    /// Reset the frame-time diagnostics (max, over-budget and measured counts).
    pub fn reset_frame_time_stats(&self) {
        *self.frame_times.write() = FrameTimeStats::default();
    }

    /// Set the speed.
    pub fn hard_reset(&self) {
        self.sender.send(ThreadCommand::HardReset)
            .expect("HardReset - the core thread has crashed");
    }

    /// Set the rapid fire input.
    pub fn set_rapid_fire_input(&self, input: Option<SuperShuckieRapidFire>) {
        self.sender.send(ThreadCommand::SetRapidFireInput(input))
            .expect("SetRapidFireInput - the core thread has crashed");
    }

    /// Set the toggle input.
    pub fn set_toggled_input(&self, input: Option<Input>) {
        self.sender.send(ThreadCommand::SetToggledInput(input))
            .expect("SetToggledInput - the core thread has crashed");
    }

    /// Create a save state.
    ///
    /// Returns `None` if no save state could be created for some unknown reason.
    ///
    /// NOTE: This is blocking.
    pub fn create_save_state(&self) -> Option<Vec<u8>> {
        let (sender, receiver) = channel();
        self.sender.send(ThreadCommand::CreateSaveState(sender))
            .expect("CreateSaveState - the core thread has crashed");
        receiver.recv().ok()
    }

    /// Load a save state.
    pub fn load_save_state(&self, state: Vec<u8>) {
        self.sender.send(ThreadCommand::LoadSaveState(state))
            .expect("LoadSaveState - the core thread has crashed");
    }

    /// Get SRAM.
    ///
    /// Returns `None` if SRAM could not be read for some unknown reason.
    ///
    /// NOTE: This is blocking.
    pub fn get_sram(&self) -> Option<Vec<u8>> {
        let (sender, receiver) = channel();
        self.sender.send(ThreadCommand::SaveSRAM(sender))
            .expect("SaveSRAM - the core thread has crashed");
        receiver.recv().ok()
    }

    /// Get whether or not a replay is being played back.
    #[inline]
    pub fn is_playing_back(&self) -> bool {
        self.playback
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
    pub fn attach_replay_player(&mut self, mut player: ReplayFilePlayer, allow_mismatch: bool) -> Result<(), ReplayPlayerAttachError> {
        player.enable_threading();

        let total_milliseconds = player.get_total_milliseconds();
        let total_frames = player.get_total_frames();

        let (sender, receiver) = channel();

        self.sender.send(ThreadCommand::AttachReplayPlayer {
            player,
            allow_mismatched: allow_mismatch,
            errors: sender
        }).expect("AttachReplayPlayer - the core thread has crashed");

        match receiver.recv() {
            Err(_) => {
                self.playback_total_frames = total_frames;
                self.playback_total_milliseconds = total_milliseconds;
                self.playback = true;
                Ok(())
            },
            Ok(n) => Err(n)
        }
    }

    /// Detach a replay
    pub fn detach_replay_player(&mut self) {
        self.playback_total_frames = 0;
        self.playback_total_milliseconds = 0.into();
        self.playback = false;
        self.sender.send(ThreadCommand::DetachReplayPlayer)
            .expect("DetachReplayPlayer - the core thread has crashed")
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
        self.replay_errors.clear_poison();
        core::mem::take(&mut *self.replay_errors.lock().expect("get_replay_recording_errors fainted due to poison"))
    }

    /// Mark the start of the replay.
    pub fn mark_start(&mut self, timer_offset: TimestampMillis) -> Result<(UnsignedInteger, TimestampMillis), ()> {
        let (sender, receiver) = channel();
        let _ = self.sender.send(ThreadCommand::MarkReplayStart(sender, timer_offset));
        receiver.recv().map_err(|_| ())
    }

    /// Mark the end of the replay.
    pub fn mark_end(&mut self) -> Result<(UnsignedInteger, TimestampMillis), ()> {
        let (sender, receiver) = channel();
        let _ = self.sender.send(ThreadCommand::MarkReplayEnd(sender));
        receiver.recv().map_err(|_| ())
    }

    /// Get the counters.
    #[inline]
    pub fn get_replay_counters(&self) -> BTreeMap<String, SignedInteger> {
        self.replay_counters.lock().expect("couldn't get replay counters (thread crash?)").clone()
    }

    /// Add an amount to a counter.
    #[inline]
    pub fn change_replay_counter(&mut self, name: String, delta: SignedInteger) {
        let _ = self.sender.send(ThreadCommand::ChangeReplayCounter { name, delta });
    }

    /// Set whether or not speed changes from replays are ignored.
    #[inline]
    pub fn set_ignore_speed_changes_in_replay(&self, ignored: bool) {
        let _ = self.sender.send(ThreadCommand::IgnoreSpeedChangesInReplay(ignored));
    }

    /// Set whether or not to resync keyframes in replay playback.
    #[inline]
    pub fn set_auto_resync_keyframes_in_replay(&self, resync: bool) {
        let _ = self.sender.send(ThreadCommand::AutoResyncKeyframesInReplay(resync));
    }

    /// Route the audio of audible frames to `output` (`None` to stop).
    #[inline]
    pub fn set_audio_output(&self, output: Option<Arc<AudioOutput>>) {
        let _ = self.sender.send(ThreadCommand::SetAudioOutput(output));
    }

    /// Turn audio rendering in the core on or off.
    #[inline]
    pub fn set_audio_enabled(&self, enabled: bool) {
        let _ = self.sender.send(ThreadCommand::SetAudioEnabled(enabled));
    }

    /// Discard audio while the game runs at any speed other than 1x.
    #[inline]
    pub fn set_audio_mute_when_sped_up(&self, mute: bool) {
        let _ = self.sender.send(ThreadCommand::SetAudioMuteWhenSpedUp(mute));
    }

    /// Transfer the given Poke-A-Byte integration if it is compatible.
    ///
    /// NOTE: This is blocking.
    pub fn transfer_pokeabyte_integration(&self, to: &ThreadedSuperShuckieCore) -> bool {
        let (sender, receiver) = channel();
        let _ = self.sender.send(ThreadCommand::TransferPokeAByteIntegrationExternal(sender, to.sender.clone()));
        receiver.recv().unwrap_or(false)
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
    pub fn poll_done(&self) -> Option<Result<(), VideoExportError>> {
        self.done.try_recv().ok()
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
    SetPokeAByteEnabled(bool, Sender<Result<(), String>>),
    StartRecordingReplay(PartialReplayRecordMetadata<std::io::BufWriter<File>, std::io::BufWriter<File>>),
    ResumeRecordingReplay {
        resume_at_frame: Option<UnsignedInteger>,
        metadata: PartialReplayRecordMetadata<std::io::BufWriter<File>, std::io::BufWriter<File>>,
        crop_policy: ResumeCropPolicy,
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
        errors: Sender<ReplayPlayerAttachError>
    },
    DetachReplayPlayer,
    EnqueueInput(Input),
    SetRapidFireInput(Option<SuperShuckieRapidFire>),
    SetToggledInput(Option<Input>),
    SetSpeed(Speed),
    HardReset,
    CreateSaveState(Sender<Vec<u8>>),
    LoadSaveState(Vec<u8>),
    SaveSRAM(Sender<Vec<u8>>),
    MarkReplayStart(Sender<(UnsignedInteger, TimestampMillis)>, TimestampMillis),
    MarkReplayEnd(Sender<(UnsignedInteger, TimestampMillis)>),
    Close,
    ChangeReplayCounter { name: String, delta: SignedInteger },
    IgnoreSpeedChangesInReplay(bool),
    AutoResyncKeyframesInReplay(bool),
    SetAudioOutput(Option<Arc<AudioOutput>>),
    SetAudioEnabled(bool),
    SetAudioMuteWhenSpedUp(bool),
    TransferPokeAByteIntegrationExternal(Sender<bool>, Sender<ThreadCommand>),
    TransferPokeAByteIntegrationInternal(Sender<bool>, PokeAByteIntegrationServer, ReplayConsoleType, ReplayHeaderBlake3Hash),
    Rendezvous(Sender<()>),
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

struct ThreadedSuperShuckieCoreThread {
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
    playback_paused: Arc<AtomicBool>,
    playback_frozen: bool,

    core: SuperShuckieCore,
    receiver: Receiver<ThreadCommand>,
    pokeabyte_integration: Option<PokeAByteIntegrationServer>,
    sender_close: Sender<()>,
    is_null: bool,

    elapsed_time: Arc<RwLock<ElapsedTimeStats>>,
    frame_times: Arc<RwLock<FrameTimeStats>>,
    emulated_frames: Arc<AtomicU64>,

    freezes: BTreeMap<u64, ByteVec>,
    replay_stalled: Arc<AtomicBool>,
}

impl ThreadedSuperShuckieCoreThread {
    fn run_thread(mut self) {
        loop {
            if let Ok(cmd) = self.receiver.try_recv() {
                if matches!(cmd, ThreadCommand::Close) {
                    break
                }

                self.handle_command(cmd);
                // counters can change without a frame running (REST while paused, seeks)
                self.update_counters();
                continue
            }

            self.handle_replay_recording_errors();
            self.go_to_desired_frame();
            self.refresh_screen_data();
            self.update_queued_screens();
            self.handle_pokeabyte_integration();
            self.check_if_replay_stalled();

            if self.is_running() {
                if !self.playback_frozen {
                    self.run_one();
                }
            }
            else if self.core.replay_player.is_none() {
                // unfortunately we can't just block until we're running again because we still need
                // to handle pokeabyte writes
                std::thread::sleep(Duration::from_millis(100));
            }
            else {
                // sleep for a reduced time so seeking can still be responsive
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        self.core.stop_recording_replay();
        self.pokeabyte_integration = None;

        let _ = self.sender_close.send(());
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

        if self.core.replay_stalled || self.core.mid_frame && self.core.core.frame_period_microseconds().is_none() {
            // Not a pacing wait (Game Boy mid-frame stepping, or nothing to run): keep going.
            return;
        }

        if let Some(until) = self.core.core.microseconds_until_next_frame() {
            let until = Duration::from_micros(until);
            if until > Self::WAKE_EARLY {
                std::thread::sleep((until - Self::WAKE_EARLY).min(Self::MAX_FRAME_WAIT));
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
        if frame != u32::MAX {
            self.core.go_to_replay_frame(frame as UnsignedInteger);
        }
        else if delta != 0 {
            self.core.go_to_replay_frame(self.core.total_frames.saturating_add_signed(delta as i64));
        }
        else {
            return
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
        if self.is_running() && self.core.mid_frame {
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

        self.core.force_stop_recording_replay();
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
                    self.core.enqueue_write(address as u32, data);
                },
                PokeAByteEmulatorCommand::Freeze { address, data } => {
                    self.core.enqueue_write(address as u32, data.clone());
                    self.freezes.insert(address, data);
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
        if self.core.mid_frame && is_running {
            return;
        }

        // apply freezes immediately regardless of frame skipping setting
        if is_running {
            for (address, data) in &self.freezes {
                self.core.enqueue_write(*address as u32, data.clone());
            }
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

    fn handle_command(&mut self, command: ThreadCommand) {
        match command {
            ThreadCommand::Start(sender) => {
                if self.playback_paused.swap(false, Ordering::Relaxed) {
                    if self.core.replay_stalled {
                        self.core.go_to_replay_frame(0);
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
            ThreadCommand::SetPokeAByteEnabled(enabled, err) => {
                if !enabled && self.pokeabyte_integration.is_some() {
                    self.pokeabyte_integration = None;
                    let _ = err.send(Ok(()));
                }
                else if enabled {
                    let integration = match PokeAByteIntegrationServer::begin_listen() {
                        Ok(n) => {
                            let _ = err.send(Ok(()));
                            n
                        },
                        Err(e) => {
                            let _ = err.send(Err(format!("{e:?}")));
                            return
                        }
                    };
                    self.pokeabyte_integration = Some(integration)
                } else {
                    let _ = err.send(Ok(()));
                }
            }
            ThreadCommand::StartRecordingReplay(metadata) => {
                self.replay_errors.lock().expect("start recording replay failed to get replay errors").clear();

                // FIXME: error if this fails
                self.core.start_recording_replay(metadata).expect("FAILED TO START RECORDING REPLAY OH NO");
                if !self.is_running() {
                    self.core.pause_timer();
                }
            }
            ThreadCommand::ResumeRecordingReplay { resume_at_frame, metadata, crop_policy } => {
                self.replay_errors.lock().expect("resume recording replay failed to get replay errors").clear();

                // FIXME: error if this fails
                self.core.resume_recording_replay(resume_at_frame, metadata, crop_policy).expect("FAILED TO RESUME RECORDING REPLAY OH NO");
                if !self.is_running() {
                    self.core.pause_timer();
                }
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
            }
            ThreadCommand::SaveSRAM(sender) => {
                let _ = sender.send(self.core.save_sram());
            }
            ThreadCommand::Close => {
                unreachable!("handle_command(ThreadCommand::Close) should not happen")
            },
            ThreadCommand::AttachReplayPlayer { player, allow_mismatched, errors } => {
                if let Err(e) = self.core.attach_replay_player(player, allow_mismatched) {
                    let _ = errors.send(e);
                }
                if !self.is_running() {
                    self.core.pause_timer();
                }
            }
            ThreadCommand::DetachReplayPlayer => {
                self.core.detach_replay_player();
            }
            ThreadCommand::MarkReplayStart(timestamp, timer_offset) => {
                if let Some(n) = self.core.mark_start(timer_offset) {
                    let _ = timestamp.send(n);
                }
            }
            ThreadCommand::MarkReplayEnd(timestamp) => {
                if let Some(n) = self.core.mark_end() {
                    let _ = timestamp.send(n);
                }
            }
            ThreadCommand::ChangeReplayCounter { name, delta } => {
                self.core.change_replay_counter(name, delta);
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
                let _ = core_sender.send(ThreadCommand::TransferPokeAByteIntegrationInternal(sender, server, replay_console_type, rom_checksum));
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
