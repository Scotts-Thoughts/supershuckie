//! The codec: round trips, byte-exact layouts, hostile input, packet allow-list, snapshots,
//! join codes, display names and compatibility.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{self, Cursor, Read};
use std::sync::atomic::{AtomicUsize, Ordering};

use common::*;
use supershuckie_play_together::protocol::*;
use supershuckie_play_together::*;
use supershuckie_replay_recorder::replay_file::{ReplayConsoleType, REPLAY_VERSION};
use supershuckie_replay_recorder::{append_packet, InputBuffer, KeyframeMetadata, Packet, Speed, TimestampMillis};

// ---------------------------------------------------------------------------------------------
// A counting allocator so a test can measure peak allocation.

struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let now = CURRENT.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// Reads from a slice and counts how many bytes it handed out.
struct CountingReader<'a> {
    data: &'a [u8],
    served: usize,
}

impl Read for CountingReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = buf.len().min(self.data.len());
        buf[..n].copy_from_slice(&self.data[..n]);
        self.data = &self.data[n..];
        self.served += n;
        Ok(n)
    }
}

// ---------------------------------------------------------------------------------------------
// Samples

fn participant(id: PeerId, name: &str) -> ParticipantInfo {
    ParticipantInfo { peer_id: id, display_name: name.to_owned(), color: 1, app_version: "0.4.14".to_owned(), publisher: publisher_info(ReplayConsoleType::GameBoy) }
}

fn max_name() -> String {
    "x".repeat(MAX_DISPLAY_NAME_BYTES)
}

fn start_state(len: usize) -> StartStateData {
    StartStateData { rom_checksum: [0x11; 32], state: (0..len).map(|i| (i % 251) as u8).collect() }
}

fn samples() -> Vec<Message> {
    let mut publisher = publisher_info(ReplayConsoleType::GameBoyColor);
    publisher.metadata.rom_name = "r".repeat(255);
    publisher.metadata.rom_filename = "ü".repeat(127);
    publisher.metadata.emulator_core_name = String::new();
    publisher.initial_input = (0..64u8).collect();
    publisher.speed = Speed::from_multiplier_float(4.0);
    publisher.frame = u64::MAX;
    let eight: Vec<ParticipantInfo> = (1..=8).map(|i| participant(i as PeerId, &max_name())).collect();
    let snapshot = snapshot_at(1234);
    vec![
        Message::Hello { protocol_version: 2, replay_version: REPLAY_VERSION, app_version: "0.4.14".to_owned(), display_name: max_name(), color: 22, publisher: publisher.clone() },
        Message::Hello { protocol_version: 0, replay_version: 0, app_version: String::new(), display_name: String::new(), color: 0, publisher: publisher_info(ReplayConsoleType::GameBoy) },
        Message::Welcome { your_peer_id: 9, session_id: u64::MAX, your_display_name: "Player (2)".to_owned(), your_color: 9, participants: eight },
        Message::Welcome { your_peer_id: 2, session_id: 1, your_display_name: String::new(), your_color: 1, participants: vec![] },
        Message::Refused { reason: RefusalReason::SessionFull, text: "full".to_owned() },
        Message::Refused { reason: RefusalReason::HostShuttingDown, text: String::new() },
        Message::PeerJoined { participant: participant(3, "Bob") },
        Message::PeerLeft { peer_id: 3, reason: LeaveReason::TooSlow },
        Message::PeerLeft { peer_id: u16::MAX, reason: LeaveReason::HostLeft },
        Message::ResetAll { race_id: 7, countdown_millis: 3000 },
        Message::SyncPause { enabled: true, paused: false },
        Message::SyncPause { enabled: false, paused: true },
        Message::Pause { from: 0, paused: true },
        Message::Pause { from: 3, paused: false },
        Message::Stream { from: 2, first_frame: 100, bytes: frames(3) },
        Message::Stream { from: 2, first_frame: 0, bytes: vec![] },
        Message::Snapshot(WireSnapshot::raw(&snapshot, 2, 3)),
        Message::Snapshot(WireSnapshot::compressed(&snapshot, 2, 0)),
        Message::Snapshot(WireSnapshot::raw(&SnapshotData { counters: (0..256).map(|i| (format!("c{i}"), -(i as i64))).collect(), ..snapshot_at(0) }, 1, 0)),
        Message::SyncHash { from: 4, frame: 5, hash: [0xEE; 32] },
        Message::RequestSnapshot { requester: 0, target: 2 },
        Message::RequestSnapshot { requester: 5, target: 0 },
        Message::StartState(WireStartState::raw(&start_state(1234))),
        Message::StartState(WireStartState::with_state(&start_state(1234), StateEncoding::Zstd, compress_state(&start_state(1234).state).unwrap())),
        Message::StartState(WireStartState::cleared()),
        Message::Ping { nonce: 1, sent_unix_millis: 2 },
        Message::Pong { nonce: u32::MAX, sent_unix_millis: u64::MAX },
        Message::Goodbye,
        Message::Error { text: "e".repeat(MAX_TEXT_BYTES) },
        Message::LinkRequest { from: 0, target: 2, nonce: 7, console: 3 },
        Message::LinkRequest { from: 2, target: 1, nonce: u32::MAX, console: u32::MAX },
        Message::LinkAccept { from: 3, target: 2, nonce: 7 },
        Message::LinkDecline { from: 3, target: 2, nonce: 7, reason: LinkDeclineReason::Busy },
        Message::LinkDecline { from: 0, target: 2, nonce: 0, reason: LinkDeclineReason::Other },
        Message::LinkStart { from: 2, target: 3, nonce: 7, frame: 1234, input: (0..64u8).collect(), rtt_millis: 40, delay_setting: 15 },
        Message::LinkStart { from: 0, target: 3, nonce: 7, frame: 0, input: InputBuffer::new(), rtt_millis: 0, delay_setting: 0 },
        Message::LinkFrame { from: 2, target: 3, frame: 99, elapsed_millis: 123_456, events: link_events(), pair_hash_frame: 60, pair_hash: [0xCD; 32] },
        Message::LinkFrame { from: 2, target: 3, frame: 0, elapsed_millis: 0, events: vec![], pair_hash_frame: 0, pair_hash: [0; 32] },
        Message::Unlink { from: 2, target: 3, reason: UnlinkReason::Desync },
        Message::Unlink { from: 0, target: 3, reason: UnlinkReason::Other },
        Message::PeerLinked { a: 2, b: 3 },
        Message::PeerUnlinked { a: 1, b: u16::MAX },
    ]
}

