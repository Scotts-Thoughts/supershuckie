//! The wire protocol, version 4. The contract is `docs/play-together-protocol.md`; this module is
//! a direct transcription of it.

use std::fmt;
use std::io::{self, Read};
use std::num::NonZeroU16;

use supershuckie_replay_recorder::replay_file::{ReplayConsoleType, ReplayFileMetadata, ReplayPatchFormat};
use supershuckie_replay_recorder::{InputBuffer, Speed};

use crate::error::DecodeError;
use crate::color::{is_valid_color, PlayerColor};
use crate::{Blake3Hash, PeerId, SessionId, MAX_PARTICIPANTS};

pub mod link;
pub mod snapshot;
pub mod stream;
pub mod wire;

pub use link::{decode_link_events, encode_link_events, LinkDeclineReason, LinkMessage, UnlinkReason, MAX_LINK_DELAY, MAX_LINK_EVENT_BYTES};
pub use snapshot::{compress_state, SnapshotData, StartStateData, StateEncoding, WireSnapshot, WireStartState, MAX_STATE_LENGTH, SNAPSHOT_ZSTD_LEVEL};
pub use stream::{count_frames, decode_packets, encode_packets, STREAM_MERGE_LIMIT};
pub use wire::{max_message_length, read_frame, read_frame_blocking, ReadError, MAX_MESSAGE_LENGTH, MAX_SMALL_MESSAGE_LENGTH, READ_CHUNK};

use link::pair_hash_fields;
use wire::{frame, Decoder, Encoder};

/// The protocol version this crate speaks.
pub const PROTOCOL_VERSION: u32 = 4;

/// Longest display name, in bytes.
pub const MAX_DISPLAY_NAME_BYTES: usize = 32;
/// Longest metadata string (ROM name, ROM file name, core name), in bytes: the replay header's
/// own limit.
pub const MAX_METADATA_STRING_BYTES: usize = 255;
/// Longest `app_version` string, in bytes.
pub const MAX_APP_VERSION_BYTES: usize = 64;
/// Longest free text (`Refused.text`, `Error.text`), in bytes.
pub const MAX_TEXT_BYTES: usize = 1024;
/// Longest input buffer, in bytes.
pub const MAX_INPUT_BYTES: usize = 64;
/// Most counters in a snapshot.
pub const MAX_COUNTERS: usize = 256;
/// Longest counter name, in bytes.
pub const MAX_COUNTER_NAME_BYTES: usize = 255;

// Tags.
pub(crate) const TAG_HELLO: u8 = 0x01;
pub(crate) const TAG_WELCOME: u8 = 0x02;
pub(crate) const TAG_REFUSED: u8 = 0x03;
pub(crate) const TAG_PEER_JOINED: u8 = 0x04;
pub(crate) const TAG_PEER_LEFT: u8 = 0x05;
pub(crate) const TAG_RESET_ALL: u8 = 0x06;
pub(crate) const TAG_SYNC_PAUSE: u8 = 0x07;
pub(crate) const TAG_PAUSE: u8 = 0x08;
pub(crate) const TAG_STREAM: u8 = 0x10;
pub(crate) const TAG_SNAPSHOT: u8 = 0x11;
pub(crate) const TAG_SYNC_HASH: u8 = 0x12;
pub(crate) const TAG_REQUEST_SNAPSHOT: u8 = 0x13;
pub(crate) const TAG_START_STATE: u8 = 0x14;
pub(crate) const TAG_PING: u8 = 0x20;
pub(crate) const TAG_PONG: u8 = 0x21;
pub(crate) const TAG_GOODBYE: u8 = 0x22;
pub(crate) const TAG_ERROR: u8 = 0x2F;
pub(crate) const TAG_LINK_REQUEST: u8 = 0x30;
pub(crate) const TAG_LINK_ACCEPT: u8 = 0x31;
pub(crate) const TAG_LINK_DECLINE: u8 = 0x32;
pub(crate) const TAG_LINK_START: u8 = 0x33;
pub(crate) const TAG_LINK_FRAME: u8 = 0x34;
pub(crate) const TAG_UNLINK: u8 = 0x35;
pub(crate) const TAG_PEER_LINKED: u8 = 0x36;
pub(crate) const TAG_PEER_UNLINKED: u8 = 0x37;

/// Byte offset, in a whole frame (length prefix included), of the first `u16` peer-id field of
/// `Stream`, `Snapshot`, `SyncHash`, `Pause`, every link cable message (`from`) and
/// `RequestSnapshot` (`requester`). The host overwrites it in place before relaying.
pub(crate) const PEER_ID_OFFSET: usize = 5;

/// What a participant publishes: its replay metadata and where its stream starts.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "serde_impl::PublisherInfoRepr", into = "serde_impl::PublisherInfoRepr"))]
pub struct PublisherInfo {
    /// Console, ROM, core, BIOS and patch information. Crops and the timer offset are not sent
    /// on the wire and are `None` on receipt.
    pub metadata: ReplayFileMetadata,
    /// The input held when the session was published.
    pub initial_input: InputBuffer,
    /// The speed when the session was published.
    pub speed: Speed,
    /// The publisher's frame count when the session was published.
    pub frame: u64,
}

