//! Sessions: the host (listens, admits, relays) and the client (connects to a host). Both
//! implement [`Session`] so the application handles them alike.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use supershuckie_replay_recorder::Packet;

use crate::conn::{Outbound, SnapshotItem, Stats};
use crate::error::{DisconnectReason, PlayTogetherError, PublishError};
use crate::protocol::{LeaveReason, Message, ParticipantInfo};
use crate::{Blake3Hash, PeerId, SessionId, SnapshotData, MAX_PARTICIPANTS};

pub mod client;
pub(crate) mod follow;
pub mod host;

pub use client::ClientSession;
pub use follow::SNAPSHOT_REQUEST_INTERVAL;
pub use host::HostSession;

/// Which side of the star we are.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Role {
    /// We listen and relay.
    Host,
    /// We connected to a host.
    Client,
}

/// Something that happened in the session, delivered by [`Session::poll_events`].
#[derive(Clone, Debug, PartialEq)]
pub enum SessionEvent {
    /// We are in: our id, our (possibly deduplicated) name, and everyone else. The host emits
    /// this right after binding so both roles are handled alike.
    Connected {
        /// The session's id.
        session_id: SessionId,
        /// Our id (1 for the host).
        local_peer_id: PeerId,
        /// Our display name as the session knows it.
        local_display_name: String,
        /// Everyone else already in the session.
        participants: Vec<ParticipantInfo>,
    },
    /// Someone was admitted.
    Joined(ParticipantInfo),
    /// Someone is gone.
    Left {
        /// Who.
        peer_id: PeerId,
        /// Why.
        reason: LeaveReason,
    },
    /// Followers want a snapshot of our session. Answer with
    /// [`PublisherHandle::publish_snapshot`], targeting the single requester or 0 (everyone)
    /// when there are several. Coalesced: at most one per two seconds.
    SnapshotRequested {
        /// Who asked.
        requesters: Vec<PeerId>,
    },
    /// A publisher's data was dropped because it could not be delivered in time.
    StreamOverrun {
        /// Which publisher.
        from: PeerId,
        /// How much was dropped.
        dropped_bytes: u64,
    },
    /// Everybody resets when `deadline` passes.
    ResetAll {
        /// Distinguishes one reset from the next.
        race_id: u32,
        /// When to reset.
        deadline: Instant,
    },
    /// A round-trip time was measured.
    RttUpdated {
        /// Whose link (the host, for a client).
        peer_id: PeerId,
        /// The time.
        rtt: Duration,
    },
    /// Something worth telling the user that did not end the session.
    Warning(String),
    /// The session is over. Terminal: nothing follows.
    Disconnected {
        /// Why.
        reason: DisconnectReason,
    },
}

/// Receives one publisher's data straight from the network reader thread (no UI hop).
pub trait FollowerSink: Send + 'static {
    /// A run of whole packets starting at the publisher's frame `first_frame`.
    fn packets(&mut self, first_frame: u64, packets: Vec<Packet>);
    /// The publisher's full state; following (re)starts from here.
    fn snapshot(&mut self, snapshot: SnapshotData);
    /// The publisher's state hash at `frame`, for comparison.
    fn sync_hash(&mut self, frame: u64, hash: Blake3Hash);
    /// The publisher is gone (or we are); no more calls follow.
    fn ended(&mut self, reason: LeaveReason);
}

/// What a session does with something the local publisher wants sent.
pub(crate) trait PublishBackend: Send + Sync {
    /// Queue `item` for `target` (0 = every participant).
    fn publish(&self, item: Outbound, target: PeerId) -> Result<(), PublishError>;
    /// Our peer id (0 for a client until `Connected`).
    fn local_id(&self) -> PeerId;
}

/// How the emulator thread publishes its session. Every method is non-blocking: a queue push
/// and a wake-up; compression and framing happen on the writer threads.
#[derive(Clone)]
pub struct PublisherHandle {
    backend: Arc<dyn PublishBackend>,
}

impl PublisherHandle {
    pub(crate) fn new(backend: Arc<dyn PublishBackend>) -> PublisherHandle {
        PublisherHandle { backend }
    }

    /// Whole `Packet` bytes for one or more frames. `first_frame` is our frame count before the
    /// first `NextFrame` in `packets`.
    pub fn publish(&self, first_frame: u64, packets: Vec<u8>) -> Result<(), PublishError> {
        self.backend.publish(Outbound::Stream { first_frame, bytes: Arc::new(packets) }, 0)
    }

