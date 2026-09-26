use super::{KeyframeEncoding, ReplayFileWriteError, ReplayFileRecorder, ReplayFileSink, ReplayFileRecorderFns};
use crate::{BookmarkTable, ByteVec, InputBuffer, SignedInteger, Speed, TimestampMillis, UnsignedInteger};
use alloc::borrow::Cow;
use alloc::borrow::ToOwned;
use alloc::string::String;
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, SyncSender};
use std::sync::Mutex;
use std::sync::{Arc, Weak};
use alloc::vec::Vec;
use std::time::Duration;

/// How many displaced keyframe state buffers the recorder thread will hold onto for the producer
/// to reclaim before it starts dropping them. Bounds the channel's own memory use if the producer
/// stops draining it (each buffer can be tens of MiB for a large console's save state) -- unlike an
/// unbounded channel, which would otherwise grow by one buffer per keyframe forever.
const FREE_BUFFER_CHANNEL_CAPACITY: usize = 4;

type RecorderMutex<Final, Temp> = Mutex<ReplayFileRecorder<Final, Temp>>;

/// File recorder that records in a separate thread and is non-blocking.
///
/// The `std` feature is required to use this.
pub struct NonBlockingReplayFileRecorder<Final: ReplayFileSink + Send + 'static, Temp: ReplayFileSink + Send + 'static> {
    recorder: Option<Arc<RecorderMutex<Final, Temp>>>,

    sender: Sender<ThreadedReplayFileRecorderCommand>,
    errors: Receiver<ReplayFileWriteError>,
    free_buffers: Receiver<Vec<u8>>,
    closed: Receiver<()>
}

impl<Final: ReplayFileSink + Send + 'static, Temp: ReplayFileSink + Send + 'static> NonBlockingReplayFileRecorder<Final, Temp> {
    /// Instantiate a non-blocking replay recorder.
    pub fn new(recorder: ReplayFileRecorder<Final, Temp>) -> NonBlockingReplayFileRecorder<Final, Temp> {
        Self::spawn(recorder, false)
    }

    /// Like [`Self::new`], but the writer thread is scheduled below the process's normal
    /// priority (on Windows; elsewhere the same as `new`). For recordings nobody is waiting on
    /// frame by frame, such as a friend's game being followed in Play Together: its keyframe
    /// compression then never takes a core from the player's own game or display.
    pub fn new_background(recorder: ReplayFileRecorder<Final, Temp>) -> NonBlockingReplayFileRecorder<Final, Temp> {
        Self::spawn(recorder, true)
    }

    fn spawn(recorder: ReplayFileRecorder<Final, Temp>, background: bool) -> NonBlockingReplayFileRecorder<Final, Temp> {
        let recorder = Arc::new(Mutex::new(recorder));

        let (sender_main, receiver_helper) = channel();
        let (sender_helper, receiver_main) = channel();
        let (closed_helper, closed_main) = channel();
        let (free_sender, free_receiver) = sync_channel(FREE_BUFFER_CHANNEL_CAPACITY);

        let helper = ThreadedReplayFileRecorderThread {
            recorder: Arc::downgrade(&recorder),
            error_sender: sender_helper,
            receiver: receiver_helper,
            free_buffers: free_sender,
            closed: closed_helper
        };

        std::thread::Builder::new()
            .name("ThreadedReplayFileRecorderThread".to_owned())
            .spawn(move || {
                if background {
                    lower_current_thread_priority();
                }
                helper.run();
            })
            .expect("failed to start a thread...");

        Self {
            sender: sender_main,
            errors: receiver_main,
            free_buffers: free_receiver,
            recorder: Some(recorder),
            closed: closed_main
        }
    }

    /// A keyframe state buffer the recorder thread has finished with, if one is waiting.
    pub fn take_free_state_buffer(&mut self) -> Option<Vec<u8>> {
        self.free_buffers.try_recv().ok()
    }

    /// Test-only: the shared recorder mutex, so a test can poison it (simulating a panic while it
    /// was held) to check that the recorder thread and [`Self::close`] both recover from that
    /// instead of panicking themselves.
    #[cfg(test)]
    fn recorder_mutex_for_test(&self) -> Arc<RecorderMutex<Final, Temp>> {
        self.recorder.clone().expect("not yet closed")
    }

    /// Return `true` if the recorder was already closed.
    #[inline]
    pub fn is_closed(&self) -> bool {
        self.recorder.is_none()
    }