/// One participant as everyone sees it.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ParticipantInfo {
    /// The id the host assigned (1 for the host itself).
    pub peer_id: PeerId,
    /// The (sanitized, deduplicated) display name.
    pub display_name: String,
    /// The colour the host gave it (a palette index, never 0).
    pub color: PlayerColor,
    /// The participant's application version string.
    pub app_version: String,
    /// What it publishes.
    pub publisher: PublisherInfo,
}

/// What the local application brings to a session.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct LocalParticipant {
    /// The requested display name (sanitized by the session; deduplicated by the host).
    pub display_name: String,
    /// The requested colour (0 = any); the host gives another when it is taken.
    pub color: PlayerColor,
    /// The application version string.
    pub app_version: String,
    /// What we publish.
    pub publisher: PublisherInfo,
}

/// Why a host refused a `Hello`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[repr(u32)]
pub enum RefusalReason {
    /// The client speaks another protocol version.
    ProtocolVersion = 0,
    /// The client writes another replay format version.
    ReplayVersion = 1,
    /// The session already has its maximum number of participants.
    SessionFull = 2,
    /// The client's console is not supported (Nintendo DS unless the host allows it; Unknown).
    ConsoleUnsupported = 3,
    /// The client is playing a patched ROM.
    PatchedRomUnsupported = 4,
    /// The `Hello` was malformed or arrived twice.
    BadHello = 5,
    /// The host is shutting down.
    HostShuttingDown = 6,
    /// Everyone starts from the host's save state, and the client's console, ROM, core or BIOS
    /// differs from the host's.
    StartStateMismatch = 7,
}

impl TryFrom<u32> for RefusalReason {
    type Error = DecodeError;
    fn try_from(value: u32) -> Result<Self, DecodeError> {
        Ok(match value {
            0 => RefusalReason::ProtocolVersion,
            1 => RefusalReason::ReplayVersion,
            2 => RefusalReason::SessionFull,
            3 => RefusalReason::ConsoleUnsupported,
            4 => RefusalReason::PatchedRomUnsupported,
            5 => RefusalReason::BadHello,
            6 => RefusalReason::HostShuttingDown,
            7 => RefusalReason::StartStateMismatch,
            other => return Err(DecodeError::BadEnum { what: "refusal reason", value: other }),
        })
    }
}

impl From<RefusalReason> for u32 {
    fn from(r: RefusalReason) -> u32 {
        r as u32
    }
}

impl fmt::Display for RefusalReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            RefusalReason::ProtocolVersion => "protocol version mismatch",
            RefusalReason::ReplayVersion => "replay format version mismatch",
            RefusalReason::SessionFull => "the session is full",
            RefusalReason::ConsoleUnsupported => "console not supported",
            RefusalReason::PatchedRomUnsupported => "patched ROMs are not supported",
            RefusalReason::BadHello => "bad hello",
            RefusalReason::HostShuttingDown => "the host is shutting down",
            RefusalReason::StartStateMismatch => "your game differs from the host's, whose save state everyone starts from",
        })
    }
}

/// Why a participant is no longer in the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[repr(u32)]
pub enum LeaveReason {
    /// It sent `Goodbye`.
    Left = 0,
    /// Nothing arrived from it in time.
    Timeout = 1,
    /// It could not keep up with what was sent to it.
    TooSlow = 2,
    /// It broke the protocol.
    ProtocolError = 3,
    /// Its socket failed.
    IoError = 4,
    /// The host removed it.
    Kicked = 5,
    /// The host ended the session.
    HostLeft = 6,
}

impl TryFrom<u32> for LeaveReason {
    type Error = DecodeError;
    fn try_from(value: u32) -> Result<Self, DecodeError> {
        Ok(match value {
            0 => LeaveReason::Left,
            1 => LeaveReason::Timeout,
            2 => LeaveReason::TooSlow,
            3 => LeaveReason::ProtocolError,
            4 => LeaveReason::IoError,
            5 => LeaveReason::Kicked,
            6 => LeaveReason::HostLeft,
            other => return Err(DecodeError::BadEnum { what: "leave reason", value: other }),
        })
    }
}

impl From<LeaveReason> for u32 {
    fn from(r: LeaveReason) -> u32 {
        r as u32
    }
}

impl fmt::Display for LeaveReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            LeaveReason::Left => "left",
            LeaveReason::Timeout => "timed out",
            LeaveReason::TooSlow => "could not keep up",
            LeaveReason::ProtocolError => "protocol error",
            LeaveReason::IoError => "connection error",
            LeaveReason::Kicked => "removed by the host",
            LeaveReason::HostLeft => "the host ended the session",
        })
    }
}