/// A link frame's worth of events: an input change, a write and a reset.
fn link_events() -> Vec<u8> {
    encode_link_events(&[
        Packet::ChangeInput { data: [1u8, 2].iter().copied().collect() },
        Packet::WriteMemory { address: 0xC000, data: [9u8; 40].iter().copied().collect() },
        Packet::ResetConsole,
        Packet::NoOp,
    ])
}

fn round_trip(m: &Message) {
    let bytes = m.encoded();
    let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
    assert_eq!(len, bytes.len() - 4, "length counts tag + payload");
    assert_eq!(bytes[4], m.tag());
    assert_eq!(Message::decode(&bytes[4..]).as_ref(), Ok(m), "decode {m:?}");
    let mut cursor = Cursor::new(bytes);
    assert_eq!(Message::read(&mut cursor).unwrap(), Some(Ok(m.clone())));
    assert_eq!(Message::read(&mut cursor).unwrap(), None, "clean EOF after the message");
}

#[test]
fn every_message_round_trips() {
    for m in samples() {
        round_trip(&m);
    }
}

#[test]
fn wire_layout_matches_the_document() {
    assert_eq!(
        Message::Ping { nonce: 0x01020304, sent_unix_millis: 0x1122334455667788 }.encoded(),
        [
            13, 0, 0, 0, // length = 1 + 4 + 8
            0x20, // Ping
            4, 3, 2, 1, // nonce
            0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, // sent_unix_millis
        ]
    );
    assert_eq!(Message::RequestSnapshot { requester: 0, target: 0x0102 }.encoded(), [5, 0, 0, 0, 0x13, 0, 0, 2, 1]);
    assert_eq!(Message::SyncPause { enabled: true, paused: false }.encoded(), [3, 0, 0, 0, 0x07, 1, 0]);
    // `from` sits right after the tag, like `Stream.from`: what the host overwrites when relaying.
    assert_eq!(Message::Pause { from: 0x0102, paused: true }.encoded(), [4, 0, 0, 0, 0x08, 2, 1, 1]);
    assert_eq!(Message::decode(&[0x08, 0, 0, 2]), Err(DecodeError::BadBool(2)));
    let cleared = Message::StartState(WireStartState::cleared()).encoded();
    assert_eq!(cleared.len(), 4 + 1 + 32 + 1 + 8 + 4);
    assert_eq!(&cleared[..5], &[46, 0, 0, 0, 0x14]);
    assert_eq!(&cleared[37..], &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]); // raw, state_len 0, no bytes
    assert_eq!(max_message_length(0x14), MAX_MESSAGE_LENGTH, "a start state is a large message");
    assert_eq!(Message::Goodbye.encoded(), [1, 0, 0, 0, 0x22]);
    let stream = Message::Stream { from: 2, first_frame: 3, bytes: vec![9, 9] }.encoded();
    assert_eq!(stream, [17, 0, 0, 0, 0x10, 2, 0, 3, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 9, 9]);
    // `from` sits right after the tag: what the host overwrites when relaying.
    assert_eq!(&stream[5..7], &[2, 0]);

    // The link cable messages: `from` first, then `target`, both u16.
    assert_eq!(Message::LinkRequest { from: 0x0102, target: 3, nonce: 7, console: 2 }.encoded(), [13, 0, 0, 0, 0x30, 2, 1, 3, 0, 7, 0, 0, 0, 2, 0, 0, 0]);
    assert_eq!(Message::LinkAccept { from: 3, target: 2, nonce: 7 }.encoded(), [9, 0, 0, 0, 0x31, 3, 0, 2, 0, 7, 0, 0, 0]);
    assert_eq!(Message::LinkDecline { from: 3, target: 2, nonce: 7, reason: LinkDeclineReason::ConsoleMismatch }.encoded(), [13, 0, 0, 0, 0x32, 3, 0, 2, 0, 7, 0, 0, 0, 2, 0, 0, 0]);
    assert_eq!(
        Message::LinkStart { from: 2, target: 3, nonce: 7, frame: 0x0100, input: [5u8].iter().copied().collect(), rtt_millis: 30, delay_setting: 4 }.encoded(),
        [27, 0, 0, 0, 0x33, 2, 0, 3, 0, 7, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 5, 30, 0, 0, 0, 4]
    );
    let frame = Message::LinkFrame { from: 2, target: 3, frame: 9, elapsed_millis: 0x0102, events: vec![0xF0], pair_hash_frame: 60, pair_hash: [0xEE; 32] }.encoded();
    assert_eq!(&frame[..30], &[1 + 2 + 2 + 8 + 8 + 4 + 1 + 8 + 32, 0, 0, 0, 0x34, 2, 0, 3, 0, 9, 0, 0, 0, 0, 0, 0, 0, 2, 1, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0xF0]);
    assert_eq!(&frame[30..38], &[60, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(&frame[38..], &[0xEE; 32]);
    assert_eq!(Message::Unlink { from: 2, target: 3, reason: UnlinkReason::Unplugged }.encoded(), [9, 0, 0, 0, 0x35, 2, 0, 3, 0, 0, 0, 0, 0]);
    assert_eq!(Message::PeerLinked { a: 2, b: 3 }.encoded(), [5, 0, 0, 0, 0x36, 2, 0, 3, 0]);
    assert_eq!(Message::PeerUnlinked { a: 2, b: 3 }.encoded(), [5, 0, 0, 0, 0x37, 2, 0, 3, 0]);
    for tag in 0x30..=0x37u8 {
        assert_eq!(max_message_length(tag), MAX_SMALL_MESSAGE_LENGTH, "link messages are small");
    }
    // Unknown reasons decode to `Other` rather than failing: a newer peer may know more.
    assert_eq!(Message::decode(&[0x32, 3, 0, 2, 0, 7, 0, 0, 0, 200, 0, 0, 0]), Ok(Message::LinkDecline { from: 3, target: 2, nonce: 7, reason: LinkDeclineReason::Other }));
    assert_eq!(Message::decode(&[0x35, 2, 0, 3, 0, 0xFF, 0xFF, 0xFF, 0xFF]), Ok(Message::Unlink { from: 2, target: 3, reason: UnlinkReason::Other }));
    assert_eq!(u32::from(LinkDeclineReason::Other), u32::MAX);
    assert_eq!(u32::from(UnlinkReason::Other), u32::MAX);
}

#[test]
fn every_truncation_is_an_error_not_a_panic() {
    for m in samples() {
        let bytes = m.encoded();
        let body = &bytes[4..];
        for cut in 0..body.len() {
            assert!(Message::decode(&body[..cut]).is_err(), "{m:?} cut at {cut} decoded");
        }
        // And through the framed reader: a truncated frame is an EOF, never a panic.
        for cut in 0..bytes.len() {
            let mut cursor = Cursor::new(&bytes[..cut]);
            match Message::read(&mut cursor) {
                Ok(None) if cut == 0 => {}
                Err(e) => assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof, "{m:?} cut at {cut}"),
                other => panic!("{m:?} cut at {cut}: {other:?}"),
            }
        }
    }
}