    /// Close the replay file recorder, blocking until closed.
    ///
    /// # Panics
    ///
    /// Panics if already closed.
    pub fn close(&mut self) -> Result<(Final, Temp), (Final, Temp, ReplayFileWriteError)> {
        // Close it
        let _ = self.sender.send(ThreadedReplayFileRecorderCommand::Close);

        // Wait for it to be fully closed (thus all writes are processed, etc.)
        let _ = self.closed.recv();

        // If the other thread is busy, we'll need to spin here until it's done.
        let mut a = self.recorder.take().expect("recorder already closed");
        let recorder = loop {
            match Arc::try_unwrap(a) {
                Ok(n) => break n,
                Err(e) => a = e
            }
            std::thread::sleep(Duration::from_millis(25));
        };

        // If the recorder thread panicked while holding the lock, the mutex is poisoned; recover
        // the (possibly inconsistent, but still usable -- writes are append-only) recorder rather
        // than panicking here too.
        let mut recorder = recorder.into_inner().unwrap_or_else(|poisoned| poisoned.into_inner());

        // Done.
        recorder.close()
    }

    /// Advance a new frame.
    pub fn next_frame(&mut self, timestamp: TimestampMillis) {
        let _ = self.sender.send(ThreadedReplayFileRecorderCommand::NextFrame { timestamp });
    }

    /// Replace the replay's bookmarks (see [`ReplayFileRecorder::set_bookmark_table`]).
    pub fn set_bookmark_table(&mut self, table: BookmarkTable) {
        let _ = self.sender.send(ThreadedReplayFileRecorderCommand::SetBookmarkTable { table });
    }

    /// Add a new keyframe.
    pub fn insert_keyframe(&mut self, state: ByteVec, timestamp: TimestampMillis) {
        let _ = self.sender.send(ThreadedReplayFileRecorderCommand::NewKeyframe { state, timestamp, encoding: KeyframeEncoding::Auto });
    }

    /// Add a new keyframe that is always stored in full (see [`KeyframeEncoding::Full`]).
    pub fn insert_keyframe_full(&mut self, state: ByteVec, timestamp: TimestampMillis) {
        let _ = self.sender.send(ThreadedReplayFileRecorderCommand::NewKeyframe { state, timestamp, encoding: KeyframeEncoding::Full });
    }

    /// Set the current input.
    pub fn set_input(&mut self, input: InputBuffer) {
        let _ = self.sender.send(ThreadedReplayFileRecorderCommand::SetInput { input });
    }

    /// Hard-reset the console.
    pub fn reset_console(&mut self) {
        let _ = self.sender.send(ThreadedReplayFileRecorderCommand::ResetConsole);
    }

    /// Write RAM to an address.
    pub fn write_memory(&mut self, address: UnsignedInteger, data: ByteVec) {
        let _ = self.sender.send(ThreadedReplayFileRecorderCommand::WriteMemory { address, data });
    }

    /// Set the current speed.
    pub fn set_speed(&mut self, speed: Speed) {
        let _ = self.sender.send(ThreadedReplayFileRecorderCommand::SetSpeed { speed });
    }

    /// Load the keyframe at the given frame index.
    pub fn load_save_state(&mut self, state: ByteVec) {
        let _ = self.sender.send(ThreadedReplayFileRecorderCommand::LoadSaveState { state });
    }

    /// What the console received over its link cable this frame (see
    /// [`ReplayFileRecorder::serial_in`]).
    pub fn serial_in(&mut self, data: ByteVec) {
        if data.is_empty() {
            return
        }
        let _ = self.sender.send(ThreadedReplayFileRecorderCommand::SerialIn { data });
    }

    /// Store a timeline picture (see [`ReplayFileRecorder::thumbnail`]).
    pub fn thumbnail(&mut self, top: (u32, u32, Vec<u8>), bottom: (u32, u32, Vec<u8>)) {
        let _ = self.sender.send(ThreadedReplayFileRecorderCommand::Thumbnail { top, bottom });
    }

    /// Check for errors, if any.
    pub fn poll_errors(&mut self) -> Vec<ReplayFileWriteError> {
        let mut errors = Vec::new();
        
        while let Ok(error) = self.errors.try_recv() {
            errors.push(error);
        }
        
        errors
    }

