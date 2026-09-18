//! A replay that arrives while it is played: the follower side of Play Together.
//!
//! A network reader pushes a publisher's packets into a [`LiveReplayFeeder`]; the follower's
//! [`SuperShuckieCore`](crate::SuperShuckieCore) pulls them out of the matching
//! [`LiveReplaySource`] exactly as it reads a replay file, one frame's worth per emulated frame.
//! The two halves share one bounded queue.
//!
//! Snapshots (a [`Packet::Keyframe`] with the state present) supersede everything queued before
//! them: a follower that fell behind jumps rather than replays the backlog. Before the first
//! snapshot every packet is dropped, since there is no state to apply them to.

use crate::stream::SnapshotRequestReason;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::Thread;
use std::time::{Duration, Instant};
use std::vec::Vec;
use supershuckie_replay_recorder::Packet;

/// Frames a feeder holds before it gives up on the backlog and asks for a snapshot instead
/// (20 s at 60 fps, 5 s at 4x).
pub const MAX_QUEUED_FRAMES: u64 = 1200;

/// Shortest time between two snapshot requests from one follower, unless a snapshot arrived in
/// between.
pub const SNAPSHOT_REQUEST_INTERVAL: Duration = Duration::from_secs(2);

/// One thing a follower reads from its publisher, in order.
#[derive(Clone, Debug, PartialEq)]
pub enum LiveItem {
    /// A replay packet (a `Keyframe` here is a snapshot with the state present).
    Packet(Packet),

    /// The publisher's work-RAM hash after `frame` (see [`crate::stream::SYNC_HASH_INTERVAL_FRAMES`]).
    SyncHash {
        /// The frame the hash was taken after.
        frame: u64,
        /// blake3 of the work RAM.
        hash: [u8; 32]
    }
}

/// What [`LiveReplaySource::pop`] found.
pub(crate) enum LivePoll {
    /// The next item.
    Item(LiveItem),

    /// Nothing has arrived yet; the publisher is still going.
    Waiting,

    /// The publisher is gone for good.
    Ended
}

/// Counters a follower core keeps up to date for whoever shows its status.
#[derive(Default, Debug)]
pub struct FollowerStats {
    /// Frames the follower is behind the newest frame the publisher sent.
    pub frames_behind: AtomicU64,

    /// The newest publisher frame number received.
    pub newest_publisher_frame: AtomicU64,

    /// Whether the follower is idle for lack of data (the publisher paused, or the network is
    /// slow), as opposed to keeping up.
    pub waiting: AtomicBool,

    /// Snapshots applied so far (the join counts as one).
    pub snapshots_applied: AtomicU64,

    /// Sync hashes that did not match the publisher's.
    pub hash_mismatches: AtomicU64,

    /// Snapshot requests sent upstream.
    pub snapshot_requests: AtomicU64,

    /// Frames emulated.
    pub emulated_frames: AtomicU64,

    /// Frames drawn.
    pub drawn_frames: AtomicU64
}

/// A plain copy of [`FollowerStats`] at one moment.
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct FollowerStatsSnapshot {
    pub frames_behind: u64,
    pub newest_publisher_frame: u64,
    pub waiting: bool,
    pub snapshots_applied: u64,
    pub hash_mismatches: u64,
    pub snapshot_requests: u64,
    pub emulated_frames: u64,
    pub drawn_frames: u64
}

impl FollowerStats {
    /// Copy every counter.
    pub fn snapshot(&self) -> FollowerStatsSnapshot {
        FollowerStatsSnapshot {
            frames_behind: self.frames_behind.load(Ordering::Relaxed),
            newest_publisher_frame: self.newest_publisher_frame.load(Ordering::Relaxed),
            waiting: self.waiting.load(Ordering::Relaxed),
            snapshots_applied: self.snapshots_applied.load(Ordering::Relaxed),
            hash_mismatches: self.hash_mismatches.load(Ordering::Relaxed),
            snapshot_requests: self.snapshot_requests.load(Ordering::Relaxed),
            emulated_frames: self.emulated_frames.load(Ordering::Relaxed),
            drawn_frames: self.drawn_frames.load(Ordering::Relaxed)
        }
    }
}

struct Queue {
    items: VecDeque<LiveItem>,