#[test]
fn oversized_lengths_are_refused_before_allocation() {
    for len in [0xFFFF_FFFFu32, MAX_MESSAGE_LENGTH + 1, 0] {
        let mut bytes = len.to_le_bytes().to_vec();
        bytes.push(0x10);
        let mut cursor = Cursor::new(bytes);
        assert_eq!(Message::read(&mut cursor).unwrap(), Some(Err(DecodeError::TooLong(len))), "length {len}");
    }
    // A Ping claiming 1 MiB: small tags are capped at 64 KiB.
    let mut bytes = (1u32 << 20).to_le_bytes().to_vec();
    bytes.push(0x20);
    bytes.extend_from_slice(&[0; 64]);
    let mut cursor = Cursor::new(bytes);
    assert_eq!(Message::read(&mut cursor).unwrap(), Some(Err(DecodeError::TooLong(1 << 20))));
    // An unknown tag claiming 64 KiB + 1: refused too.
    let mut bytes = (MAX_SMALL_MESSAGE_LENGTH + 1).to_le_bytes().to_vec();
    bytes.push(0x7E);
    let mut cursor = Cursor::new(bytes);
    assert_eq!(Message::read(&mut cursor).unwrap(), Some(Err(DecodeError::TooLong(MAX_SMALL_MESSAGE_LENGTH + 1))));

    // A Stream claiming the full 48 MiB over 10 bytes of input: EOF, with a small peak
    // allocation (the body is read in chunks, never `vec![0; claimed]`).
    let mut bytes = MAX_MESSAGE_LENGTH.to_le_bytes().to_vec();
    bytes.push(0x10);
    bytes.extend_from_slice(&[0; 5]);
    let mut reader = CountingReader { data: &bytes, served: 0 };
    let baseline = CURRENT.load(Ordering::Relaxed);
    PEAK.store(baseline, Ordering::Relaxed);
    let result = Message::read(&mut reader);
    let peak = PEAK.load(Ordering::Relaxed) - baseline;
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    assert_eq!(reader.served, 10);
    assert!(peak < 1 << 20, "peak allocation {peak} bytes for a 48 MiB claim");
}

#[test]
fn unknown_tag_is_skipped_then_the_next_message_reads() {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&6u32.to_le_bytes());
    bytes.push(0x7F);
    bytes.extend_from_slice(&[1, 2, 3, 4, 5]);
    Message::Ping { nonce: 1, sent_unix_millis: 2 }.encode(&mut bytes);
    let mut cursor = Cursor::new(bytes);
    assert_eq!(Message::read(&mut cursor).unwrap(), Some(Err(DecodeError::UnknownTag(0x7F))));
    assert_eq!(Message::read(&mut cursor).unwrap(), Some(Ok(Message::Ping { nonce: 1, sent_unix_millis: 2 })));
    assert_eq!(Message::read(&mut cursor).unwrap(), None);
    assert_eq!(max_message_length(0x7F), MAX_SMALL_MESSAGE_LENGTH);
    assert_eq!(max_message_length(0x10), MAX_MESSAGE_LENGTH);
    assert_eq!(max_message_length(0x11), MAX_MESSAGE_LENGTH);
}