/// Every message of the protocol.
#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    /// Client → host, first thing after connecting.
    Hello {
        /// Must equal [`PROTOCOL_VERSION`].
        protocol_version: u32,
        /// Must equal the host's `REPLAY_VERSION`.
        replay_version: u32,
        /// The client's application version string.
        app_version: String,
        /// The requested display name.
        display_name: String,
        /// The requested colour (0 = any).
        color: PlayerColor,
        /// What the client publishes.
        publisher: PublisherInfo,
    },
    /// Host → client, admitting it.
    Welcome {
        /// The client's id.
        your_peer_id: PeerId,
        /// The session's id.
        session_id: SessionId,
        /// The client's display name after sanitizing and deduplication.
        your_display_name: String,
        /// The colour the host gave the client.
        your_color: PlayerColor,
        /// Everyone already in the session, the host first.
        participants: Vec<ParticipantInfo>,
    },
    /// Host → client, declining it; the host closes afterwards.
    Refused {
        /// Why.
        reason: RefusalReason,
        /// A sentence for the user.
        text: String,
    },
    /// Host → clients: someone was admitted.
    PeerJoined {
        /// Who.
        participant: ParticipantInfo,
    },
    /// Host → clients: someone is gone. Sent to a kicked client about itself, too.
    PeerLeft {
        /// Who.
        peer_id: PeerId,
        /// Why.
        reason: LeaveReason,
    },
    /// Host → clients: everybody resets when the countdown ends.
    ResetAll {
        /// Distinguishes one reset from the next.
        race_id: u32,
        /// How long from receipt until the reset.
        countdown_millis: u32,
    },
    /// Host → clients: whether pausing is shared, and the session's pause state to adopt.
    /// Sent right after `Welcome` and whenever the host changes the setting.
    SyncPause {
        /// Whether one participant's pause pauses everyone.
        enabled: bool,
        /// Whether the session is paused right now (meaningful while `enabled`).
        paused: bool,
    },
    /// A participant paused or unpaused everyone. Relayed by the host to everyone else only
    /// while sync pause is enabled.
    Pause {
        /// Who (a client sends 0; the host fills it in before relaying).
        from: PeerId,
        /// Paused, or unpaused.
        paused: bool,
    },
    /// A run of whole replay packets from `from`'s emulator.
    Stream {
        /// The publisher (rewritten by the host before relaying).
        from: PeerId,
        /// The publisher's frame count before the first `NextFrame` in `bytes`.
        first_frame: u64,
        /// Whole packets; see [`decode_packets`].
        bytes: Vec<u8>,
    },
    /// A publisher's full state, so a follower can start applying its stream.
    Snapshot(WireSnapshot),
    /// A hash of the publisher's state at `frame`, for followers to compare against.
    SyncHash {
        /// The publisher (rewritten by the host before relaying).
        from: PeerId,
        /// The frame the hash was taken at.
        frame: u64,
        /// The hash.
        hash: Blake3Hash,
    },
    /// Ask `target` for a snapshot. A client sends `requester = 0`; the host fills it in.
    RequestSnapshot {
        /// Who asks.
        requester: PeerId,
        /// Which publisher.
        target: PeerId,
    },
    /// Host → clients: the save state everyone's own game is loaded from, or (cleared) that
    /// there is none any more. Sent right after `Welcome` while one is set, and on every change.
    StartState(WireStartState),
    /// Keepalive and round-trip probe; answered with `Pong` carrying the same fields.
    Ping {
        /// Matches the `Pong`.
        nonce: u32,
        /// The sender's clock when it sent the ping.
        sent_unix_millis: u64,
    },
    /// The answer to a `Ping`.
    Pong {
        /// From the ping.
        nonce: u32,
        /// From the ping.
        sent_unix_millis: u64,
    },
    /// Leaving cleanly.
    Goodbye,
    /// The sender is closing because the receiver broke the protocol.
    Error {
        /// What went wrong.
        text: String,
    },
    /// `from` asks `target` to plug a link cable between their games. Relayed by the host,
    /// which declines on the target's behalf when either is already linked or the consoles'
    /// families differ.
    LinkRequest {
        /// The requester (rewritten by the host before relaying).
        from: PeerId,
        /// Who is asked.
        target: PeerId,
        /// Distinguishes this request from the requester's other ones.
        nonce: u32,
        /// The requester's console type (a `ReplayConsoleType` number).
        console: u32,
    },
    /// `from` accepts `target`'s request `nonce`. The host records the pair and tells everyone
    /// with `PeerLinked`.
    LinkAccept {
        /// Who accepts (rewritten by the host).
        from: PeerId,
        /// The requester.
        target: PeerId,
        /// The request.
        nonce: u32,
    },
    /// `from` declines `target`'s request `nonce` (or the host does, on `from`'s behalf).
    LinkDecline {
        /// Who declines (rewritten by the host).
        from: PeerId,
        /// The requester.
        target: PeerId,
        /// The request.
        nonce: u32,
        /// Why.
        reason: LinkDeclineReason,
    },
    /// Where `from`'s game stopped for the link; sent by both ends once they have paused.
    LinkStart {
        /// Who stopped (rewritten by the host).
        from: PeerId,
        /// The other end.
        target: PeerId,
        /// The request this link came from.
        nonce: u32,
        /// The sender's frame count at the hold.
        frame: u64,
        /// The input it holds there.
        input: InputBuffer,
        /// The sender's last round-trip time to the host, in milliseconds (0 for the host).
        rtt_millis: u32,
        /// The sender's input-delay setting: 0 for automatic, else the frames it wants at least.
        delay_setting: u8,
    },
    /// One lockstep frame of `from`'s events for `target`.
    LinkFrame {
        /// The sender (rewritten by the host).
        from: PeerId,
        /// The other end.
        target: PeerId,
        /// The link frame the events land on.
        frame: u64,
        /// The sender's recording clock (the `elapsed_millis` its snapshots and stream use) as
        /// it sent the frame, `delay` frames before `frame` runs.
        elapsed_millis: u64,
        /// Whole packets; see [`decode_link_events`].
        events: Vec<u8>,
        /// The frame `pair_hash` was taken at (meaningful when `pair_hash` is not all zero).
        pair_hash_frame: u64,
        /// A hash of both consoles' work RAM, or all zero for none.
        pair_hash: Blake3Hash,
    },
    /// `from` unplugs the cable to `target` (or the host does, when `from` left).
    Unlink {
        /// Who unplugs (rewritten by the host).
        from: PeerId,
        /// The other end.
        target: PeerId,
        /// Why.
        reason: UnlinkReason,
    },
    /// Host → clients: `a` and `b` are linked (for the roster).
    PeerLinked {
        /// One end.
        a: PeerId,
        /// The other.
        b: PeerId,
    },
    /// Host → clients: `a` and `b` are no longer linked.
    PeerUnlinked {
        /// One end.
        a: PeerId,
        /// The other.
        b: PeerId,
    },
}

