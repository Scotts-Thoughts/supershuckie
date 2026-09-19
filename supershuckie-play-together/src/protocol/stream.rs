//! `Stream.bytes`: whole replay packets in `PacketIO` encoding at `REPLAY_VERSION`.

use supershuckie_replay_recorder::replay_file::REPLAY_VERSION;
use supershuckie_replay_recorder::{append_packet, Packet, PacketIO};

use crate::error::DecodeError;

/// A writer merges consecutive pending streams into one `Stream` message up to this many bytes,
/// splitting only between publish calls (never inside a packet); a single larger item goes out
/// on its own.
pub const STREAM_MERGE_LIMIT: usize = 256 << 10;

/// Serialize packets the way `Stream.bytes` carries them.
pub fn encode_packets(packets: &[Packet]) -> Vec<u8> {
    let mut out = Vec::new();
    for packet in packets {
        append_packet(packet, &mut out);
    }
    out
}

/// Parse `Stream.bytes` to the end.
///
/// Trailing garbage is an error, and so is any packet kind Play Together never sends: only
/// `NoOp`, `NextFrame`, `ChangeInput`, `WriteMemory`, `ChangeSpeed`, `ResetConsole`,
/// `LoadSaveState`, `IncrementCounter` and `SerialIn` pass. Anything else (keyframes, blobs,
/// bookmarks) is [`DecodeError::ForbiddenPacket`].
pub fn decode_packets(bytes: &[u8]) -> Result<Vec<Packet>, DecodeError> {
    let mut packets = Vec::new();
    let mut cursor = bytes;
    while !cursor.is_empty() {
        let packet = Packet::read_all(&mut cursor, REPLAY_VERSION).map_err(|e| DecodeError::BadPacket(format!("{e:?}")))?;
        if let Some(kind) = forbidden_kind(&packet) {
            return Err(DecodeError::ForbiddenPacket(kind));
        }
        packets.push(packet);
    }
    Ok(packets)
}

/// How many `NextFrame` packets are in `packets`.
pub fn count_frames(packets: &[Packet]) -> u64 {
    packets.iter().filter(|p| matches!(p, Packet::NextFrame { .. })).count() as u64
}

/// The name of a packet kind that must not appear in a stream, or `None` when it is allowed.
fn forbidden_kind(packet: &Packet) -> Option<&'static str> {
    match packet {
        Packet::NoOp
        | Packet::NextFrame { .. }
        | Packet::ChangeInput { .. }
        | Packet::WriteMemory { .. }
        | Packet::ChangeSpeed { .. }
        | Packet::ResetConsole
        | Packet::LoadSaveState { .. }
        | Packet::IncrementCounter { .. }
        | Packet::SerialIn { .. } => None,
        Packet::Keyframe { .. } => Some("Keyframe"),
        Packet::DeltaKeyframe { .. } => Some("DeltaKeyframe"),
        Packet::RegionDeltaKeyframe { .. } => Some("RegionDeltaKeyframe"),
        Packet::CompressedBlob { .. } => Some("CompressedBlob"),
        Packet::Bookmark { .. } => Some("Bookmark"),
        Packet::BookmarkTable { .. } => Some("BookmarkTable"),
    }
}