    /// `NextFrame` items in `items`.
    frames_queued: u64,

    /// The publisher's frame number the newest queued `NextFrame` completes (the frame count
    /// after it): the last snapshot's frame plus the `NextFrame`s pushed since.
    newest_frame: u64,

    /// Whether a snapshot has ever been queued (nothing before one is worth keeping).
    have_snapshot: bool,

    /// Snapshot requests are held back until this instant unless a snapshot arrives first.
    request_allowed_at: Option<Instant>,

    /// The core thread, to wake it when something arrives.
    waker: Option<Thread>
}

struct Shared {
    queue: Mutex<Queue>,
    ended: AtomicBool,
    stats: Arc<FollowerStats>,
    upstream: Sender<SnapshotRequestReason>
}

impl Shared {
    fn request_snapshot(&self, queue: &mut Queue, reason: SnapshotRequestReason, now: Instant) {
        if queue.request_allowed_at.is_some_and(|t| now < t) {
            return
        }
        queue.request_allowed_at = Some(now + SNAPSHOT_REQUEST_INTERVAL);
        self.stats.snapshot_requests.fetch_add(1, Ordering::Relaxed);
        let _ = self.upstream.send(reason);
    }
}

/// The producer half: a network reader pushes what the publisher sent.
pub struct LiveReplayFeeder(Arc<Shared>);

/// The consumer half: the follower core reads from it.
pub struct LiveReplaySource(Arc<Shared>);

/// Make a connected feeder/source pair. `stats` is kept up to date by both halves; snapshot
/// requests (rate-limited) go out through `upstream` for the owner to forward to the publisher.
pub fn live_replay_channel(stats: Arc<FollowerStats>, upstream: Sender<SnapshotRequestReason>) -> (LiveReplayFeeder, LiveReplaySource) {
    let shared = Arc::new(Shared {
        queue: Mutex::new(Queue {
            items: VecDeque::new(),
            frames_queued: 0,
            newest_frame: 0,
            have_snapshot: false,
            request_allowed_at: None,
            waker: None
        }),
        ended: AtomicBool::new(false),
        stats,
        upstream
    });
    (LiveReplayFeeder(shared.clone()), LiveReplaySource(shared))
}

impl LiveReplayFeeder {
    /// Queue a packet. A `Keyframe` (a snapshot) clears whatever was queued before it; before
    /// the first snapshot everything else is dropped.
    pub fn push_packet(&self, packet: Packet) {
        let mut queue = self.0.queue.lock().unwrap_or_else(|p| p.into_inner());
        match &packet {
            Packet::Keyframe { metadata, .. } => {
                queue.items.clear();
                queue.frames_queued = 0;
                queue.newest_frame = metadata.elapsed_frames;
                queue.have_snapshot = true;
                // The snapshot answers whatever request was outstanding.
                queue.request_allowed_at = None;
            }
            _ if !queue.have_snapshot => return,
            Packet::NextFrame { .. } => {
                if queue.frames_queued >= MAX_QUEUED_FRAMES {
                    // Far too much to catch up on: drop it all and ask for a fresh state. What is
                    // dropped ends mid-frame at worst, and the snapshot replaces all of it.
                    queue.items.clear();
                    queue.frames_queued = 0;
                    queue.have_snapshot = false;
                    let now = Instant::now();
                    self.0.request_snapshot(&mut queue, SnapshotRequestReason::QueueOverflow, now);
                    return
                }
                queue.frames_queued += 1;
                queue.newest_frame += 1;
            }
            _ => {}
        }
        self.0.stats.newest_publisher_frame.store(queue.newest_frame, Ordering::Relaxed);
        queue.items.push_back(LiveItem::Packet(packet));
        if let Some(waker) = queue.waker.as_ref() {
            waker.unpark();
        }
    }

    /// Queue a sync hash (dropped before the first snapshot).
    pub fn push_sync_hash(&self, frame: u64, hash: [u8; 32]) {
        let mut queue = self.0.queue.lock().unwrap_or_else(|p| p.into_inner());
        if !queue.have_snapshot {
            return
        }
        queue.items.push_back(LiveItem::SyncHash { frame, hash });
        if let Some(waker) = queue.waker.as_ref() {
            waker.unpark();
        }
    }

