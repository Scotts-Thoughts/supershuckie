//! Live publishing of the running session (Play Together).
//!
//! A [`SuperShuckieCore`](crate::SuperShuckieCore) can mirror everything that reproduces its
//! session -- the same events a replay file records -- into a [`StreamPublisherFns`], independently
//! of whether a replay is being recorded to disk. A follower somewhere else feeds those events
//! into its own core through a [`LiveReplaySource`](crate::live_replay::LiveReplaySource) and
//! emulates the same game in step.
//!
//! What goes out, and when:
//!
//! * a full **snapshot** (the exact save state plus the input, speed, frame counter, time and
//!   counters at that moment) when publishing starts and whenever one is requested
//!   ([`SuperShuckieCore::request_stream_snapshot`](crate::SuperShuckieCore::request_stream_snapshot)),
//!   always at a frame boundary and in order with the packets around it;
//! * the per-frame events: input changes (once per emulated frame at most), external memory
//!   writes that were actually applied, resets, save-state loads, counter changes and one
//!   `next_frame` per emulated frame;
//! * a **sync hash** of the console's work RAM every [`SYNC_HASH_INTERVAL_FRAMES`] frames, right
//!   after that frame's `next_frame`, so a follower can tell that it has drifted and ask for a
//!   snapshot.
//!
//! Speed changes, periodic keyframes, bookmarks and timer marks are not published: a follower
//! paces itself on arrival and keeps its own file.

use alloc::string::String;
use alloc::vec::Vec;
use supershuckie_replay_recorder::{ByteVec, InputBuffer, KeyframeMetadata, SignedInteger, TimestampMillis, UnsignedInteger};

/// Frames between two sync hashes of a publisher's work RAM (one emulated second).
///
/// Hashing is cheap (tens of microseconds on the Game Boy, about 0.2 ms on the Game Boy Advance)
/// but a desync shows up in work RAM within a frame or two whenever one looks, so once a second
/// catches it just as well as every frame would.
pub const SYNC_HASH_INTERVAL_FRAMES: u64 = 60;

/// Where a publishing core sends its session.
///
/// Every method is called on the core thread, once per event; implementations must never block
/// (hand the data to another thread) and must report a broken transport through
/// [`poll_errors`](Self::poll_errors) rather than panic.
pub trait StreamPublisherFns: Send + 'static {
    /// The complete state at a frame boundary: `state` is exactly what
    /// `EmulatorCore::create_save_state` produced (never a delta), `metadata` says which frame
    /// and time it is at and what input, speed and counters go with it. Sent when publishing
    /// starts and on request; a follower loads it and continues from the packets that follow.
    fn snapshot(&mut self, metadata: KeyframeMetadata, state: Vec<u8>);

    /// One frame was emulated; `timestamp_millis` is the publisher's absolute replay time after
    /// it (the implementation derives deltas). Called once per emulated frame.
    fn next_frame(&mut self, timestamp_millis: TimestampMillis);

    /// The console's input changed (encoded as the console encodes it).
    fn set_input(&mut self, input: InputBuffer);

    /// External memory write that was applied.
    fn write_memory(&mut self, address: UnsignedInteger, data: ByteVec);

    /// The console was hard reset.
    fn reset_console(&mut self);

    /// A save state was loaded.
    fn load_save_state(&mut self, state: ByteVec);

    /// A replay counter changed by `delta`.
    fn change_counter(&mut self, name: String, delta: SignedInteger);

    /// blake3 of the work RAM after frame `frame` (see [`SYNC_HASH_INTERVAL_FRAMES`]).
    fn sync_hash(&mut self, frame: UnsignedInteger, hash: [u8; 32]);

    /// Publishing stopped; nothing follows.
    fn end(&mut self);

    /// Problems since the last call (a follower or the network went away); the core reports
    /// them and keeps publishing to whoever is left.
    fn poll_errors(&mut self) -> Vec<String>;

    /// A state buffer the implementation has finished with, for reuse by the next snapshot (the
    /// same pooling as the replay recorder's; see `ReplayFileRecorderFns::take_free_state_buffer`).
    fn take_free_state_buffer(&mut self) -> Option<Vec<u8>> {
        None
    }
}

/// Why a follower asks its publisher for a snapshot.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum SnapshotRequestReason {
    /// The follower just attached and has no state yet.
    Join,

    /// The follower's work RAM hash differed from the publisher's at the same frame.
    HashMismatch,

    /// The follower is so far behind that emulating the backlog costs more than a state load.
    TooFarBehind,

    /// The follower's queue overflowed and the backlog was dropped.
    QueueOverflow,

    /// Packets went missing between the publisher and the follower.
    Gap
}

impl core::fmt::Display for SnapshotRequestReason {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Join => "joining",
            Self::HashMismatch => "desynced",
            Self::TooFarBehind => "too far behind",
            Self::QueueOverflow => "queue overflowed",
            Self::Gap => "packets were lost"
        })
    }
}