/// Encode a body by hand (tag + payload) for the hostile cases.
fn body(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut b = vec![tag];
    b.extend_from_slice(payload);
    b
}

fn str_field(s: &[u8]) -> Vec<u8> {
    let mut b = (s.len() as u32).to_le_bytes().to_vec();
    b.extend_from_slice(s);
    b
}

#[test]
fn hostile_counts_strings_and_values() {
    // Welcome claiming 2^32-1 participants.
    let mut p = Vec::new();
    p.extend_from_slice(&2u16.to_le_bytes());
    p.extend_from_slice(&1u64.to_le_bytes());
    p.extend(str_field(b"me"));
    p.push(1); // your_color
    p.extend_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(Message::decode(&body(0x02, &p)), Err(DecodeError::TooMany { what: "participants", count: u32::MAX, max: MAX_PARTICIPANTS }));
    // Nine participants: over the cap even when they would fit.
    let mut nine = Message::Welcome { your_peer_id: 2, session_id: 1, your_display_name: "a".into(), your_color: 1, participants: (1..=8).map(|i| participant(i, "p")).collect() }.encoded();
    // length (4) | tag (1) | your_peer_id (2) | session_id (8) | name (4 + 1) | your_color (1) | count
    nine[4 + 1 + 2 + 8 + 4 + 1 + 1..4 + 1 + 2 + 8 + 4 + 1 + 1 + 4].copy_from_slice(&9u32.to_le_bytes());
    assert_eq!(Message::decode(&nine[4..]), Err(DecodeError::TooMany { what: "participants", count: 9, max: MAX_PARTICIPANTS }));

    // A 300-byte rom_name.
    let mut publisher = publisher_info(ReplayConsoleType::GameBoy);
    publisher.metadata.rom_name = "n".repeat(300);
    let hello = Message::Hello { protocol_version: 2, replay_version: 6, app_version: String::new(), display_name: "a".into(), color: 0, publisher }.encoded();
    assert_eq!(Message::decode(&hello[4..]), Err(DecodeError::FieldTooLong { what: "rom_name", len: 300, max: 255 }));

    // A 33-byte display name.
    let hello = Message::Hello { protocol_version: 2, replay_version: 6, app_version: String::new(), display_name: "a".repeat(33), color: 0, publisher: publisher_info(ReplayConsoleType::GameBoy) }.encoded();
    assert_eq!(Message::decode(&hello[4..]), Err(DecodeError::FieldTooLong { what: "display_name", len: 33, max: 32 }));

    // Non-UTF-8 text.
    let mut p = 0u32.to_le_bytes().to_vec();
    p.extend(str_field(&[0xFF, 0xFE]));
    assert_eq!(Message::decode(&body(0x03, &p)), Err(DecodeError::BadString));

    // Zero peer ids where a real peer is required.
    let mut stream = Message::Stream { from: 2, first_frame: 0, bytes: vec![] }.encoded();
    stream[5..7].copy_from_slice(&[0, 0]);
    assert_eq!(Message::decode(&stream[4..]), Err(DecodeError::ZeroPeerId));
    let mut welcome = Message::Welcome { your_peer_id: 2, session_id: 1, your_display_name: "a".into(), your_color: 1, participants: vec![] }.encoded();
    welcome[5..7].copy_from_slice(&[0, 0]);
    assert_eq!(Message::decode(&welcome[4..]), Err(DecodeError::ZeroPeerId));
    let mut left = Message::PeerLeft { peer_id: 2, reason: LeaveReason::Left }.encoded();
    left[5..7].copy_from_slice(&[0, 0]);
    assert_eq!(Message::decode(&left[4..]), Err(DecodeError::ZeroPeerId));
    // ...but not where zero is meaningful.
    assert!(Message::decode(&Message::RequestSnapshot { requester: 0, target: 0 }.encoded()[4..]).is_ok());

    // Zero speed.
    let hello = Message::Hello { protocol_version: 2, replay_version: 6, app_version: String::new(), display_name: "a".into(), color: 0, publisher: publisher_info(ReplayConsoleType::GameBoy) }.encoded();
    let speed_at = hello.len() - 8 - 2;
    let mut zero_speed = hello.clone();
    zero_speed[speed_at..speed_at + 2].copy_from_slice(&[0, 0]);
    assert_eq!(Message::decode(&zero_speed[4..]), Err(DecodeError::BadSpeed));

    // console_type 999.
    let console_at = 4 + 1 + 4 + 4 + 4 + 4 + 1 + 1; // ... | app_version | display_name | color
    let mut bad_console = hello.clone();
    bad_console[console_at..console_at + 4].copy_from_slice(&999u32.to_le_bytes());
    assert_eq!(Message::decode(&bad_console[4..]), Err(DecodeError::BadEnum { what: "console type", value: 999 }));

    // A colour outside the palette in an assignment.
    let mut bad_color = Message::Welcome { your_peer_id: 2, session_id: 1, your_display_name: "a".into(), your_color: 1, participants: vec![] }.encoded();
    let color_at = 4 + 1 + 2 + 8 + 4 + 1;
    bad_color[color_at] = 0;
    assert_eq!(Message::decode(&bad_color[4..]), Err(DecodeError::BadColor(0)));
    bad_color[color_at] = 23;
    assert_eq!(Message::decode(&bad_color[4..]), Err(DecodeError::BadColor(23)));

    // Unknown reasons and encodings.
    let mut p = 99u32.to_le_bytes().to_vec();
    p.extend(str_field(b""));
    assert_eq!(Message::decode(&body(0x03, &p)), Err(DecodeError::BadEnum { what: "refusal reason", value: 99 }));
    let mut p = 2u16.to_le_bytes().to_vec();
    p.extend_from_slice(&7u32.to_le_bytes());
    assert_eq!(Message::decode(&body(0x05, &p)), Err(DecodeError::BadEnum { what: "leave reason", value: 7 }));

    // Trailing bytes.
    assert_eq!(Message::decode(&[0x22, 0]), Err(DecodeError::TrailingBytes(1)));
    assert_eq!(Message::decode(&[]), Err(DecodeError::Truncated));

    // A snapshot with 257 counters, or an input of 65 bytes.
    let big = Message::Snapshot(WireSnapshot::raw(&SnapshotData { counters: (0..257).map(|i| (format!("{i}"), 0)).collect(), ..snapshot_at(0) }, 1, 0)).encoded();
    assert_eq!(Message::decode(&big[4..]), Err(DecodeError::TooMany { what: "counters", count: 257, max: MAX_COUNTERS }));
    let big = Message::Snapshot(WireSnapshot::raw(&SnapshotData { input: (0..65u8).collect(), ..snapshot_at(0) }, 1, 0)).encoded();
    assert_eq!(Message::decode(&big[4..]), Err(DecodeError::FieldTooLong { what: "input", len: 65, max: MAX_INPUT_BYTES }));

    // Link cable messages: a zero target, a delay over the limit, too many event bytes.
    let mut request = Message::LinkRequest { from: 2, target: 3, nonce: 1, console: 1 }.encoded();
    request[7..9].copy_from_slice(&[0, 0]);
    assert_eq!(Message::decode(&request[4..]), Err(DecodeError::ZeroPeerId));
    let mut start = Message::LinkStart { from: 2, target: 3, nonce: 1, frame: 0, input: InputBuffer::new(), rtt_millis: 0, delay_setting: 15 }.encoded();
    let last = start.len() - 1;
    start[last] = 16;
    assert_eq!(Message::decode(&start[4..]), Err(DecodeError::BadEnum { what: "link delay setting", value: 16 }));
    let mut start = Message::LinkStart { from: 2, target: 3, nonce: 1, frame: 0, input: (0..65u8).collect(), rtt_millis: 0, delay_setting: 0 }.encoded();
    assert_eq!(Message::decode(&start[4..]), Err(DecodeError::FieldTooLong { what: "input", len: 65, max: MAX_INPUT_BYTES }));
    start.clear();
    let frame = Message::LinkFrame { from: 2, target: 3, frame: 0, elapsed_millis: 0, events: vec![0; MAX_LINK_EVENT_BYTES + 1], pair_hash_frame: 0, pair_hash: [0; 32] }.encoded();
    assert_eq!(Message::decode(&frame[4..]), Err(DecodeError::FieldTooLong { what: "link events", len: MAX_LINK_EVENT_BYTES as u32 + 1, max: MAX_LINK_EVENT_BYTES }));
    let mut linked = Message::PeerLinked { a: 2, b: 3 }.encoded();
    linked[5..7].copy_from_slice(&[0, 0]);
    assert_eq!(Message::decode(&linked[4..]), Err(DecodeError::ZeroPeerId));
}