    /// The publisher is gone: whatever is queued still plays, then the follower stalls.
    pub fn end(&self) {
        self.0.ended.store(true, Ordering::Relaxed);
        let queue = self.0.queue.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(waker) = queue.waker.as_ref() {
            waker.unpark();
        }
    }

    /// Whether a snapshot has been queued since the feeder was made (or since the last overflow).
    pub fn has_snapshot(&self) -> bool {
        self.0.queue.lock().unwrap_or_else(|p| p.into_inner()).have_snapshot
    }
}

impl LiveReplaySource {
    /// Take the next item, if any.
    pub(crate) fn pop(&self) -> LivePoll {
        let mut queue = self.0.queue.lock().unwrap_or_else(|p| p.into_inner());
        match queue.items.pop_front() {
            Some(item) => {
                if matches!(item, LiveItem::Packet(Packet::NextFrame { .. })) {
                    queue.frames_queued -= 1;
                }
                LivePoll::Item(item)
            }
            None if self.0.ended.load(Ordering::Relaxed) => LivePoll::Ended,
            None => LivePoll::Waiting
        }
    }

    /// The core thread to wake when something arrives.
    pub fn set_waker(&self, thread: Thread) {
        self.0.queue.lock().unwrap_or_else(|p| p.into_inner()).waker = Some(thread);
    }

    /// Complete frames queued and not yet read.
    pub fn frames_available(&self) -> u64 {
        self.0.queue.lock().unwrap_or_else(|p| p.into_inner()).frames_queued
    }

    /// The publisher's newest frame number received.
    pub fn newest_publisher_frame(&self) -> u64 {
        self.0.queue.lock().unwrap_or_else(|p| p.into_inner()).newest_frame
    }

    /// Whether the publisher has ended the stream.
    pub fn ended(&self) -> bool {
        self.0.ended.load(Ordering::Relaxed)
    }

    /// Ask the publisher for a snapshot (rate-limited: at most one per
    /// [`SNAPSHOT_REQUEST_INTERVAL`] unless a snapshot arrives in between).
    pub fn request_snapshot(&self, reason: SnapshotRequestReason) {
        let mut queue = self.0.queue.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        self.0.request_snapshot(&mut queue, reason, now);
    }