impl Message {
    /// The message's tag byte.
    pub fn tag(&self) -> u8 {
        match self {
            Message::Hello { .. } => TAG_HELLO,
            Message::Welcome { .. } => TAG_WELCOME,
            Message::Refused { .. } => TAG_REFUSED,
            Message::PeerJoined { .. } => TAG_PEER_JOINED,
            Message::PeerLeft { .. } => TAG_PEER_LEFT,
            Message::ResetAll { .. } => TAG_RESET_ALL,
            Message::SyncPause { .. } => TAG_SYNC_PAUSE,
            Message::Pause { .. } => TAG_PAUSE,
            Message::Stream { .. } => TAG_STREAM,
            Message::Snapshot(_) => TAG_SNAPSHOT,
            Message::SyncHash { .. } => TAG_SYNC_HASH,
            Message::RequestSnapshot { .. } => TAG_REQUEST_SNAPSHOT,
            Message::StartState(_) => TAG_START_STATE,
            Message::Ping { .. } => TAG_PING,
            Message::Pong { .. } => TAG_PONG,
            Message::Goodbye => TAG_GOODBYE,
            Message::Error { .. } => TAG_ERROR,
            Message::LinkRequest { .. } => TAG_LINK_REQUEST,
            Message::LinkAccept { .. } => TAG_LINK_ACCEPT,
            Message::LinkDecline { .. } => TAG_LINK_DECLINE,
            Message::LinkStart { .. } => TAG_LINK_START,
            Message::LinkFrame { .. } => TAG_LINK_FRAME,
            Message::Unlink { .. } => TAG_UNLINK,
            Message::PeerLinked { .. } => TAG_PEER_LINKED,
            Message::PeerUnlinked { .. } => TAG_PEER_UNLINKED,
        }
    }

    /// Whether this is a link cable message relayed between the two ends of a link (everything
    /// but `PeerLinked` / `PeerUnlinked`, which the host broadcasts).
    pub fn is_link_relay(&self) -> bool {
        matches!(
            self,
            Message::LinkRequest { .. }
                | Message::LinkAccept { .. }
                | Message::LinkDecline { .. }
                | Message::LinkStart { .. }
                | Message::LinkFrame { .. }
                | Message::Unlink { .. }
        )
    }

    /// The `from` and `target` of a link cable message relayed between two ends.
    pub fn link_endpoints(&self) -> Option<(PeerId, PeerId)> {
        match self {
            Message::LinkRequest { from, target, .. }
            | Message::LinkAccept { from, target, .. }
            | Message::LinkDecline { from, target, .. }
            | Message::LinkStart { from, target, .. }
            | Message::LinkFrame { from, target, .. }
            | Message::Unlink { from, target, .. } => Some((*from, *target)),
            _ => None,
        }
    }