#[test]
fn packet_allow_list() {
    let packets = vec![
        Packet::NoOp,
        Packet::ChangeInput { data: [1u8].iter().copied().collect() },
        Packet::NextFrame { timestamp_delta: TimestampMillis(16) },
        Packet::WriteMemory { address: 0x2000000, data: [1u8, 2, 3, 4].iter().copied().collect() },
        Packet::ChangeSpeed { speed: Speed::from_multiplier_float(2.0) },
        Packet::ResetConsole,
        Packet::LoadSaveState { state: vec![7u8; 100].into_iter().collect() },
        Packet::IncrementCounter { name: "resets".to_owned(), delta: -1 },
        Packet::NextFrame { timestamp_delta: TimestampMillis(17) },
    ];
    let bytes = encode_packets(&packets);
    assert_eq!(decode_packets(&bytes), Ok(packets.clone()));
    assert_eq!(count_frames(&packets), 2);
    assert_eq!(decode_packets(&[]), Ok(vec![]));

    let mut bytes = Vec::new();
    append_packet(&Packet::NextFrame { timestamp_delta: TimestampMillis(1) }, &mut bytes);
    append_packet(&Packet::Keyframe { metadata: KeyframeMetadata::default(), state: vec![0u8; 16].into_iter().collect() }, &mut bytes);
    assert_eq!(decode_packets(&bytes), Err(DecodeError::ForbiddenPacket("Keyframe")));

    let mut bytes = Vec::new();
    append_packet(&Packet::BookmarkTable { table: Default::default() }, &mut bytes);
    assert_eq!(decode_packets(&bytes), Err(DecodeError::ForbiddenPacket("BookmarkTable")));

    let mut bytes = frames(2);
    bytes.extend_from_slice(&[0xFF, 0xFF, 0xFF]);
    assert!(matches!(decode_packets(&bytes), Err(DecodeError::BadPacket(_))), "trailing garbage");
    let bytes = frames(2);
    assert!(matches!(decode_packets(&bytes[..bytes.len() - 1]), Err(DecodeError::BadPacket(_))), "truncated packet");

    // A stream may carry SerialIn (a linked game's serial input, recorded per frame).
    let mut bytes = Vec::new();
    append_packet(&Packet::SerialIn { data: vec![1u8, 2, 3].into_iter().collect() }, &mut bytes);
    assert!(decode_packets(&bytes).is_ok());
}