    /// Our full state, for `target` (0 = everyone). zstd compression happens on the writer
    /// thread, never here.
    pub fn publish_snapshot(&self, snapshot: SnapshotData, target: PeerId) -> Result<(), PublishError> {
        self.backend.publish(Outbound::Snapshot { snapshot: Arc::new(SnapshotItem::new(snapshot)), target }, target)
    }

    /// Our state hash at `frame`.
    pub fn publish_sync_hash(&self, frame: u64, hash: Blake3Hash) -> Result<(), PublishError> {
        // Streams get their `from` from the writer; a hash is tiny, so it is framed here with
        // the id the backend knows (a client has none before `Connected`).
        let from = self.backend.local_id();
        if from == 0 {
            return Err(PublishError::NotConnected);
        }
        self.backend.publish(Outbound::Encoded(Arc::new(Message::SyncHash { from, frame, hash }.encoded())), 0)
    }
}

/// Counters for the stats display.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SessionStats {
    /// Bytes received on every connection.
    pub bytes_in: u64,
    /// Bytes sent on every connection.
    pub bytes_out: u64,
    /// Messages received.
    pub messages_in: u64,
    /// Messages sent.
    pub messages_out: u64,
    /// Messages with a tag this version does not know, skipped.
    pub unknown_messages_skipped: u64,
    /// Bytes waiting in outbound queues right now.
    pub outbound_queued_bytes: u64,
    /// The last measured round-trip time per connection.
    pub rtt: Vec<(PeerId, Duration)>,
}

/// A running session of either role.
pub trait Session: Send + Sync {
    /// Host or client.
    fn role(&self) -> Role;
    /// The session's id (0 for a client until `Connected`).
    fn session_id(&self) -> SessionId;
    /// Our id (0 for a client until `Connected`).
    fn local_peer_id(&self) -> PeerId;
    /// Everyone but us.
    fn participants(&self) -> Vec<ParticipantInfo>;
    /// Whether the session is up (a client after `Connected`, a host until `leave`).
    fn is_connected(&self) -> bool;
    /// Everything that happened since the last call. Never blocks.
    fn poll_events(&self) -> Vec<SessionEvent>;
    /// How to publish our session.
    fn publisher(&self) -> PublisherHandle;
    /// Follow `publisher`: installs the sink and asks for a snapshot; its streams and hashes are
    /// discarded until the snapshot arrives.
    fn subscribe(&self, publisher: PeerId, sink: Box<dyn FollowerSink>) -> Result<(), PlayTogetherError>;
    /// Stop following `publisher` (the sink is dropped without a call).
    fn unsubscribe(&self, publisher: PeerId);
    /// Ask `publisher` for a fresh snapshot (at most once per two seconds per publisher).
    fn request_snapshot(&self, publisher: PeerId);
    /// Host only: everybody resets after `countdown`. The host gets the `ResetAll` event too.
    fn send_reset_all(&self, countdown: Duration) -> Result<u32, PlayTogetherError>;
    /// Host only: remove a participant.
    fn kick(&self, peer: PeerId) -> Result<(), PlayTogetherError>;
    /// Counters.
    fn stats(&self) -> SessionStats;
    /// Say goodbye, stop and join the threads (bounded, about two seconds). Also on drop.
    fn leave(&self);
}

/// How the host listens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostConfig {
    /// The address to bind (`"0.0.0.0"` for every interface).
    pub bind_address: String,
    /// The port to bind (0 picks a free one).
    pub port: u16,
    /// Most participants including the host (at most [`MAX_PARTICIPANTS`]).
    pub max_participants: usize,
    /// Whether Nintendo DS sessions may join.
    pub allow_nintendo_ds: bool,
    /// How long a connection may take to send its `Hello`.
    pub handshake_timeout: Duration,
    /// How long a connection may go without sending anything.
    pub idle_timeout: Duration,
}

impl Default for HostConfig {
    fn default() -> Self {
        HostConfig {
            bind_address: "0.0.0.0".to_owned(),
            port: crate::DEFAULT_PORT,
            max_participants: MAX_PARTICIPANTS,
            allow_nintendo_ds: false,
            handshake_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(10),
        }
    }
}