    /// A [`LinkMessage`] as the wire message `from` sends.
    pub fn from_link(from: PeerId, message: LinkMessage) -> Message {
        match message {
            LinkMessage::Request { target, nonce, console } => Message::LinkRequest { from, target, nonce, console },
            LinkMessage::Accept { target, nonce } => Message::LinkAccept { from, target, nonce },
            LinkMessage::Decline { target, nonce, reason } => Message::LinkDecline { from, target, nonce, reason },
            LinkMessage::Start { target, nonce, frame, input, rtt_millis, delay_setting } => {
                Message::LinkStart { from, target, nonce, frame, input, rtt_millis, delay_setting }
            }
            LinkMessage::Frame { target, frame, elapsed_millis, events, pair_hash } => {
                let (pair_hash_frame, pair_hash) = pair_hash_fields(pair_hash);
                Message::LinkFrame { from, target, frame, elapsed_millis, events: encode_link_events(&events), pair_hash_frame, pair_hash }
            }
            LinkMessage::Unlink { target, reason } => Message::Unlink { from, target, reason },
        }
    }

    /// Append the framed message (length, tag, payload) to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        frame(out, |e| {
            e.u8(self.tag());
            match self {
                Message::Hello { protocol_version, replay_version, app_version, display_name, color, publisher } => {
                    e.u32(*protocol_version);
                    e.u32(*replay_version);
                    e.string(app_version);
                    e.string(display_name);
                    e.u8(*color);
                    encode_publisher(e, publisher);
                }
                Message::Welcome { your_peer_id, session_id, your_display_name, your_color, participants } => {
                    e.u16(*your_peer_id);
                    e.u64(*session_id);
                    e.string(your_display_name);
                    e.u8(*your_color);
                    e.u32(participants.len() as u32);
                    for p in participants {
                        encode_participant(e, p);
                    }
                }
                Message::Refused { reason, text } => {
                    e.u32(u32::from(*reason));
                    e.string(text);
                }
                Message::PeerJoined { participant } => encode_participant(e, participant),
                Message::PeerLeft { peer_id, reason } => {
                    e.u16(*peer_id);
                    e.u32(u32::from(*reason));
                }
                Message::ResetAll { race_id, countdown_millis } => {
                    e.u32(*race_id);
                    e.u32(*countdown_millis);
                }
                Message::SyncPause { enabled, paused } => {
                    e.bool(*enabled);
                    e.bool(*paused);
                }
                Message::Pause { from, paused } => {
                    e.u16(*from);
                    e.bool(*paused);
                }
                Message::Stream { from, first_frame, bytes } => {
                    e.u16(*from);
                    e.u64(*first_frame);
                    e.bytes(bytes);
                }
                Message::Snapshot(s) => {
                    e.u16(s.from);
                    e.u16(s.target);
                    e.u64(s.frame);
                    e.u64(s.elapsed_millis);
                    e.bytes(&s.input);
                    e.u16(s.speed.speed_over_256.get());
                    e.u32(s.counters.len() as u32);
                    for (name, value) in &s.counters {
                        e.string(name);
                        e.i64(*value);
                    }
                    e.u8(s.encoding as u8);
                    e.u64(s.state_len);
                    e.bytes(&s.state);
                }
                Message::SyncHash { from, frame, hash } => {
                    e.u16(*from);
                    e.u64(*frame);
                    e.hash(hash);
                }
                Message::RequestSnapshot { requester, target } => {
                    e.u16(*requester);
                    e.u16(*target);
                }
                Message::StartState(s) => {
                    e.hash(&s.rom_checksum);
                    e.u8(s.encoding as u8);
                    e.u64(s.state_len);
                    e.bytes(&s.state);
                }
                Message::Ping { nonce, sent_unix_millis } | Message::Pong { nonce, sent_unix_millis } => {
                    e.u32(*nonce);
                    e.u64(*sent_unix_millis);
                }
                Message::Goodbye => {}
                Message::Error { text } => e.string(text),
                Message::LinkRequest { from, target, nonce, console } => {
                    e.u16(*from);
                    e.u16(*target);
                    e.u32(*nonce);
                    e.u32(*console);
                }
                Message::LinkAccept { from, target, nonce } => {
                    e.u16(*from);
                    e.u16(*target);
                    e.u32(*nonce);
                }
                Message::LinkDecline { from, target, nonce, reason } => {
                    e.u16(*from);
                    e.u16(*target);
                    e.u32(*nonce);
                    e.u32(u32::from(*reason));
                }
                Message::LinkStart { from, target, nonce, frame, input, rtt_millis, delay_setting } => {
                    e.u16(*from);
                    e.u16(*target);
                    e.u32(*nonce);
                    e.u64(*frame);
                    e.bytes(input);
                    e.u32(*rtt_millis);
                    e.u8(*delay_setting);
                }
                Message::LinkFrame { from, target, frame, elapsed_millis, events, pair_hash_frame, pair_hash } => {
                    e.u16(*from);
                    e.u16(*target);
                    e.u64(*frame);
                    e.u64(*elapsed_millis);
                    e.bytes(events);
                    e.u64(*pair_hash_frame);
                    e.hash(pair_hash);
                }
                Message::Unlink { from, target, reason } => {
                    e.u16(*from);
                    e.u16(*target);
                    e.u32(u32::from(*reason));
                }
                Message::PeerLinked { a, b } | Message::PeerUnlinked { a, b } => {
                    e.u16(*a);
                    e.u16(*b);
                }
            }
        });
    }

    /// The framed message as a fresh buffer.
    pub fn encoded(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode(&mut out);
        out
    }

    /// Decode one message body (tag + payload, without the length prefix).
    pub fn decode(body: &[u8]) -> Result<Message, DecodeError> {
        let mut d = Decoder::new(body);
        let tag = d.u8()?;
        let message = match tag {
            TAG_HELLO => Message::Hello {
                protocol_version: d.u32()?,
                replay_version: d.u32()?,
                app_version: d.string("app_version", MAX_APP_VERSION_BYTES)?,
                display_name: d.string("display_name", MAX_DISPLAY_NAME_BYTES)?,
                color: d.u8()?,
                publisher: decode_publisher(&mut d)?,
            },
            TAG_WELCOME => {
                let your_peer_id = peer_id(&mut d)?;
                let session_id = d.u64()?;
                let your_display_name = d.string("display_name", MAX_DISPLAY_NAME_BYTES)?;
                let your_color = assigned_color(&mut d)?;
                let n = d.count("participants", MAX_PARTICIPANTS, PARTICIPANT_MIN_BYTES)?;
                let mut participants = Vec::with_capacity(n);
                for _ in 0..n {
                    participants.push(decode_participant(&mut d)?);
                }
                Message::Welcome { your_peer_id, session_id, your_display_name, your_color, participants }
            }
            TAG_REFUSED => Message::Refused { reason: RefusalReason::try_from(d.u32()?)?, text: d.string("text", MAX_TEXT_BYTES)? },
            TAG_PEER_JOINED => Message::PeerJoined { participant: decode_participant(&mut d)? },
            TAG_PEER_LEFT => Message::PeerLeft { peer_id: peer_id(&mut d)?, reason: LeaveReason::try_from(d.u32()?)? },
            TAG_RESET_ALL => Message::ResetAll { race_id: d.u32()?, countdown_millis: d.u32()? },
            TAG_SYNC_PAUSE => Message::SyncPause { enabled: d.bool()?, paused: d.bool()? },
            TAG_PAUSE => Message::Pause { from: d.u16()?, paused: d.bool()? },
            TAG_STREAM => Message::Stream {
                from: peer_id(&mut d)?,
                first_frame: d.u64()?,
                bytes: d.bytes("stream", MAX_MESSAGE_LENGTH as usize)?.to_vec(),
            },
            TAG_SNAPSHOT => {
                let from = peer_id(&mut d)?;
                let target = d.u16()?;
                let frame = d.u64()?;
                let elapsed_millis = d.u64()?;
                let input = input_buffer(&mut d)?;
                let speed = speed(&mut d)?;
                let counters = counters(&mut d)?;
                let encoding = StateEncoding::try_from(d.u8()?)?;
                let state_len = d.u64()?;
                if state_len > MAX_STATE_LENGTH {
                    return Err(DecodeError::StateTooLarge(state_len));
                }
                let state = d.bytes("state", MAX_MESSAGE_LENGTH as usize)?.to_vec();
                Message::Snapshot(WireSnapshot { from, target, frame, elapsed_millis, input, speed, counters, encoding, state_len, state })
            }
            TAG_SYNC_HASH => Message::SyncHash { from: peer_id(&mut d)?, frame: d.u64()?, hash: d.hash()? },
            TAG_REQUEST_SNAPSHOT => Message::RequestSnapshot { requester: d.u16()?, target: d.u16()? },
            TAG_START_STATE => {
                let rom_checksum = d.hash()?;
                let encoding = StateEncoding::try_from(d.u8()?)?;
                let state_len = d.u64()?;
                if state_len > MAX_STATE_LENGTH {
                    return Err(DecodeError::StateTooLarge(state_len));
                }
                let state = d.bytes("state", MAX_MESSAGE_LENGTH as usize)?.to_vec();
                Message::StartState(WireStartState { rom_checksum, encoding, state_len, state })
            }
            TAG_PING => Message::Ping { nonce: d.u32()?, sent_unix_millis: d.u64()? },
            TAG_PONG => Message::Pong { nonce: d.u32()?, sent_unix_millis: d.u64()? },
            TAG_GOODBYE => Message::Goodbye,
            TAG_ERROR => Message::Error { text: d.string("text", MAX_TEXT_BYTES)? },
            TAG_LINK_REQUEST => Message::LinkRequest { from: d.u16()?, target: peer_id(&mut d)?, nonce: d.u32()?, console: d.u32()? },
            TAG_LINK_ACCEPT => Message::LinkAccept { from: d.u16()?, target: peer_id(&mut d)?, nonce: d.u32()? },
            TAG_LINK_DECLINE => Message::LinkDecline { from: d.u16()?, target: peer_id(&mut d)?, nonce: d.u32()?, reason: LinkDeclineReason::from(d.u32()?) },
            TAG_LINK_START => {
                let from = d.u16()?;
                let target = peer_id(&mut d)?;
                let nonce = d.u32()?;
                let frame = d.u64()?;
                let input = input_buffer(&mut d)?;
                let rtt_millis = d.u32()?;
                let delay_setting = d.u8()?;
                if delay_setting > MAX_LINK_DELAY {
                    return Err(DecodeError::BadEnum { what: "link delay setting", value: u32::from(delay_setting) });
                }
                Message::LinkStart { from, target, nonce, frame, input, rtt_millis, delay_setting }
            }
            TAG_LINK_FRAME => {
                let from = d.u16()?;
                let target = peer_id(&mut d)?;
                let frame = d.u64()?;
                let elapsed_millis = d.u64()?;
                let events = d.bytes("link events", MAX_LINK_EVENT_BYTES)?.to_vec();
                let pair_hash_frame = d.u64()?;
                let pair_hash = d.hash()?;
                Message::LinkFrame { from, target, frame, elapsed_millis, events, pair_hash_frame, pair_hash }
            }
            TAG_UNLINK => Message::Unlink { from: d.u16()?, target: peer_id(&mut d)?, reason: UnlinkReason::from(d.u32()?) },
            TAG_PEER_LINKED => Message::PeerLinked { a: peer_id(&mut d)?, b: peer_id(&mut d)? },
            TAG_PEER_UNLINKED => Message::PeerUnlinked { a: peer_id(&mut d)?, b: peer_id(&mut d)? },
            other => return Err(DecodeError::UnknownTag(other)),
        };
        d.finish()?;
        Ok(message)
    }

    /// Decode a whole frame (length prefix included), as [`read_frame`] returns it.
    pub fn decode_frame(frame: &[u8]) -> Result<Message, DecodeError> {
        match frame.get(4..) {
            Some(body) => Message::decode(body),
            None => Err(DecodeError::Truncated),
        }
    }

    /// Read one framed message from `r`. `Ok(None)` at a clean end of input. An unknown tag comes
    /// back as `Err(UnknownTag)` with its body consumed, so the caller can skip it and go on; an
    /// unacceptable length prefix comes back as `Err(TooLong)` (the stream is then unusable).
    pub fn read(r: &mut impl Read) -> io::Result<Option<Result<Message, DecodeError>>> {
        match read_frame_blocking(r) {
            Ok(None) => Ok(None),
            Ok(Some(frame)) => Ok(Some(Message::decode_frame(&frame))),
            Err(ReadError::Io(e)) => Err(e),
            Err(ReadError::Decode(e)) => Ok(Some(Err(e))),
            Err(ReadError::Stopped(never)) => match never {},
        }
    }
}

