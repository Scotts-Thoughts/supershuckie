//! Per-publisher follow state on a receiver: what to do with each `Stream`, `Snapshot` and
//! `SyncHash` that arrives from one publisher.
//!
//! ```text
//! AwaitingSnapshot ──Snapshot addressed to us──▶ Live { expected = snapshot.frame }
//!        ▲                                            │
//!        └──────── Stream.first_frame != expected ────┘   (gap: ask for a snapshot again)
//! ```
//!
//! Streams and hashes are discarded while awaiting (and while no sink is subscribed). The sink
//! is called with the slot's own lock held and never with the session's lock, so a slow sink
//! delays only its own publisher's reader.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use supershuckie_replay_recorder::Packet;

use crate::protocol::count_frames;
use crate::{Blake3Hash, FollowerSink, LeaveReason, SnapshotData};

/// Minimum time between two `RequestSnapshot`s for the same publisher.
pub const SNAPSHOT_REQUEST_INTERVAL: Duration = Duration::from_secs(2);

/// What a stream did to the follow state.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StreamOutcome {
    /// Not live (or no sink): discarded.
    Discarded,
    /// Handed to the sink.
    Delivered,
    /// `first_frame` did not match; the slot is awaiting a snapshot again.
    Gap {
        /// What we expected.
        expected: u64,
        /// What arrived.
        got: u64,
    },
}

struct FollowState {
    sink: Option<Box<dyn FollowerSink>>,
    /// `Some(expected_frame)` while live.
    live: Option<u64>,
    last_request: Option<Instant>,
}

/// One publisher's follow state plus its sink.
pub(crate) struct FollowSlot {
    state: Mutex<FollowState>,
}

impl FollowSlot {
    pub(crate) fn new() -> FollowSlot {
        FollowSlot { state: Mutex::new(FollowState { sink: None, live: None, last_request: None }) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FollowState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Install a sink; the slot awaits a snapshot. Any previous sink is dropped.
    pub(crate) fn set_sink(&self, sink: Box<dyn FollowerSink>) {
        let mut s = self.lock();
        s.sink = Some(sink);
        s.live = None;
    }

    /// Remove the sink without telling it anything.
    pub(crate) fn clear_sink(&self) {
        let mut s = self.lock();
        s.sink = None;
        s.live = None;
    }

    /// Tell the sink the publisher is gone and drop it.
    pub(crate) fn end(&self, reason: LeaveReason) {
        let mut s = self.lock();
        s.live = None;
        if let Some(mut sink) = s.sink.take() {
            sink.ended(reason);
        }
    }

    /// Whether a sink is installed.
    pub(crate) fn has_sink(&self) -> bool {
        self.lock().sink.is_some()
    }

    /// Whether a snapshot has arrived and streams are being applied.
    pub(crate) fn is_live(&self) -> bool {
        self.lock().live.is_some()
    }

    /// Whether a `RequestSnapshot` may go out now (rate limited); records it if so.
    pub(crate) fn may_request(&self, force: bool) -> bool {
        let mut s = self.lock();
        let now = Instant::now();
        let allowed = force || s.last_request.is_none_or(|t| now.duration_since(t) >= SNAPSHOT_REQUEST_INTERVAL);
        if allowed {
            s.last_request = Some(now);
        }
        allowed
    }

    /// A snapshot addressed to us arrived: go live at its frame.
    pub(crate) fn on_snapshot(&self, snapshot: SnapshotData) -> bool {
        let mut s = self.lock();
        let Some(sink) = s.sink.as_mut() else {
            return false;
        };
        let frame = snapshot.frame;
        sink.snapshot(snapshot);
        s.live = Some(frame);
        // The snapshot answers whatever request was outstanding; the next problem may ask again
        // right away.
        s.last_request = None;
        true
    }

    /// A stream arrived.
    pub(crate) fn on_stream(&self, first_frame: u64, packets: Vec<Packet>) -> StreamOutcome {
        let mut s = self.lock();
        let Some(expected) = s.live else {
            return StreamOutcome::Discarded;
        };
        if first_frame != expected {
            s.live = None;
            return StreamOutcome::Gap { expected, got: first_frame };
        }
        let frames = count_frames(&packets);
        match s.sink.as_mut() {
            Some(sink) => {
                sink.packets(first_frame, packets);
                s.live = Some(expected + frames);
                StreamOutcome::Delivered
            }
            None => {
                s.live = None;
                StreamOutcome::Discarded
            }
        }
    }

    /// A sync hash arrived; delivered only while live.
    pub(crate) fn on_sync_hash(&self, frame: u64, hash: Blake3Hash) -> bool {
        let mut s = self.lock();
        if s.live.is_none() {
            return false;
        }
        match s.sink.as_mut() {
            Some(sink) => {
                sink.sync_hash(frame, hash);
                true
            }
            None => false,
        }
    }
}
