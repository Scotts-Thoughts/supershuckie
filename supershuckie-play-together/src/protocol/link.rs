//! The link cable messages (tags `0x30`–`0x37`): their reason codes, the packet allow-list of
//! `LinkFrame.events`, and [`LinkMessage`], what the application sends through
//! `Session::send_link`.
//!
//! Two participants that follow each other can plug a link cable between their games. Each
//! side then runs the other's game (the follower it already has) in delay-based lockstep with
//! its own: every input, RAM write and reset is scheduled `delay` frames ahead and sent to the
//! partner in a `LinkFrame`, so both machines feed both consoles the same events on the same
//! frames. The cable itself never crosses the network; only the events do.

use std::fmt;

use supershuckie_replay_recorder::replay_file::REPLAY_VERSION;
use supershuckie_replay_recorder::{append_packet, InputBuffer, Packet, PacketIO};

use crate::error::DecodeError;
use crate::{Blake3Hash, PeerId};

/// Longest `LinkFrame.events`, in bytes. A frame normally carries an input change or nothing;
/// a RAM tool write is the largest thing it can hold.
pub const MAX_LINK_EVENT_BYTES: usize = 32 << 10;

/// Longest input delay, in frames, either side may ask for.
pub const MAX_LINK_DELAY: u8 = 15;

/// Why a link request was declined (`LinkDecline.reason`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[repr(u32)]
pub enum LinkDeclineReason {
    /// The other player said no.
    Declined = 0,
    /// One of the two is already linked (the host refuses on their behalf).
    Busy = 1,
    /// The two games are on different console families (the host refuses on their behalf).
    ConsoleMismatch = 2,
    /// The other player is not following the requester's game closely enough to link.
    NotFollowing = 3,
    /// The other player did not answer in time.
    Timeout = 4,
    /// The other player cannot link right now (a replay is attached, the console cannot link,
    /// the target is gone).
    Unavailable = 5,
    /// A reason this version does not know.
    Other = u32::MAX,
}

impl From<u32> for LinkDeclineReason {
    fn from(value: u32) -> Self {
        match value {
            0 => LinkDeclineReason::Declined,
            1 => LinkDeclineReason::Busy,
            2 => LinkDeclineReason::ConsoleMismatch,
            3 => LinkDeclineReason::NotFollowing,
            4 => LinkDeclineReason::Timeout,
            5 => LinkDeclineReason::Unavailable,
            _ => LinkDeclineReason::Other,
        }
    }
}

impl From<LinkDeclineReason> for u32 {
    fn from(r: LinkDeclineReason) -> u32 {
        r as u32
    }
}

impl fmt::Display for LinkDeclineReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            LinkDeclineReason::Declined => "declined",
            LinkDeclineReason::Busy => "already linked with someone else",
            LinkDeclineReason::ConsoleMismatch => "a different console",
            LinkDeclineReason::NotFollowing => "not following your game closely enough",
            LinkDeclineReason::Timeout => "no answer",
            LinkDeclineReason::Unavailable => "cannot link right now",
            LinkDeclineReason::Other => "unknown reason",
        })
    }
}

/// Why a link cable was unplugged (`Unlink.reason`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[repr(u32)]
pub enum UnlinkReason {
    /// A player unplugged it.
    Unplugged = 0,
    /// The two machines' games diverged (the pair hash differed).
    Desync = 1,
    /// One side waited too long for the other's frames.
    Timeout = 2,
    /// The other player left the session (sent by the host).
    PeerLeft = 3,
    /// The link could not be started, or an emulator error ended it.
    Failed = 4,
    /// The host found one of the two already linked when the accept arrived.
    Busy = 5,
    /// A reason this version does not know.
    Other = u32::MAX,
}

impl From<u32> for UnlinkReason {
    fn from(value: u32) -> Self {
        match value {
            0 => UnlinkReason::Unplugged,
            1 => UnlinkReason::Desync,
            2 => UnlinkReason::Timeout,
            3 => UnlinkReason::PeerLeft,
            4 => UnlinkReason::Failed,
            5 => UnlinkReason::Busy,
            _ => UnlinkReason::Other,
        }
    }
}

impl From<UnlinkReason> for u32 {
    fn from(r: UnlinkReason) -> u32 {
        r as u32
    }
}

impl fmt::Display for UnlinkReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            UnlinkReason::Unplugged => "unplugged",
            UnlinkReason::Desync => "the games went out of sync",
            UnlinkReason::Timeout => "the other side stopped answering",
            UnlinkReason::PeerLeft => "the other player left",
            UnlinkReason::Failed => "the link failed",
            UnlinkReason::Busy => "already linked with someone else",
            UnlinkReason::Other => "unknown reason",
        })
    }
}