/// The fewest bytes a `ParticipantInfo` can occupy on the wire (used to refuse absurd counts
/// before allocating).
const PARTICIPANT_MIN_BYTES: usize = 2 + 4 + 1 + 4 + PUBLISHER_MIN_BYTES;
const PUBLISHER_MIN_BYTES: usize = 4 + 4 + 4 + 32 + 32 + 4 + 4 + 32 + 4 + 2 + 8;

/// A colour the host assigned: must name a palette entry.
fn assigned_color(d: &mut Decoder<'_>) -> Result<PlayerColor, DecodeError> {
    let color = d.u8()?;
    if is_valid_color(color) {
        Ok(color)
    }
    else {
        Err(DecodeError::BadColor(color))
    }
}

fn peer_id(d: &mut Decoder<'_>) -> Result<PeerId, DecodeError> {
    match d.u16()? {
        0 => Err(DecodeError::ZeroPeerId),
        id => Ok(id),
    }
}

fn speed(d: &mut Decoder<'_>) -> Result<Speed, DecodeError> {
    NonZeroU16::new(d.u16()?).map(|speed_over_256| Speed { speed_over_256 }).ok_or(DecodeError::BadSpeed)
}

fn input_buffer(d: &mut Decoder<'_>) -> Result<InputBuffer, DecodeError> {
    Ok(d.bytes("input", MAX_INPUT_BYTES)?.iter().copied().collect())
}