#[test]
fn link_event_allow_list() {
    let events = vec![
        Packet::NoOp,
        Packet::ChangeInput { data: [1u8].iter().copied().collect() },
        Packet::WriteMemory { address: 0xC000, data: [1u8, 2, 3, 4].iter().copied().collect() },
        Packet::ResetConsole,
    ];
    let bytes = encode_link_events(&events);
    assert_eq!(decode_link_events(&bytes), Ok(events));
    assert_eq!(decode_link_events(&[]), Ok(vec![]));
    for (packet, name) in [
        (Packet::NextFrame { timestamp_delta: TimestampMillis(16) }, "NextFrame"),
        (Packet::ChangeSpeed { speed: Speed::from_multiplier_float(2.0) }, "ChangeSpeed"),
        (Packet::LoadSaveState { state: vec![7u8; 10].into_iter().collect() }, "LoadSaveState"),
        (Packet::IncrementCounter { name: "resets".to_owned(), delta: 1 }, "IncrementCounter"),
        (Packet::SerialIn { data: vec![1u8].into_iter().collect() }, "SerialIn"),
        (Packet::Keyframe { metadata: KeyframeMetadata::default(), state: vec![0u8; 16].into_iter().collect() }, "Keyframe"),
        (Packet::BookmarkTable { table: Default::default() }, "BookmarkTable"),
    ] {
        let mut bytes = Vec::new();
        append_packet(&Packet::NoOp, &mut bytes);
        append_packet(&packet, &mut bytes);
        assert_eq!(decode_link_events(&bytes), Err(DecodeError::ForbiddenPacket(name)));
    }
    let mut bytes = encode_link_events(&[Packet::ResetConsole]);
    bytes.push(0xFF);
    assert!(matches!(decode_link_events(&bytes), Err(DecodeError::BadPacket(_))));
    // The link families.
    assert_eq!(link_family(ReplayConsoleType::GameBoy), Some(LinkFamily::GameBoy));
    assert_eq!(link_family(ReplayConsoleType::SuperGameBoy2), Some(LinkFamily::GameBoy));
    assert_eq!(link_family(ReplayConsoleType::GameBoyAdvance), Some(LinkFamily::GameBoyAdvance));
    assert_eq!(link_family(ReplayConsoleType::NintendoDS), None);
    assert!(can_link(ReplayConsoleType::GameBoy, ReplayConsoleType::GameBoyColor));
    assert!(!can_link(ReplayConsoleType::GameBoyColor, ReplayConsoleType::GameBoyAdvance));
    assert!(!can_link(ReplayConsoleType::Unknown, ReplayConsoleType::Unknown));
}

#[test]
fn start_state_decoding() {
    let data = start_state(5000);
    // Raw, compressed and cleared all decode back to the application's view.
    assert_eq!(WireStartState::raw(&data).into_state(), Ok(Some(data.clone())));
    let compressed = WireStartState::with_state(&data, StateEncoding::Zstd, compress_state(&data.state).unwrap());
    assert!(compressed.state.len() < data.state.len());
    assert_eq!(compressed.into_state(), Ok(Some(data.clone())));
    assert_eq!(WireStartState::cleared().into_state(), Ok(None));
    assert!(WireStartState::cleared().is_cleared());
    assert!(!WireStartState::raw(&data).is_cleared());

    // A raw state whose length disagrees with state_len, a state_len over the cap, and a zstd
    // frame that disagrees with state_len are refused; the cap is checked at decode time before
    // anything is allocated.
    let mut wrong = WireStartState::raw(&data);
    wrong.state_len = 4999;
    assert!(matches!(wrong.into_state(), Err(DecodeError::BadState(_))));
    let mut huge = WireStartState::raw(&data);
    huge.state_len = MAX_STATE_LENGTH + 1;
    assert_eq!(huge.clone().into_state(), Err(DecodeError::StateTooLarge(MAX_STATE_LENGTH + 1)));
    let encoded = Message::StartState(huge).encoded();
    assert_eq!(Message::decode(&encoded[4..]), Err(DecodeError::StateTooLarge(MAX_STATE_LENGTH + 1)));
    let mut lying = WireStartState::with_state(&data, StateEncoding::Zstd, compress_state(&data.state).unwrap());
    lying.state_len = 10;
    assert!(matches!(lying.into_state(), Err(DecodeError::BadState(_))));
    // An empty state that is not cleared (state_len > 0, no bytes) is a bad raw state, not None.
    let empty = WireStartState { rom_checksum: [1; 32], encoding: StateEncoding::Raw, state_len: 3, state: Vec::new() };
    assert!(matches!(empty.into_state(), Err(DecodeError::BadState(_))));
}