    /// The counters this source updates.
    pub fn stats(&self) -> &Arc<FollowerStats> {
        &self.0.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;
    use supershuckie_replay_recorder::{ByteVec, KeyframeMetadata, TimestampMillis};

    fn keyframe(frame: u64) -> Packet {
        Packet::Keyframe {
            metadata: KeyframeMetadata { elapsed_frames: frame, ..Default::default() },
            state: ByteVec::from(&[1u8, 2, 3][..])
        }
    }

    fn next_frame() -> Packet {
        Packet::NextFrame { timestamp_delta: TimestampMillis(16) }
    }

    #[test]
    fn packets_before_the_first_snapshot_are_dropped_and_a_snapshot_clears_the_backlog() {
        let (tx, _rx) = channel();
        let (feeder, source) = live_replay_channel(Arc::new(FollowerStats::default()), tx);

        feeder.push_packet(next_frame());
        assert!(matches!(source.pop(), LivePoll::Waiting));
        assert_eq!(source.frames_available(), 0);

        feeder.push_packet(keyframe(10));
        feeder.push_packet(next_frame());
        feeder.push_packet(next_frame());
        assert_eq!(source.frames_available(), 2);
        assert_eq!(source.newest_publisher_frame(), 12);

        feeder.push_packet(keyframe(50));
        assert_eq!(source.frames_available(), 0);
        assert_eq!(source.newest_publisher_frame(), 50);
        assert!(matches!(source.pop(), LivePoll::Item(LiveItem::Packet(Packet::Keyframe { .. }))));
        assert!(matches!(source.pop(), LivePoll::Waiting));

        feeder.end();
        assert!(matches!(source.pop(), LivePoll::Ended));
    }

    #[test]
    fn overflow_drops_the_queue_and_requests_a_snapshot_once() {
        let (tx, rx) = channel();
        let stats = Arc::new(FollowerStats::default());
        let (feeder, source) = live_replay_channel(stats.clone(), tx);

        feeder.push_packet(keyframe(0));
        for _ in 0..MAX_QUEUED_FRAMES + 5 {
            feeder.push_packet(next_frame());
        }
        assert_eq!(source.frames_available(), 0, "the backlog is dropped");
        assert_eq!(rx.try_recv(), Ok(SnapshotRequestReason::QueueOverflow));
        assert!(rx.try_recv().is_err(), "one request, not one per dropped packet");
        assert_eq!(stats.snapshot_requests.load(Ordering::Relaxed), 1);

        // Nothing is kept until the snapshot answers.
        feeder.push_packet(next_frame());
        assert!(matches!(source.pop(), LivePoll::Waiting));
        feeder.push_packet(keyframe(2000));
        assert!(matches!(source.pop(), LivePoll::Item(LiveItem::Packet(Packet::Keyframe { .. }))));
    }

    #[test]
    fn snapshot_requests_are_rate_limited_until_a_snapshot_arrives() {
        let (tx, rx) = channel();
        let (feeder, source) = live_replay_channel(Arc::new(FollowerStats::default()), tx);

        source.request_snapshot(SnapshotRequestReason::Join);
        source.request_snapshot(SnapshotRequestReason::HashMismatch);
        assert_eq!(rx.try_recv(), Ok(SnapshotRequestReason::Join));
        assert!(rx.try_recv().is_err(), "a second request within the interval is held back");

        feeder.push_packet(keyframe(0));
        source.request_snapshot(SnapshotRequestReason::HashMismatch);
        assert_eq!(rx.try_recv(), Ok(SnapshotRequestReason::HashMismatch), "a snapshot re-arms the limiter");
    }
}

// ---------------------------------------------------------------------------------------------
// The follower side of `SuperShuckieCore`.
// ---------------------------------------------------------------------------------------------

use crate::emulator::PartialReplayRecordMetadata;
use crate::{ReplayPlayerAttachError, SuperShuckieCore};
use std::boxed::Box;
use std::string::String;
use supershuckie_replay_recorder::replay_file::record::{NonBlockingReplayFileRecorder, ReplayFileRecorder, ReplayFileRecorderFns, ReplayFileSink, ReplayFileWriteError};
use supershuckie_replay_recorder::replay_file::ReplayFileMetadata;
use supershuckie_replay_recorder::{ByteVec, InputBuffer, KeyframeMetadata, Speed, TimestampMillis};

/// Builds a follower's file recorder once the state to start it from is known.
type MakeFollowerRecorder = Box<dyn FnOnce(TimestampMillis, ByteVec, Speed, ByteVec) -> Result<Box<dyn ReplayFileRecorderFns>, ReplayFileWriteError> + Send>;

struct PendingFollowerRecording {
    make: MakeFollowerRecorder,
    frames_per_keyframe: u64
}

/// What a core following another player's game keeps besides its packet source.
pub struct FollowerState {
    source: LiveReplaySource,

    /// Sync hashes for frames before this one are ignored: right after a snapshot the console
    /// may still hold regenerated buffers the publisher's state did not carry.
    hash_suppressed_until: u64,

    /// The console's current input as the publisher encoded it (a snapshot's, or the last
    /// `ChangeInput`), which a file started mid-stream needs.
    last_input: InputBuffer,

    /// A file recording asked for before there was a state to start it from.
    pending_recording: Option<PendingFollowerRecording>,