fn counters(d: &mut Decoder<'_>) -> Result<Vec<(String, i64)>, DecodeError> {
    let n = d.count("counters", MAX_COUNTERS, 4 + 8)?;
    let mut counters = Vec::with_capacity(n);
    for _ in 0..n {
        let name = d.string("counter name", MAX_COUNTER_NAME_BYTES)?;
        let value = d.i64()?;
        counters.push((name, value));
    }
    Ok(counters)
}

fn encode_publisher(e: &mut Encoder<'_>, p: &PublisherInfo) {
    let m = &p.metadata;
    e.u32(u32::from(m.console_type));
    e.string(&m.rom_name);
    e.string(&m.rom_filename);
    e.hash(&m.rom_checksum);
    e.hash(&m.bios_checksum);
    e.string(&m.emulator_core_name);
    e.u32(u32::from(m.patch_format));
    e.hash(&m.patch_target_checksum);
    e.bytes(&p.initial_input);
    e.u16(p.speed.speed_over_256.get());
    e.u64(p.frame);
}

fn decode_publisher(d: &mut Decoder<'_>) -> Result<PublisherInfo, DecodeError> {
    let console = d.u32()?;
    let console_type = ReplayConsoleType::try_from(console).map_err(|_| DecodeError::BadEnum { what: "console type", value: console })?;
    let rom_name = d.string("rom_name", MAX_METADATA_STRING_BYTES)?;
    let rom_filename = d.string("rom_filename", MAX_METADATA_STRING_BYTES)?;
    let rom_checksum = d.hash()?;
    let bios_checksum = d.hash()?;
    let emulator_core_name = d.string("emulator_core_name", MAX_METADATA_STRING_BYTES)?;
    let patch = d.u32()?;
    let patch_format = ReplayPatchFormat::try_from(patch).map_err(|_| DecodeError::BadEnum { what: "patch format", value: patch })?;
    let patch_target_checksum = d.hash()?;
    let initial_input = input_buffer(d)?;
    let speed = speed(d)?;
    let frame = d.u64()?;
    Ok(PublisherInfo {
        metadata: ReplayFileMetadata {
            console_type,
            rom_name,
            rom_filename,
            rom_checksum,
            bios_checksum,
            emulator_core_name,
            patch_format,
            patch_target_checksum,
            crop_start: None,
            crop_end: None,
            timer_offset: None,
        },
        initial_input,
        speed,
        frame,
    })
}