#[test]
fn snapshot_decoding_rejects_bad_states() {
    let snapshot = snapshot_at(10);
    let wire = WireSnapshot::compressed(&snapshot, 2, 3);
    assert_eq!(wire.encoding, StateEncoding::Zstd);
    assert!(wire.state.len() < snapshot.state.len());
    assert_eq!(wire.clone().into_snapshot(), Ok(snapshot.clone()));
    assert_eq!(WireSnapshot::raw(&snapshot, 2, 3).into_snapshot(), Ok(snapshot.clone()));

    // A zstd frame whose header claims 1 TiB: refused by the content-size check.
    let mut fake = 0xFD2FB528u32.to_le_bytes().to_vec();
    fake.push(0xE0); // 8-byte content size, single segment
    fake.extend_from_slice(&(1u64 << 40).to_le_bytes());
    fake.extend_from_slice(&[0; 8]);
    let wire = WireSnapshot::with_state(&snapshot, 2, 3, StateEncoding::Zstd, fake);
    assert!(matches!(wire.into_snapshot(), Err(DecodeError::BadState(_))));

    // state_len over the cap: refused before anything is decoded, in both decoders.
    let mut wire = WireSnapshot::compressed(&snapshot, 2, 3);
    wire.state_len = MAX_STATE_LENGTH + 1;
    let encoded = Message::Snapshot(wire.clone()).encoded();
    assert_eq!(Message::decode(&encoded[4..]), Err(DecodeError::StateTooLarge(MAX_STATE_LENGTH + 1)));
    assert_eq!(wire.into_snapshot(), Err(DecodeError::StateTooLarge(MAX_STATE_LENGTH + 1)));

    // A raw state whose length disagrees with state_len.
    let mut wire = WireSnapshot::raw(&snapshot, 2, 3);
    wire.state_len += 1;
    assert!(matches!(wire.into_snapshot(), Err(DecodeError::BadState(_))));

    // Garbage as zstd.
    let wire = WireSnapshot::with_state(&snapshot, 2, 3, StateEncoding::Zstd, vec![1, 2, 3]);
    assert!(matches!(wire.into_snapshot(), Err(DecodeError::BadState(_))));

    // Unknown encoding byte on the wire.
    let mut encoded = Message::Snapshot(WireSnapshot::raw(&snapshot, 2, 3)).encoded();
    let enc_at = encoded.len() - 4 - snapshot.state.len() - 8 - 1;
    encoded[enc_at] = 9;
    assert_eq!(Message::decode(&encoded[4..]), Err(DecodeError::BadEnum { what: "state encoding", value: 9 }));
}

#[test]
fn join_codes() {
    assert_eq!(JoinCode::parse("1.2.3.4:30170"), Ok(JoinCode { host: "1.2.3.4".into(), port: 30170 }));
    assert_eq!(JoinCode::parse("  example.org:1234 \n"), Ok(JoinCode { host: "example.org".into(), port: 1234 }));
    assert_eq!(JoinCode::parse("[2001:db8::1]:30170"), Ok(JoinCode { host: "2001:db8::1".into(), port: 30170 }));
    assert_eq!(JoinCode::parse("[::1]"), Ok(JoinCode { host: "::1".into(), port: DEFAULT_PORT }));
    assert_eq!(JoinCode::parse("2001:db8::1"), Ok(JoinCode { host: "2001:db8::1".into(), port: DEFAULT_PORT }));
    assert_eq!(JoinCode::parse("example.org"), Ok(JoinCode { host: "example.org".into(), port: DEFAULT_PORT }));
    assert_eq!(JoinCode::parse("localhost"), Ok(JoinCode { host: "localhost".into(), port: DEFAULT_PORT }));
    assert_eq!(JoinCode::parse(""), Err(JoinCodeError::Empty));
    assert_eq!(JoinCode::parse("   "), Err(JoinCodeError::Empty));
    assert_eq!(JoinCode::parse("http://example.org:1"), Err(JoinCodeError::HasScheme));
    assert_eq!(JoinCode::parse("exam ple.org"), Err(JoinCodeError::HasWhitespace));
    assert_eq!(JoinCode::parse("example.org:"), Err(JoinCodeError::MissingPort));
    assert_eq!(JoinCode::parse("[::1]:"), Err(JoinCodeError::MissingPort));
    assert_eq!(JoinCode::parse("example.org:0"), Err(JoinCodeError::BadPort));
    assert_eq!(JoinCode::parse("example.org:70000"), Err(JoinCodeError::BadPort));
    assert_eq!(JoinCode::parse("example.org:abc"), Err(JoinCodeError::BadPort));
    assert_eq!(JoinCode::parse("bad host!:1"), Err(JoinCodeError::HasWhitespace));
    assert_eq!(JoinCode::parse("bad!host:1"), Err(JoinCodeError::BadHost("bad!host".into())));
    assert_eq!(JoinCode::parse(".example.org"), Err(JoinCodeError::BadHost(".example.org".into())));
    assert_eq!(JoinCode::parse("[zzz]:1"), Err(JoinCodeError::BadHost("zzz".into())));
    assert_eq!(JoinCode::parse("[::1]x"), Err(JoinCodeError::BadHost("[::1]x".into())));
    assert_eq!(JoinCode { host: "1.2.3.4".into(), port: 5 }.format(), "1.2.3.4:5");
    assert_eq!(JoinCode { host: "2001:db8::1".into(), port: 5 }.format(), "[2001:db8::1]:5");
    assert_eq!(JoinCode { host: "example.org".into(), port: 30170 }.format(), "example.org:30170");
    for code in ["1.2.3.4:30170", "[2001:db8::1]:30170", "example.org:30170"] {
        assert_eq!(JoinCode::parse(code).unwrap().format(), code);
    }
    // probe_local_ip sends nothing and never panics; offline it is simply None.
    let _ = probe_local_ip();
}

