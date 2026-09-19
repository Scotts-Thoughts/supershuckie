//! Error and reason types shared by the protocol and the sessions.

use std::fmt;

use crate::code::JoinCodeError;
use crate::{PeerId, RefusalReason};

/// Why a message could not be decoded.
///
/// Everything that comes off a socket is hostile input; every variant here is a clean refusal,
/// never a panic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// The message ends before its fields do.
    Truncated,
    /// A tag this version does not know. The body has already been consumed and skipped.
    UnknownTag(u8),
    /// A string field is not UTF-8.
    BadString,
    /// The message has bytes after its last field.
    TrailingBytes(usize),
    /// A framed message claims a length outside what its tag allows (or zero).
    TooLong(u32),
    /// A boolean byte other than 0 or 1.
    BadBool(u8),
    /// A peer id that must not be zero was zero.
    ZeroPeerId,
    /// A speed of zero (speeds are `NonZeroU16`).
    BadSpeed,
    /// An assigned player colour that is not in the palette.
    BadColor(u8),
    /// An enumeration value (console, patch format, reason, encoding) this version does not know.
    BadEnum {
        /// Which field.
        what: &'static str,
        /// The value on the wire.
        value: u32,
    },
    /// A `vec` field claims more items than allowed.
    TooMany {
        /// Which field.
        what: &'static str,
        /// The count on the wire.
        count: u32,
        /// The cap.
        max: usize,
    },
    /// A string or bytes field is longer than allowed.
    FieldTooLong {
        /// Which field.
        what: &'static str,
        /// The length on the wire.
        len: u32,
        /// The cap.
        max: usize,
    },
    /// A stream carries a packet kind that Play Together never sends.
    ForbiddenPacket(&'static str),
    /// A stream's packet bytes do not parse.
    BadPacket(String),
    /// A snapshot claims a decompressed state larger than allowed.
    StateTooLarge(u64),
    /// A snapshot's state does not decompress (or its raw length disagrees with `state_len`).
    BadState(String),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Truncated => f.write_str("message is shorter than its fields"),
            DecodeError::UnknownTag(t) => write!(f, "unknown message tag 0x{t:02X}"),
            DecodeError::BadString => f.write_str("string is not UTF-8"),
            DecodeError::TrailingBytes(n) => write!(f, "{n} unexpected trailing bytes"),
            DecodeError::TooLong(n) => write!(f, "message of {n} bytes is longer than allowed"),
            DecodeError::BadBool(b) => write!(f, "boolean byte {b} is neither 0 nor 1"),
            DecodeError::ZeroPeerId => f.write_str("peer id 0 where a real peer is required"),
            DecodeError::BadSpeed => f.write_str("speed of zero"),
            DecodeError::BadColor(c) => write!(f, "player colour {c} is not in the palette"),
            DecodeError::BadEnum { what, value } => write!(f, "unknown {what} value {value}"),
            DecodeError::TooMany { what, count, max } => write!(f, "{count} {what} is more than the {max} allowed"),
            DecodeError::FieldTooLong { what, len, max } => write!(f, "{what} of {len} bytes is longer than the {max} allowed"),
            DecodeError::ForbiddenPacket(kind) => write!(f, "stream carries a {kind} packet, which Play Together never sends"),
            DecodeError::BadPacket(text) => write!(f, "stream packet does not parse: {text}"),
            DecodeError::StateTooLarge(n) => write!(f, "snapshot state of {n} bytes is larger than allowed"),
            DecodeError::BadState(text) => write!(f, "snapshot state does not decode: {text}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Which stage of a connection's life a timeout happened in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Phase {
    /// Resolving the host name and opening the TCP connection.
    Connect,
    /// Waiting for the `Hello` / `Welcome` exchange to finish.
    Handshake,
    /// Connected; nothing arrived for the idle timeout.
    Idle,
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Phase::Connect => "connecting",
            Phase::Handshake => "handshake",
            Phase::Idle => "idle",
        })
    }
}

/// Why the local session ended (the terminal `SessionEvent::Disconnected`).
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum DisconnectReason {
    /// We called `leave()`.
    Left,
    /// The host sent `Goodbye`.
    HostLeft,
    /// The host kicked us.
    Kicked,
    /// Nothing arrived in time during the given phase.
    Timeout(Phase),
    /// Our outbound queue overflowed: the other side could not keep up with what we send.
    TooSlow {
        /// Bytes waiting in the queue when it overflowed.
        queued_bytes: u64,
    },
    /// The other side broke the protocol (or told us we did).
    ProtocolError(String),
    /// The socket failed.
    IoError(String),
    /// The host refused our `Hello`.
    Refused {
        /// The machine-readable reason.
        reason: RefusalReason,
        /// The host's explanation.
        text: String,
    },
    /// The TCP connection could not be made.
    ConnectFailed(String),
}