    /// Mark the start of the replay.
    pub fn mark_start(&mut self, timer_offset: TimestampMillis) {
        let _ = self.sender.send(ThreadedReplayFileRecorderCommand::MarkStart { timer_offset });
    }

    /// Mark the end of the replay.
    pub fn mark_end(&mut self) {
        let _ = self.sender.send(ThreadedReplayFileRecorderCommand::MarkEnd);
    }

    /// Change the counter.
    pub fn change_counter(&mut self, name: String, delta: SignedInteger) {
        let _ = self.sender.send(ThreadedReplayFileRecorderCommand::IncrementCounter { name, delta });
    }
}

/// Schedule the calling thread below the process's normal priority (see
/// [`NonBlockingReplayFileRecorder::new_background`]).
#[cfg(windows)]
fn lower_current_thread_priority() {
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

#[cfg(not(windows))]
fn lower_current_thread_priority() {}

struct ThreadedReplayFileRecorderThread<Final: ReplayFileSink, Temp: ReplayFileSink> {
    recorder: Weak<RecorderMutex<Final, Temp>>,

    // note: the success of sending will never be checked; we do not care because this thread will
    // eventually be closed if it fails
    error_sender: Sender<ReplayFileWriteError>,
    receiver: Receiver<ThreadedReplayFileRecorderCommand>,
    /// Displaced keyframe state buffers go back to the producer through here. Bounded (see
    /// [`FREE_BUFFER_CHANNEL_CAPACITY`]): if the producer stops draining it, buffers are dropped
    /// instead of piling up unboundedly.
    free_buffers: SyncSender<Vec<u8>>,
    closed: Sender<()>
}

/// Reports, via `errors`, that the recorder thread stopped without a clean `Close` command -- a
/// panic (leaving the mutex poisoned, though still recovered by [`NonBlockingReplayFileRecorder::close`])
/// or the recorder being dropped out from under the thread. Without this, such a stop is silent:
/// nothing more is ever written, but nothing says so either.
struct ExitNotice {
    errors: Sender<ReplayFileWriteError>,
    /// Set only on the `Close` command path; every other way out of the loop leaves this `false`.
    clean: bool
}

impl Drop for ExitNotice {
    fn drop(&mut self) {
        if !self.clean {
            let _ = self.errors.send(ReplayFileWriteError::Other {
                explanation: Cow::Borrowed("the replay recorder thread stopped unexpectedly (poisoned lock or panic); the recording is no longer being written")
            });
        }
    }
}

impl<Final: ReplayFileSink, Temp: ReplayFileSink> ThreadedReplayFileRecorderThread<Final, Temp> {
    fn run(mut self) {
        let mut exit_notice = ExitNotice { errors: self.error_sender.clone(), clean: false };

        loop {
            // If any of these fails, abort the thread.
            let Ok(command) = self.receiver.recv() else {
                break
            };
            if matches!(command, ThreadedReplayFileRecorderCommand::Close) {
                exit_notice.clean = true;
                break
            }
            let Some(recorder) = self.recorder.upgrade() else {
                break
            };
            // Recover a poisoned lock (the recorder itself panicked while holding it) instead of
            // silently exiting the thread: append-only writes stay valid even after a panic
            // mid-write, and this lets the thread keep serving commands.
            let mut recorder = recorder.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

            if let Err(e) = self.handle_command(command, &mut recorder) {
                let _ = self.error_sender.send(e);
            }
        }

        drop(exit_notice);
        let _ = self.closed.send(());
    }

    fn handle_command(&mut self, command: ThreadedReplayFileRecorderCommand, recorder: &mut ReplayFileRecorder<Final, Temp>) -> Result<(), ReplayFileWriteError> {
        match command {
            ThreadedReplayFileRecorderCommand::Close => Ok(()),
            ThreadedReplayFileRecorderCommand::WriteMemory { address, data } => {
                recorder.write_memory(address, data)
            },
            ThreadedReplayFileRecorderCommand::NewKeyframe { timestamp, state, encoding } => {
                let result = recorder.insert_keyframe_with(state, timestamp, encoding);
                if let Some(buffer) = recorder.take_recycled_state() {
                    // If the producer isn't draining these (channel full), drop the buffer rather
                    // than growing the channel without bound.
                    let _ = self.free_buffers.try_send(buffer);
                }
                result.map(|_| ())
            }
            ThreadedReplayFileRecorderCommand::SetInput { input } => {
                recorder.set_input(input)
            },
            ThreadedReplayFileRecorderCommand::SetSpeed { speed } => {
                recorder.set_speed(speed)
            },
            ThreadedReplayFileRecorderCommand::SetBookmarkTable { table } => {
                recorder.set_bookmark_table(table)
            },
            ThreadedReplayFileRecorderCommand::ResetConsole => {
                recorder.reset_console()
            },
            ThreadedReplayFileRecorderCommand::NextFrame { timestamp } => {
                recorder.next_frame(timestamp)
            },
            ThreadedReplayFileRecorderCommand::LoadSaveState { state } => {
                recorder.load_save_state(state)
            }
            ThreadedReplayFileRecorderCommand::SerialIn { data } => {
                recorder.serial_in(data)
            }
            ThreadedReplayFileRecorderCommand::Thumbnail { top, bottom } => {
                recorder.thumbnail((top.0, top.1, top.2.as_slice()), (bottom.0, bottom.1, bottom.2.as_slice()))
            }
            ThreadedReplayFileRecorderCommand::MarkStart { timer_offset } => {
                recorder.mark_start(timer_offset)
            }
            ThreadedReplayFileRecorderCommand::MarkEnd => {
                recorder.mark_end()
            }
            ThreadedReplayFileRecorderCommand::IncrementCounter { name, delta } => {
                recorder.change_counter(name, delta)
            }
        }
    }
}

enum ThreadedReplayFileRecorderCommand {
    NextFrame { timestamp: TimestampMillis },
    SetBookmarkTable { table: BookmarkTable },
    NewKeyframe { state: ByteVec, timestamp: TimestampMillis, encoding: KeyframeEncoding },
    SetInput { input: InputBuffer },
    SetSpeed { speed: Speed },
    WriteMemory { address: UnsignedInteger, data: ByteVec },
    LoadSaveState { state: ByteVec },
    SerialIn { data: ByteVec },
    Thumbnail { top: (u32, u32, Vec<u8>), bottom: (u32, u32, Vec<u8>) },
    IncrementCounter { name: String, delta: SignedInteger },
    MarkStart { timer_offset: TimestampMillis },
    MarkEnd,
    ResetConsole,
    Close
}

impl<Final: ReplayFileSink + Sync + Send + 'static, Temp: ReplayFileSink + Sync + Send + 'static> ReplayFileRecorderFns for NonBlockingReplayFileRecorder<Final, Temp> {
    #[inline]
    fn is_closed(&self) -> bool {
        self.is_closed()
    }

    #[inline]
    fn close(&mut self) -> Result<(), ReplayFileWriteError> {
        self.close().map_err(|e| e.2)?;
        Ok(())
    }

    #[inline]
    fn next_frame(&mut self, timestamp_millis: TimestampMillis) -> Result<(), ReplayFileWriteError> {
        self.next_frame(timestamp_millis);
        Ok(())
    }

    #[inline]
    fn set_bookmark_table(&mut self, table: BookmarkTable) -> Result<(), ReplayFileWriteError> {
        self.set_bookmark_table(table);
        Ok(())
    }

    #[inline]
    fn insert_keyframe(&mut self, state: ByteVec, timestamp: TimestampMillis) -> Result<(), ReplayFileWriteError> {
        self.insert_keyframe(state, timestamp);
        Ok(())
    }

    #[inline]
    fn insert_keyframe_full(&mut self, state: ByteVec, timestamp: TimestampMillis) -> Result<(), ReplayFileWriteError> {
        self.insert_keyframe_full(state, timestamp);
        Ok(())
    }

    #[inline]
    fn set_input(&mut self, input_buffer: InputBuffer) -> Result<(), ReplayFileWriteError> {
        self.set_input(input_buffer);
        Ok(())
    }

    #[inline]
    fn reset_console(&mut self) -> Result<(), ReplayFileWriteError> {
        self.reset_console();
        Ok(())
    }

    #[inline]
    fn write_memory(&mut self, address: UnsignedInteger, data: ByteVec) -> Result<(), ReplayFileWriteError> {
        self.write_memory(address, data);
        Ok(())
    }

    #[inline]
    fn set_speed(&mut self, speed: Speed) -> Result<(), ReplayFileWriteError> {
        self.set_speed(speed);
        Ok(())
    }

    #[inline]
    fn load_save_state(&mut self, state: ByteVec) -> Result<(), ReplayFileWriteError> {
        self.load_save_state(state);
        Ok(())
    }

    #[inline]
    fn serial_in(&mut self, data: ByteVec) -> Result<(), ReplayFileWriteError> {
        self.serial_in(data);
        Ok(())
    }

    fn thumbnail(&mut self, top: (u32, u32, Vec<u8>), bottom: (u32, u32, Vec<u8>)) -> Result<(), ReplayFileWriteError> {
        self.thumbnail(top, bottom);
        Ok(())
    }

    #[inline]
    fn get_errors(&mut self) -> Vec<ReplayFileWriteError> {
        self.poll_errors()
    }

    #[inline]
    fn mark_start(&mut self, timer_offset: TimestampMillis) -> Result<(), ReplayFileWriteError> {
        self.mark_start(timer_offset);
        Ok(())
    }

    #[inline]
    fn mark_end(&mut self) -> Result<(), ReplayFileWriteError> {
        self.mark_end();
        Ok(())
    }

    #[inline]
    fn change_counter(&mut self, counter: String, delta: SignedInteger) -> Result<(), ReplayFileWriteError> {
        self.change_counter(counter, delta);
        Ok(())
    }

    #[inline]
    fn take_free_state_buffer(&mut self) -> Option<Vec<u8>> {
        self.take_free_state_buffer()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay_file::ReplayHeaderBytes;
    use crate::test_support::*;
    use crate::{ByteVec, PacketWriteCommand, Speed};
    use alloc::boxed::Box;

    fn settings() -> crate::replay_file::record::ReplayFileRecorderSettings {
        crate::replay_file::record::ReplayFileRecorderSettings {
            minimum_uncompressed_bytes_per_blob: usize::MAX,
            max_frames_per_blob: 0,
            compression_level: crate::replay_file::record::DEFAULT_ZSTD_COMPRESSION_LEVEL_V4,
            mask_transient_buffers: true,
            stored_keyframe_levels: (15, 15),
            stored_keyframe_compression_level: 3,
        }
    }

    /// Polls `poll` (typically `nb.poll_errors`) until it returns something or `timeout` elapses.
    fn wait_for_errors(mut poll: impl FnMut() -> Vec<ReplayFileWriteError>, timeout: Duration) -> Vec<ReplayFileWriteError> {
        let start = std::time::Instant::now();
        loop {
            let errors = poll();
            if !errors.is_empty() || start.elapsed() > timeout {
                return errors;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Runs `f` with the default panic hook replaced by a no-op, so a deliberately triggered panic
    /// (in a spawned thread, or one this test expects the recorder thread to take) does not spam
    /// the test output with a panic backtrace.
    fn silencing_panics<T>(f: impl FnOnce() -> T) -> T {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result = f();
        std::panic::set_hook(previous);
        result
    }

    /// A temp sink that fails every write from its `fail_from`-th call (1-based) onward.
    #[derive(Clone)]
    struct FailingTempSink {
        calls: Arc<Mutex<usize>>,
        fail_from: usize,
    }

    impl FailingTempSink {
        fn new(fail_from: usize) -> Self {
            Self { calls: Arc::new(Mutex::new(0)), fail_from }
        }

        /// Bumps the call counter; returns `true` if this call should fail.
        fn bump(&self) -> bool {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            *calls >= self.fail_from
        }
    }

    impl ReplayFileSink for FailingTempSink {
        fn write_bytes(&mut self, _bytes: &[u8]) -> Result<(), ReplayFileWriteError> {
            if self.bump() {
                return Err(ReplayFileWriteError::Other { explanation: Cow::Borrowed("injected temp failure") });
            }
            Ok(())
        }
        fn truncate(&mut self, _size: u64) -> Result<(), ReplayFileWriteError> {
            Ok(())
        }
        fn overwrite_header(&mut self, _header_data: &ReplayHeaderBytes) -> Result<(), ReplayFileWriteError> {
            Ok(())
        }
        fn write_packet_data(&mut self, instructions: &[PacketWriteCommand<'_>]) -> Result<usize, ReplayFileWriteError> {
            if self.bump() {
                return Err(ReplayFileWriteError::Other { explanation: Cow::Borrowed("injected temp failure") });
            }
            Ok(instructions.iter().map(|i| i.bytes().len()).sum())
        }
    }

    /// A temp sink that panics (rather than erroring) on its `panic_from`-th write call (1-based),
    /// simulating the recorder thread dying instead of a write merely failing.
    #[derive(Clone)]
    struct PanicOnWriteSink {
        calls: Arc<Mutex<usize>>,
        panic_from: usize,
    }

    impl PanicOnWriteSink {
        fn new(panic_from: usize) -> Self {
            Self { calls: Arc::new(Mutex::new(0)), panic_from }
        }

        fn bump(&self) {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            if *calls >= self.panic_from {
                panic!("PanicOnWriteSink: simulated recorder thread panic");
            }
        }
    }

    impl ReplayFileSink for PanicOnWriteSink {
        fn write_bytes(&mut self, _bytes: &[u8]) -> Result<(), ReplayFileWriteError> {
            self.bump();
            Ok(())
        }
        fn truncate(&mut self, _size: u64) -> Result<(), ReplayFileWriteError> {
            Ok(())
        }
        fn overwrite_header(&mut self, _header_data: &ReplayHeaderBytes) -> Result<(), ReplayFileWriteError> {
            Ok(())
        }
        fn write_packet_data(&mut self, instructions: &[PacketWriteCommand<'_>]) -> Result<usize, ReplayFileWriteError> {
            self.bump();
            Ok(instructions.iter().map(|i| i.bytes().len()).sum())
        }
    }

    fn make_recorder<Temp: ReplayFileSink + Send + 'static>(temp: Temp) -> ReplayFileRecorder<Vec<u8>, Temp> {
        ReplayFileRecorder::new_with_metadata(
            make_metadata(),
            ByteVec::new(),
            settings(),
            0u64.into(),
            ib(&[0]),
            Speed::default(),
            bv(&state_for(0)),
            Vec::<u8>::new(),
            temp,
        )
        .unwrap()
    }

    /// A mutex poisoned by an unrelated panic (simulating the recorder thread having panicked at
    /// some earlier point while holding it) must not stop the recorder thread from continuing to
    /// process commands, nor stop `close()` from finishing normally.
    #[test]
    fn close_recovers_a_poisoned_recorder_mutex() {
        let mut nb = NonBlockingReplayFileRecorder::new(make_recorder(SharedSink::default()));
        let mutex = nb.recorder_mutex_for_test();

        silencing_panics(|| {
            let _ = std::thread::spawn(move || {
                let _guard = mutex.lock().unwrap();
                panic!("intentionally poisoning the mutex for the test");
            })
            .join();
        });

        // The background thread must still be able to process a command through the now-poisoned
        // mutex, and close() must still finish (and succeed: nothing else about the recording is
        // broken -- only the mutex's poison flag was set).
        nb.next_frame(16u64.into());
        let result = nb.close();
        assert!(result.is_ok(), "{:?}", result.err().map(|(_, _, e)| e));
    }

    /// A temp-sink failure reaches the caller through `poll_errors()` (asynchronously, since the
    /// write happens on the recorder thread), and does not stop `close()` from succeeding.
    #[test]
    fn errors_are_polled() {
        // Calls on the temp sink so far once construction finishes: #1 header, #2 (empty) patch,
        // #3 the frame-0 keyframe. #4 is the first `next_frame`, which is where the injected
        // failure starts.
        let mut nb = NonBlockingReplayFileRecorder::new(make_recorder(FailingTempSink::new(4)));

        nb.next_frame(16u64.into());

        let errors = wait_for_errors(|| nb.poll_errors(), Duration::from_secs(2));
        assert!(errors.iter().any(|e| matches!(e, ReplayFileWriteError::TempSink { .. })), "{errors:?}");

        let result = nb.close();
        assert!(result.is_ok(), "{:?}", result.err().map(|(_, _, e)| e));
    }

    /// If the recorder thread itself dies (here, a write panics instead of merely failing) rather
    /// than exiting cleanly through `Close`, that is reported through `poll_errors()` instead of
    /// being silent.
    #[test]
    fn thread_death_is_reported() {
        let mut nb = NonBlockingReplayFileRecorder::new(make_recorder(PanicOnWriteSink::new(4)));

        let errors = silencing_panics(|| {
            nb.next_frame(16u64.into());
            wait_for_errors(|| nb.poll_errors(), Duration::from_secs(2))
        });

        assert!(
            errors.iter().any(|e| matches!(e, ReplayFileWriteError::Other { explanation } if explanation.contains("stopped unexpectedly"))),
            "{errors:?}"
        );
    }
}