/// How a client connects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientConfig {
    /// How long each resolved address gets to answer the TCP connect.
    pub connect_timeout: Duration,
    /// How long the host gets to answer our `Hello`.
    pub handshake_timeout: Duration,
    /// How long the host may go without sending anything.
    pub idle_timeout: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        ClientConfig {
            connect_timeout: Duration::from_secs(5),
            handshake_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(10),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Shared machinery

/// The socket read/write poll granularity: how often blocked reader and writer threads check
/// their stop flags and deadlines.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// How long `leave()` waits for the threads before detaching them.
pub(crate) const LEAVE_TIMEOUT: Duration = Duration::from_secs(2);

/// How long `leave()` gives the writers to get a `Goodbye` out before sockets are shut down.
pub(crate) const GOODBYE_GRACE: Duration = Duration::from_millis(500);

/// The events waiting for `poll_events`.
#[derive(Default)]
pub(crate) struct EventQueue {
    events: Mutex<VecDeque<SessionEvent>>,
}

impl EventQueue {
    pub(crate) fn push(&self, event: SessionEvent) {
        self.events.lock().unwrap_or_else(|e| e.into_inner()).push_back(event);
    }
    pub(crate) fn drain(&self) -> Vec<SessionEvent> {
        self.events.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect()
    }
}

/// Collects `RequestSnapshot`s addressed to the local publisher and lets at most one
/// `SnapshotRequested` out per [`SNAPSHOT_REQUEST_INTERVAL`], carrying everyone who asked.
#[derive(Default)]
pub(crate) struct Coalescer {
    pending: Vec<PeerId>,
    last_emit: Option<Instant>,
}

impl Coalescer {
    /// Note a request; a requester already pending is dropped.
    pub(crate) fn request(&mut self, requester: PeerId) {
        if requester != 0 && !self.pending.contains(&requester) {
            self.pending.push(requester);
        }
    }

    /// The requesters to announce now, if the interval allows.
    pub(crate) fn flush(&mut self) -> Option<Vec<PeerId>> {
        if self.pending.is_empty() {
            return None;
        }
        let now = Instant::now();
        if self.last_emit.is_some_and(|t| now.duration_since(t) < SNAPSHOT_REQUEST_INTERVAL) {
            return None;
        }
        self.last_emit = Some(now);
        Some(std::mem::take(&mut self.pending))
    }

    /// Forget requests from `peer` (it left).
    pub(crate) fn forget(&mut self, peer: PeerId) {
        self.pending.retain(|p| *p != peer);
    }
}

/// The `LeaveReason` a sink hears when our own session ends for `reason`.
pub(crate) fn leave_reason_for(reason: &DisconnectReason) -> LeaveReason {
    match reason {
        DisconnectReason::Left => LeaveReason::Left,
        DisconnectReason::HostLeft => LeaveReason::HostLeft,
        DisconnectReason::Kicked => LeaveReason::Kicked,
        DisconnectReason::Timeout(_) => LeaveReason::Timeout,
        DisconnectReason::TooSlow { .. } => LeaveReason::TooSlow,
        DisconnectReason::ProtocolError(_) => LeaveReason::ProtocolError,
        DisconnectReason::IoError(_) => LeaveReason::IoError,
        DisconnectReason::Refused { .. } | DisconnectReason::ConnectFailed(_) => LeaveReason::Left,
    }
}

/// Wait up to `timeout` for every handle to finish, join the finished ones and detach the rest.
pub(crate) fn join_bounded(handles: Vec<std::thread::JoinHandle<()>>, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    let mut handles = handles;
    while !handles.is_empty() && Instant::now() < deadline {
        handles.retain(|h| !h.is_finished());
        if handles.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // Finished handles were dropped by `retain`; joining a finished thread is immediate but its
    // handle is gone, and detaching is what we want for the stragglers anyway.
    drop(handles);
}

/// The session's counters plus the per-connection numbers the caller adds.
pub(crate) fn base_stats(stats: &Stats) -> SessionStats {
    SessionStats {
        bytes_in: stats.bytes_in.load(Ordering::Relaxed),
        bytes_out: stats.bytes_out.load(Ordering::Relaxed),
        messages_in: stats.messages_in.load(Ordering::Relaxed),
        messages_out: stats.messages_out.load(Ordering::Relaxed),
        unknown_messages_skipped: stats.unknown_messages_skipped.load(Ordering::Relaxed),
        outbound_queued_bytes: 0,
        rtt: Vec::new(),
    }
}