    /// Problems that ended following (a snapshot that would not load).
    errors: Vec<String>
}

impl SuperShuckieCore {
    /// Follow another player's game: `source` delivers their packets (see [`live_replay_channel`]);
    /// `metadata` describes their ROM, BIOS and core, which must match this core's like a replay
    /// file's would (see `attach_replay_player`) unless `allow_mismatched`.
    ///
    /// Refused while this console is being published. Otherwise any recording is stopped and any
    /// replay detached; nothing runs until the first snapshot arrives (see
    /// [`Self::is_replay_waiting`]), and the clock is the publisher's from then on. Speed changes
    /// in the stream are ignored: a follower is paced by whoever drives it.
    pub fn attach_live_replay_source(&mut self, source: LiveReplaySource, metadata: &ReplayFileMetadata, allow_mismatched: bool) -> Result<(), ReplayPlayerAttachError> {
        self.check_replay_compat(metadata.console_type, &metadata.rom_checksum, &metadata.bios_checksum, &metadata.emulator_core_name, allow_mismatched)?;
        if self.stream_publisher.is_some() {
            return Err(ReplayPlayerAttachError::Incompatible {
                description: String::from("This console is being published to other players; it cannot follow one.")
            })
        }

        self.stop_recording_replay();
        self.detach_replay_player();
        self.detach_live_source();

        self.current_input = crate::emulator::Input::new();
        self.next_input = None;
        self.writes.clear();
        self.replay_counters = Some(alloc::collections::BTreeMap::new());
        self.replay_stalled = false;
        self.replay_frame_pending = false;
        self.input_latched = false;
        self.replay_playback_stopped = false;
        self.replay_waiting = true;
        self.frames_since_last_keyframe = 0;
        self.stream_time_origin = 0.into();
        self.ignore_speed_changes_in_replays = true;
        source.stats().waiting.store(true, Ordering::Relaxed);
        self.follower = Some(FollowerState {
            source,
            hash_suppressed_until: 0,
            last_input: InputBuffer::new(),
            pending_recording: None,
            errors: Vec::new()
        });
        self.clear_audio();
        self.bump_state_epoch();
        Ok(())
    }

    /// Stop following (see [`Self::attach_live_replay_source`]); closes the follower's own file
    /// recording, if any. The console keeps whatever state it had.
    pub fn detach_live_source(&mut self) {
        if self.follower.take().is_none() {
            return
        }
        self.stop_recording_replay();
        self.replay_stalled = false;
        self.replay_frame_pending = false;
        self.replay_waiting = false;
        self.input_latched = false;
        self.replay_playback_stopped = false;
        self.replay_counters = None;
        self.stream_time_origin = 0.into();
        self.reset_input();
        self.clear_audio();
        self.bump_state_epoch();
    }

    /// The counters of the live replay being followed, if any.
    pub fn follower_stats(&self) -> Option<&Arc<FollowerStats>> {
        self.follower.as_ref().map(|f| f.source.stats())
    }

    /// Complete frames received from the publisher and not yet run.
    pub fn live_frames_available(&self) -> u64 {
        self.follower.as_ref().map(|f| f.source.frames_available()).unwrap_or(0)
    }

    /// The newest frame number the publisher has sent.
    pub fn live_newest_publisher_frame(&self) -> u64 {
        self.follower.as_ref().map(|f| f.source.newest_publisher_frame()).unwrap_or(0)
    }

    /// Ask the publisher for a snapshot (rate-limited; see [`LiveReplaySource::request_snapshot`]).
    pub fn live_request_snapshot(&self, reason: SnapshotRequestReason) {
        if let Some(f) = self.follower.as_ref() {
            f.source.request_snapshot(reason);
        }
    }

    /// The core thread to wake when the publisher's data arrives.
    pub fn set_live_waker(&self, thread: Thread) {
        if let Some(f) = self.follower.as_ref() {
            f.source.set_waker(thread);
        }
    }

    /// Problems following reported since the last call.
    pub fn poll_follower_errors(&mut self) -> Vec<String> {
        self.follower.as_mut().map(|f| core::mem::take(&mut f.errors)).unwrap_or_default()
    }