impl fmt::Display for DisconnectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DisconnectReason::Left => f.write_str("left the session"),
            DisconnectReason::HostLeft => f.write_str("the host ended the session"),
            DisconnectReason::Kicked => f.write_str("the host removed you from the session"),
            DisconnectReason::Timeout(phase) => write!(f, "timed out ({phase})"),
            DisconnectReason::TooSlow { queued_bytes } => {
                write!(f, "the connection could not keep up ({queued_bytes} bytes were waiting to be sent)")
            }
            DisconnectReason::ProtocolError(text) => write!(f, "protocol error: {text}"),
            DisconnectReason::IoError(text) => write!(f, "connection error: {text}"),
            DisconnectReason::Refused { reason, text } => {
                if text.is_empty() {
                    write!(f, "the host refused the connection ({reason})")
                } else {
                    write!(f, "the host refused the connection: {text}")
                }
            }
            DisconnectReason::ConnectFailed(text) => write!(f, "could not connect: {text}"),
        }
    }
}

/// An error from a session call.
#[derive(Debug)]
pub enum PlayTogetherError {
    /// The host could not bind its listening socket.
    Bind {
        /// The `address:port` we tried.
        address: String,
        /// The OS error.
        source: std::io::Error,
    },
    /// The join code does not parse.
    JoinCode(JoinCodeError),
    /// The TCP connection could not be made.
    Connect {
        /// The join code we tried.
        code: String,
        /// The OS error.
        source: std::io::Error,
    },
    /// A timeout in the given phase.
    Timeout(Phase),
    /// The host refused our `Hello`.
    Refused {
        /// The machine-readable reason.
        reason: RefusalReason,
        /// The host's explanation.
        text: String,
    },
    /// The other side sent something that does not decode.
    Protocol(DecodeError),
    /// A socket error.
    Io(std::io::Error),
    /// Only the host may do this.
    NotHost,
    /// The session is not (or no longer) connected.
    NotConnected,
    /// No participant has that id.
    NoSuchPeer(PeerId),
    /// The session has ended.
    Disconnected(DisconnectReason),
}

impl fmt::Display for PlayTogetherError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlayTogetherError::Bind { address, source } => write!(f, "could not listen on {address}: {source}"),
            PlayTogetherError::JoinCode(e) => write!(f, "bad join code: {e}"),
            PlayTogetherError::Connect { code, source } => write!(f, "could not connect to {code}: {source}"),
            PlayTogetherError::Timeout(phase) => write!(f, "timed out ({phase})"),
            PlayTogetherError::Refused { reason, text } => {
                if text.is_empty() {
                    write!(f, "the host refused the connection ({reason})")
                } else {
                    write!(f, "the host refused the connection: {text}")
                }
            }
            PlayTogetherError::Protocol(e) => write!(f, "protocol error: {e}"),
            PlayTogetherError::Io(e) => write!(f, "connection error: {e}"),
            PlayTogetherError::NotHost => f.write_str("only the host can do that"),
            PlayTogetherError::NotConnected => f.write_str("not connected to a session"),
            PlayTogetherError::NoSuchPeer(id) => write!(f, "no participant has id {id}"),
            PlayTogetherError::Disconnected(reason) => write!(f, "disconnected: {reason}"),
        }
    }
}

impl std::error::Error for PlayTogetherError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PlayTogetherError::Bind { source, .. } | PlayTogetherError::Connect { source, .. } => Some(source),
            PlayTogetherError::JoinCode(e) => Some(e),
            PlayTogetherError::Protocol(e) => Some(e),
            PlayTogetherError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<JoinCodeError> for PlayTogetherError {
    fn from(e: JoinCodeError) -> Self {
        PlayTogetherError::JoinCode(e)
    }
}

impl From<DecodeError> for PlayTogetherError {
    fn from(e: DecodeError) -> Self {
        PlayTogetherError::Protocol(e)
    }
}

impl From<std::io::Error> for PlayTogetherError {
    fn from(e: std::io::Error) -> Self {
        PlayTogetherError::Io(e)
    }
}

/// Why a `PublisherHandle` call could not queue its data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PublishError {
    /// Not connected yet (a client before `Connected`).
    NotConnected,
    /// The session has ended.
    Disconnected(DisconnectReason),
    /// The outbound queue overflowed; the session is now disconnecting with `TooSlow`.
    QueueFull {
        /// Bytes waiting in the queue when it overflowed.
        queued_bytes: u64,
    },
}

impl fmt::Display for PublishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PublishError::NotConnected => f.write_str("not connected to a session"),
            PublishError::Disconnected(reason) => write!(f, "disconnected: {reason}"),
            PublishError::QueueFull { queued_bytes } => {
                write!(f, "outbound queue is full ({queued_bytes} bytes waiting)")
            }
        }
    }
}

impl std::error::Error for PublishError {}