#[test]
fn display_names() {
    assert_eq!(sanitize_display_name("  Ash  "), "Ash");
    assert_eq!(sanitize_display_name(""), "Player");
    assert_eq!(sanitize_display_name("\u{7}\t\n"), "Player");
    assert_eq!(sanitize_display_name("A\u{0}sh\u{1b}"), "Ash");
    assert_eq!(sanitize_display_name(&"a".repeat(40)), "a".repeat(32));
    // Clamped on a char boundary: 10 × 'ü' (2 bytes) + 'x' = 21 bytes; 16 × 'ü' = 32; 17 → 16.
    assert_eq!(sanitize_display_name(&"ü".repeat(17)), "ü".repeat(16));
    let clamped = sanitize_display_name(&format!("{}é", "a".repeat(31)));
    assert_eq!(clamped, "a".repeat(31));
    assert!(sanitize_display_name(&"x".repeat(100)).len() <= MAX_DISPLAY_NAME_BYTES);

    let taken = vec!["Ash".to_owned(), "Ash (2)".to_owned()];
    assert_eq!(dedupe_display_name("Ash", &taken), "Ash (3)");
    assert_eq!(dedupe_display_name("Misty", &taken), "Misty");
    assert_eq!(dedupe_display_name("  Ash ", &taken), "Ash (3)");
    let long = "b".repeat(32);
    let deduped = dedupe_display_name(&long, &[long.clone()]);
    assert_eq!(deduped, format!("{} (2)", "b".repeat(28)));
    assert!(deduped.len() <= MAX_DISPLAY_NAME_BYTES);
}

#[test]
fn compatibility() {
    let base = metadata(ReplayConsoleType::GameBoyAdvance);
    assert_eq!(follow_compatibility(&base, &base), FollowCompatibility::Ok);

    let mut other = base.clone();
    other.console_type = ReplayConsoleType::GameBoy;
    other.rom_checksum = [9; 32];
    assert_eq!(follow_compatibility(&base, &other), FollowCompatibility::ConsoleMismatch { theirs: ReplayConsoleType::GameBoy });

    let mut other = base.clone();
    other.rom_checksum = [9; 32];
    other.emulator_core_name = "other".into();
    assert_eq!(follow_compatibility(&base, &other), FollowCompatibility::RomMismatch);

    let mut other = base.clone();
    other.emulator_core_name = "mGBA 0.9.0".into();
    other.bios_checksum = [9; 32];
    assert_eq!(follow_compatibility(&base, &other), FollowCompatibility::CoreMismatch { theirs: "mGBA 0.9.0".into() });

    let mut other = base.clone();
    other.bios_checksum = [9; 32];
    assert_eq!(follow_compatibility(&base, &other), FollowCompatibility::BiosMismatch);

    let text = describe_incompatibility(&FollowCompatibility::ConsoleMismatch { theirs: ReplayConsoleType::GameBoy }, "Ash");
    assert!(text.contains("Ash") && text.contains("Game Boy"), "{text}");
    assert!(describe_incompatibility(&FollowCompatibility::RomMismatch, "Ash").contains("ROM"));
    assert!(describe_incompatibility(&FollowCompatibility::CoreMismatch { theirs: "x".into() }, "Ash").contains("'x'"));
    assert!(describe_incompatibility(&FollowCompatibility::BiosMismatch, "Ash").contains("BIOS"));
    assert!(describe_incompatibility(&FollowCompatibility::Ok, "Ash").contains("Ash"));
}

/// xorshift64*: enough randomness for a smoke test, no crate needed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

#[test]
fn fuzz_smoke() {
    let mut rng = Rng(0x9E3779B97F4A7C15);
    let tags = [0x01u8, 0x02, 0x03, 0x04, 0x05, 0x06, 0x10, 0x11, 0x12, 0x13, 0x20, 0x21, 0x22, 0x2F, 0x00, 0x7F];
    for tag in tags {
        for _ in 0..10_000 {
            let len = rng.below(200);
            let mut body = vec![tag];
            body.extend((0..len).map(|_| rng.next() as u8));
            let _ = Message::decode(&body);
        }
    }
    // Random mutations of a valid Welcome: flipped bytes, truncations, insertions.
    let welcome = Message::Welcome {
        your_peer_id: 4,
        session_id: 77,
        your_display_name: "Player (2)".to_owned(),
        your_color: 3,
        participants: (1..=3).map(|i| participant(i, "p")).collect(),
    }
    .encoded();
    for _ in 0..1_000 {
        let mut m = welcome[4..].to_vec();
        for _ in 0..1 + rng.below(4) {
            match rng.below(3) {
                0 => {
                    let at = rng.below(m.len());
                    m[at] = rng.next() as u8;
                }
                1 => {
                    let at = rng.below(m.len());
                    m.truncate(at);
                    if m.is_empty() {
                        m.push(0x02);
                    }
                }
                _ => {
                    let at = rng.below(m.len());
                    m.insert(at, rng.next() as u8);
                }
            }
        }
        let _ = Message::decode(&m);
        let mut framed = (m.len() as u32).to_le_bytes().to_vec();
        framed.extend_from_slice(&m);
        let _ = Message::read(&mut Cursor::new(framed));
    }
    // Random packet streams.
    for _ in 0..2_000 {
        let len = rng.below(64);
        let bytes: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        let _ = decode_packets(&bytes);
    }
    // Random join codes and names.
    for _ in 0..2_000 {
        let len = rng.below(24);
        let s: String = (0..len).map(|_| char::from_u32(rng.below(0x250) as u32).unwrap_or('?')).collect();
        let _ = JoinCode::parse(&s);
        let name = sanitize_display_name(&s);
        assert!(name.len() <= MAX_DISPLAY_NAME_BYTES && !name.is_empty());
    }
    let _ = InputBuffer::new();
}