    /// Also write the followed game to a replay file of its own, from the next snapshot on (or
    /// right away when one has been applied and the console sits at a frame boundary). The file
    /// counts frames and time from that point; snapshots that arrive later go in as full
    /// keyframes. `metadata` is the publisher's (their ROM, BIOS and core), which is what the
    /// file reproduces.
    pub fn start_recording_follower_replay<FS, TS>(&mut self, partial: PartialReplayRecordMetadata<FS, TS>, metadata: ReplayFileMetadata) -> Result<(), ReplayFileWriteError>
    where
        FS: ReplayFileSink + Send + Sync + 'static,
        TS: ReplayFileSink + Send + Sync + 'static
    {
        if self.follower.is_none() {
            return Err(ReplayFileWriteError::BadInput {
                explanation: alloc::borrow::Cow::Borrowed("not following another player's game")
            })
        }
        self.stop_recording_replay();

        let settings = partial.settings;
        let patch_data = partial.patch_data;
        let final_file = partial.final_file;
        let temp_file = partial.temp_file;
        let frames_per_keyframe = partial.frames_per_keyframe.get();
        let metadata = ReplayFileMetadata { crop_start: None, crop_end: None, timer_offset: None, ..metadata };
        let make: MakeFollowerRecorder = Box::new(move |starting_timestamp, input, speed, state| {
            let recorder = ReplayFileRecorder::new_with_metadata(metadata, patch_data, settings, starting_timestamp, input, speed, state, final_file, temp_file)?;
            Ok(Box::new(NonBlockingReplayFileRecorder::new(recorder)) as Box<dyn ReplayFileRecorderFns>)
        });

        let follower = self.follower.as_mut().expect("checked above");
        follower.pending_recording = Some(PendingFollowerRecording { make, frames_per_keyframe });

        let have_state = follower.source.stats().snapshots_applied.load(Ordering::Relaxed) > 0;
        if have_state && !self.core.is_mid_frame() {
            let state = ByteVec::Heap(self.core.create_save_state());
            self.start_pending_follower_recording(state)?;
        }
        Ok(())
    }

    /// Whether the followed game is being written to a file (started or still waiting for its
    /// first state).
    pub fn is_recording_follower_replay(&self) -> bool {
        self.follower.as_ref().is_some_and(|f| f.pending_recording.is_some()) || (self.is_following() && self.replay_file_recorder.is_some())
    }

    fn start_pending_follower_recording(&mut self, state: ByteVec) -> Result<(), ReplayFileWriteError> {
        let Some(follower) = self.follower.as_mut() else {
            return Ok(())
        };
        let Some(pending) = follower.pending_recording.take() else {
            return Ok(())
        };
        let input = follower.last_input.clone();
        let speed = self.replay_playback_speed;
        // The file's time starts at zero here.
        let recorder = (pending.make)(0.into(), input, speed, state)?;
        self.stream_time_origin = self.total_milliseconds;
        self.frames_per_keyframe = pending.frames_per_keyframe;
        self.frames_since_last_keyframe = 0;
        self.full_keyframe_pending = false;
        self.replay_file_recorder = Some(recorder);
        Ok(())
    }

    /// Read the live replay up to the next frame marker (see `handle_replay`).
    pub(crate) fn handle_live_replay(&mut self) {
        loop {
            let poll = match self.follower.as_ref() {
                Some(f) => f.source.pop(),
                None => return
            };
            match poll {
                LivePoll::Waiting => {
                    self.replay_waiting = true;
                    if let Some(f) = self.follower.as_ref() {
                        f.source.stats().waiting.store(true, Ordering::Relaxed);
                    }
                    return
                }
                LivePoll::Ended => {
                    self.replay_waiting = false;
                    self.replay_stalled = true;
                    return
                }
                LivePoll::Item(LiveItem::SyncHash { frame, hash }) => self.check_sync_hash(frame, hash),
                LivePoll::Item(LiveItem::Packet(Packet::Keyframe { metadata, state })) => {
                    self.apply_stream_snapshot(&metadata, state.as_slice());
                    if self.replay_stalled {
                        return
                    }
                }
                LivePoll::Item(LiveItem::Packet(packet)) => {
                    let next_frame = matches!(packet, Packet::NextFrame { .. });
                    self.apply_playback_packet(&packet, None);
                    if next_frame {
                        self.replay_waiting = false;
                        if let Some(f) = self.follower.as_ref() {
                            f.source.stats().waiting.store(false, Ordering::Relaxed);
                        }
                        return
                    }
                }
            }
        }
    }