/// A link cable message as the application sends it (`Session::send_link`). `from` is filled
/// in by the session; `target` is the other end.
#[derive(Clone, Debug, PartialEq)]
pub enum LinkMessage {
    /// Ask `target` to plug a cable between the two games. `console` is our console type (the
    /// `ReplayConsoleType` number), so the other side can tell without a lookup.
    Request {
        /// Who to link with.
        target: PeerId,
        /// Distinguishes this request from every other one we send.
        nonce: u32,
        /// Our console type.
        console: u32,
    },
    /// Accept `target`'s request `nonce`.
    Accept {
        /// The requester.
        target: PeerId,
        /// Its request.
        nonce: u32,
    },
    /// Decline `target`'s request `nonce`.
    Decline {
        /// The requester.
        target: PeerId,
        /// Its request.
        nonce: u32,
        /// Why.
        reason: LinkDeclineReason,
    },
    /// Where our game stopped for the link: sent by both sides once they have paused.
    Start {
        /// The other end.
        target: PeerId,
        /// The request this link came from.
        nonce: u32,
        /// Our frame count at the hold.
        frame: u64,
        /// The input we hold there.
        input: InputBuffer,
        /// Our last round-trip time to the host, in milliseconds (0 for the host).
        rtt_millis: u32,
        /// Our input-delay setting: 0 for automatic, else the frames we want at least.
        delay_setting: u8,
    },
    /// One lockstep frame's events.
    Frame {
        /// The other end.
        target: PeerId,
        /// The link frame these events land on.
        frame: u64,
        /// Our recording clock (the `elapsed_millis` our snapshots and stream use) as we send
        /// it, `delay` frames before `frame` runs.
        elapsed_millis: u64,
        /// What lands there (`NoOp`, `ChangeInput`, `WriteMemory`, `ResetConsole` only).
        events: Vec<Packet>,
        /// A hash of both consoles' work RAM at that frame, when one is due.
        pair_hash: Option<(u64, Blake3Hash)>,
    },
    /// Unplug the cable.
    Unlink {
        /// The other end.
        target: PeerId,
        /// Why.
        reason: UnlinkReason,
    },
}

impl LinkMessage {
    /// The other end.
    pub fn target(&self) -> PeerId {
        match self {
            LinkMessage::Request { target, .. }
            | LinkMessage::Accept { target, .. }
            | LinkMessage::Decline { target, .. }
            | LinkMessage::Start { target, .. }
            | LinkMessage::Frame { target, .. }
            | LinkMessage::Unlink { target, .. } => *target,
        }
    }
}

/// Serialize link events the way `LinkFrame.events` carries them.
pub fn encode_link_events(events: &[Packet]) -> Vec<u8> {
    let mut out = Vec::new();
    for packet in events {
        append_packet(packet, &mut out);
    }
    out
}

/// Parse `LinkFrame.events` to the end. Only `NoOp`, `ChangeInput`, `WriteMemory` and
/// `ResetConsole` may appear; anything else is [`DecodeError::ForbiddenPacket`].
pub fn decode_link_events(bytes: &[u8]) -> Result<Vec<Packet>, DecodeError> {
    let mut events = Vec::new();
    let mut cursor = bytes;
    while !cursor.is_empty() {
        let packet = Packet::read_all(&mut cursor, REPLAY_VERSION).map_err(|e| DecodeError::BadPacket(format!("{e:?}")))?;
        if let Some(kind) = forbidden_link_kind(&packet) {
            return Err(DecodeError::ForbiddenPacket(kind));
        }
        events.push(packet);
    }
    Ok(events)
}

/// The name of a packet kind that must not appear in a link frame, or `None` when it is allowed.
fn forbidden_link_kind(packet: &Packet) -> Option<&'static str> {
    match packet {
        Packet::NoOp | Packet::ChangeInput { .. } | Packet::WriteMemory { .. } | Packet::ResetConsole => None,
        Packet::NextFrame { .. } => Some("NextFrame"),
        Packet::ChangeSpeed { .. } => Some("ChangeSpeed"),
        Packet::LoadSaveState { .. } => Some("LoadSaveState"),
        Packet::IncrementCounter { .. } => Some("IncrementCounter"),
        Packet::SerialIn { .. } => Some("SerialIn"),
        Packet::Keyframe { .. } => Some("Keyframe"),
        Packet::DeltaKeyframe { .. } => Some("DeltaKeyframe"),
        Packet::RegionDeltaKeyframe { .. } => Some("RegionDeltaKeyframe"),
        Packet::CompressedBlob { .. } => Some("CompressedBlob"),
        Packet::Bookmark { .. } => Some("Bookmark"),
        Packet::BookmarkTable { .. } => Some("BookmarkTable"),
    }
}

/// The `pair_hash_frame` / `pair_hash` fields of a `LinkFrame`: an all-zero hash means none.
pub(crate) fn pair_hash_fields(pair_hash: Option<(u64, Blake3Hash)>) -> (u64, Blake3Hash) {
    pair_hash.unwrap_or((0, [0; 32]))
}

/// The inverse of [`pair_hash_fields`].
pub(crate) fn pair_hash_option(frame: u64, hash: Blake3Hash) -> Option<(u64, Blake3Hash)> {
    if hash == [0; 32] { None } else { Some((frame, hash)) }
}