fn encode_participant(e: &mut Encoder<'_>, p: &ParticipantInfo) {
    e.u16(p.peer_id);
    e.string(&p.display_name);
    e.u8(p.color);
    e.string(&p.app_version);
    encode_publisher(e, &p.publisher);
}

fn decode_participant(d: &mut Decoder<'_>) -> Result<ParticipantInfo, DecodeError> {
    Ok(ParticipantInfo {
        peer_id: peer_id(d)?,
        display_name: d.string("display_name", MAX_DISPLAY_NAME_BYTES)?,
        color: assigned_color(d)?,
        app_version: d.string("app_version", MAX_APP_VERSION_BYTES)?,
        publisher: decode_publisher(d)?,
    })
}

#[cfg(feature = "serde")]
mod serde_impl {
    use serde::{Deserialize, Serialize};
    use supershuckie_replay_recorder::replay_file::{ReplayConsoleType, ReplayFileMetadata, ReplayPatchFormat};
    use supershuckie_replay_recorder::Speed;

    use super::PublisherInfo;

    /// `PublisherInfo` with the recorder's types flattened to plain numbers and byte arrays.
    #[derive(Serialize, Deserialize)]
    pub struct PublisherInfoRepr {
        pub console_type: u32,
        pub rom_name: String,
        pub rom_filename: String,
        pub rom_checksum: [u8; 32],
        pub bios_checksum: [u8; 32],
        pub emulator_core_name: String,
        pub patch_format: u32,
        pub patch_target_checksum: [u8; 32],
        pub initial_input: Vec<u8>,
        pub speed_over_256: u16,
        pub frame: u64,
    }

    impl From<PublisherInfo> for PublisherInfoRepr {
        fn from(p: PublisherInfo) -> Self {
            let m = p.metadata;
            PublisherInfoRepr {
                console_type: u32::from(m.console_type),
                rom_name: m.rom_name,
                rom_filename: m.rom_filename,
                rom_checksum: m.rom_checksum,
                bios_checksum: m.bios_checksum,
                emulator_core_name: m.emulator_core_name,
                patch_format: u32::from(m.patch_format),
                patch_target_checksum: m.patch_target_checksum,
                initial_input: p.initial_input.to_vec(),
                speed_over_256: p.speed.speed_over_256.get(),
                frame: p.frame,
            }
        }
    }

    impl TryFrom<PublisherInfoRepr> for PublisherInfo {
        type Error = String;
        fn try_from(r: PublisherInfoRepr) -> Result<Self, String> {
            let console_type = ReplayConsoleType::try_from(r.console_type).map_err(|_| format!("unknown console type {}", r.console_type))?;
            let patch_format = ReplayPatchFormat::try_from(r.patch_format).map_err(|_| format!("unknown patch format {}", r.patch_format))?;
            let speed_over_256 = std::num::NonZeroU16::new(r.speed_over_256).ok_or_else(|| "speed of zero".to_owned())?;
            Ok(PublisherInfo {
                metadata: ReplayFileMetadata {
                    console_type,
                    rom_name: r.rom_name,
                    rom_filename: r.rom_filename,
                    rom_checksum: r.rom_checksum,
                    bios_checksum: r.bios_checksum,
                    emulator_core_name: r.emulator_core_name,
                    patch_format,
                    patch_target_checksum: r.patch_target_checksum,
                    crop_start: None,
                    crop_end: None,
                    timer_offset: None,
                },
                initial_input: r.initial_input.into_iter().collect(),
                speed: Speed { speed_over_256 },
                frame: r.frame,
            })
        }
    }
}