    /// Load a snapshot the publisher sent: the console is put exactly where they were, and
    /// whatever was queued before it was already discarded by the feeder.
    fn apply_stream_snapshot(&mut self, metadata: &KeyframeMetadata, state: &[u8]) {
        if let Err(e) = self.core.load_save_state(state) {
            if let Some(f) = self.follower.as_mut() {
                f.errors.push(alloc::format!("the other player's state could not be loaded: {e}"));
            }
            self.replay_stalled = true;
            return
        }
        // Save states do not carry the buttons held; restore what the publisher had pressed.
        self.core.set_input_encoded(metadata.input.as_slice());
        let previous_counters = self.replay_counters.take().unwrap_or_default();
        self.total_frames = metadata.elapsed_frames;
        self.total_milliseconds = metadata.elapsed_millis;
        self.replay_counters = Some(metadata.counters.iter().map(|c| (c.name.clone(), c.value)).collect());
        self.replay_playback_speed = metadata.speed;
        self.replay_frame_pending = false;
        self.replay_stalled = false;
        self.frames_since_last_keyframe = 0;
        self.bump_state_epoch();
        self.clear_audio();

        let now_recording = self.replay_file_recorder.is_some();
        if let Some(f) = self.follower.as_mut() {
            f.hash_suppressed_until = metadata.elapsed_frames + Self::POST_LOAD_FRAMES;
            f.last_input = metadata.input.clone();
            f.source.stats().snapshots_applied.fetch_add(1, Ordering::Relaxed);
        }

        if now_recording {
            // The file jumps too: a state load, then a full keyframe so the jump is seekable.
            let ms = self.recording_millis();
            self.with_recorder(|r| r.load_save_state(ByteVec::from(state)));
            self.with_recorder(|r| r.set_input(metadata.input.clone()));
            self.with_recorder(|r| r.set_speed(metadata.speed));
            // Counters are absolute in a snapshot; the file records the differences.
            for counter in &metadata.counters {
                let delta = counter.value.wrapping_sub(previous_counters.get(&counter.name).copied().unwrap_or(0));
                if delta != 0 {
                    let name = counter.name.clone();
                    self.with_recorder(|r| r.change_counter(name, delta));
                }
            }
            self.with_recorder(|r| r.insert_keyframe_full(ByteVec::from(state), ms));
        }
        else if let Err(e) = self.start_pending_follower_recording(ByteVec::from(state)) {
            if let Some(f) = self.follower.as_mut() {
                f.errors.push(alloc::format!("the replay file could not be started: {e}"));
            }
        }
    }

    /// Compare the publisher's work-RAM hash for `frame` with ours; a mismatch asks for a
    /// snapshot (rate-limited) and playing continues meanwhile.
    fn check_sync_hash(&mut self, frame: u64, hash: [u8; 32]) {
        let suppressed_until = match self.follower.as_ref() {
            Some(f) => f.hash_suppressed_until,
            None => return
        };
        if frame != self.total_frames || frame < suppressed_until {
            return
        }
        let Some(mine) = self.sync_hash() else {
            return
        };
        if mine != hash && let Some(f) = self.follower.as_ref() {
            f.source.stats().hash_mismatches.fetch_add(1, Ordering::Relaxed);
            f.source.request_snapshot(SnapshotRequestReason::HashMismatch);
        }
    }

    /// Mirror a live replay's packet into the follower's own file recording (nothing for a
    /// replay played from a file: such a core never records).
    pub(crate) fn follower_mirror(&mut self, packet: &Packet, wrote: bool) {
        let Some(follower) = self.follower.as_mut() else {
            return
        };
        if let Packet::ChangeInput { data } = packet {
            follower.last_input = data.clone();
        }
        if self.replay_file_recorder.is_none() {
            return
        }
        match packet {
            Packet::NextFrame { .. } => {
                let ms = self.recording_millis();
                self.with_recorder(|r| r.next_frame(ms));
            }
            Packet::ChangeInput { data } => {
                let data = data.clone();
                self.with_recorder(|r| r.set_input(data));
            }
            Packet::WriteMemory { address, data } if wrote => {
                let (address, data) = (*address, data.clone());
                self.with_recorder(|r| r.write_memory(address, data));
            }
            Packet::ChangeSpeed { speed } => {
                let speed = *speed;
                self.with_recorder(|r| r.set_speed(speed));
            }
            Packet::ResetConsole => self.with_recorder(|r| r.reset_console()),
            Packet::LoadSaveState { state } => {
                let state = state.clone();
                self.with_recorder(|r| r.load_save_state(state));
            }
            Packet::IncrementCounter { name, delta } => {
                let (name, delta) = (name.clone(), *delta);
                self.with_recorder(|r| r.change_counter(name, delta));
            }
            _ => {}
        }
    }
}
