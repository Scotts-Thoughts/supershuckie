//! Play Together: the network protocol and session layer.
//!
//! Every participant publishes its emulator session as a stream of replay packets. One
//! participant hosts a TCP port, the others connect, and the host relays every stream to every
//! other participant (a star). A follower asks a publisher for a snapshot (its full save state),
//! then applies the packets that follow it.
//!
//! The wire format is `docs/play-together-protocol.md`, version 1. Everything read from a socket
//! is treated as hostile: lengths are capped before allocation, bodies are read in chunks, and
//! nothing that arrives can make this crate panic.
//!
//! ```text
//! HostSession::bind(config, local)        -> Session + local_addr()
//! ClientSession::connect(code, cfg, local) -> Session (outcome arrives as an event)
//! session.poll_events()                    every frame from the UI thread
//! session.publisher().publish(..)          from the emulator thread
//! session.subscribe(peer, sink)            to follow a participant
//! ```

#![warn(missing_docs)]

pub mod code;
pub mod compat;
pub mod conn;
pub mod error;
pub mod protocol;
pub mod session;
pub mod transport;

pub use code::{probe_local_ip, JoinCode, JoinCodeError};
pub use compat::{dedupe_display_name, describe_incompatibility, follow_compatibility, sanitize_display_name, FollowCompatibility};
pub use error::{DecodeError, DisconnectReason, Phase, PlayTogetherError, PublishError};
pub use protocol::{
    count_frames, decode_packets, encode_packets, LeaveReason, LocalParticipant, Message, ParticipantInfo, PublisherInfo, RefusalReason, SnapshotData,
    PROTOCOL_VERSION,
};
pub use session::{
    ClientConfig, ClientSession, FollowerSink, HostConfig, HostSession, PublisherHandle, Role, Session, SessionEvent, SessionStats,
};
pub use transport::{Connection, Listener, TcpTransport, Transport};

/// Identifies a participant within a session. 0 means none (or everyone, as a target); the host
/// is always 1; clients get 2, 3, ... and an id is never reused within a session.
pub type PeerId = u16;

/// Identifies a session; chosen by the host.
pub type SessionId = u64;

/// A blake3 hash.
pub type Blake3Hash = [u8; 32];

/// The TCP port a host uses unless told otherwise.
pub const DEFAULT_PORT: u16 = 30170;

/// Most participants in a session, host included.
pub const MAX_PARTICIPANTS: usize = 8;
