//! Misbehaving peers: garbage, silence, duplicate hellos, spoofed ids, unknown targets.

mod common;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use common::*;
use supershuckie_play_together::protocol::{Message, WireSnapshot, WireStartState};
use supershuckie_play_together::*;
use supershuckie_replay_recorder::replay_file::{ReplayConsoleType, REPLAY_VERSION};
use supershuckie_replay_recorder::Speed;

struct Raw {
    stream: TcpStream,
}

impl Raw {
    fn connect(addr: SocketAddr) -> Raw {
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        stream.set_nodelay(true).unwrap();
        Raw { stream }
    }
    fn send(&mut self, m: &Message) {
        self.stream.write_all(&m.encoded()).unwrap();
    }
    fn read(&mut self) -> Option<Result<Message, DecodeError>> {
        Message::read(&mut self.stream).ok().flatten()
    }
    /// Read messages until `pred` matches (skipping pings and the like) or the stream ends.
    fn read_until(&mut self, pred: impl Fn(&Message) -> bool) -> Option<Message> {
        loop {
            match self.read()? {
                Ok(m) if pred(&m) => return Some(m),
                Ok(Message::Ping { nonce, sent_unix_millis }) => self.send(&Message::Pong { nonce, sent_unix_millis }),
                Ok(_) => {}
                Err(_) => return None,
            }
        }
    }
    fn hello(name: &str) -> Message {
        Message::Hello {
            protocol_version: PROTOCOL_VERSION,
            replay_version: REPLAY_VERSION,
            app_version: "raw".to_owned(),
            display_name: name.to_owned(),
            color: 0,
            publisher: publisher_info(ReplayConsoleType::GameBoy),
        }
    }
    fn handshake(addr: SocketAddr, name: &str) -> (Raw, PeerId) {
        let mut raw = Raw::connect(addr);
        raw.send(&Raw::hello(name));
        match raw.read() {
            Some(Ok(Message::Welcome { your_peer_id, .. })) => (raw, your_peer_id),
            other => panic!("expected Welcome, got {other:?}"),
        }
    }
    fn eof(&mut self) -> bool {
        let mut buf = [0u8; 4096];
        loop {
            match self.stream.read(&mut buf) {
                Ok(0) => return true,
                Ok(_) => {}
                Err(_) => return true,
            }
        }
    }
}

#[test]
fn garbage_before_hello_is_closed_and_the_host_goes_on() {
    let _wd = watchdog(10, "garbage_before_hello_is_closed_and_the_host_goes_on");
    let host = bind_host("Host");
    let mut hl = Vec::new();
    wait_connected(&host, &mut hl);

    let mut garbage = Raw::connect(host.local_addr());
    garbage.stream.write_all(&[0xFF; 64]).unwrap();
    // An absurd length is a protocol error: the host says so and closes.
    match garbage.read() {
        Some(Ok(Message::Error { text })) => assert!(text.contains("longer than allowed"), "{text}"),
        other => panic!("{other:?}"),
    }
    assert!(garbage.eof());

    // A known non-Hello tag before Hello is a violation too.
    let mut early = Raw::connect(host.local_addr());
    early.send(&Message::RequestSnapshot { requester: 0, target: 1 });
    match early.read() {
        Some(Ok(Message::Error { text })) => assert!(text.contains("Hello"), "{text}"),
        other => panic!("{other:?}"),
    }
    assert!(early.eof());

    // An unknown tag before Hello is skipped, then the Hello is honoured.
    let mut odd = Raw::connect(host.local_addr());
    odd.stream.write_all(&[3, 0, 0, 0, 0x7A, 1, 2]).unwrap();
    odd.send(&Raw::hello("Odd"));
    assert!(matches!(odd.read(), Some(Ok(Message::Welcome { your_peer_id: 2, .. }))));
    assert_eq!(wait_joined(&host, &mut hl).display_name, "Odd");
    assert_eq!(host.stats().unknown_messages_skipped, 1);

    let client = connect_client(&host, "Proper");
    let mut cl = Vec::new();
    assert_eq!(wait_connected(&client, &mut cl), 3);
    assert_eq!(wait_joined(&host, &mut hl).display_name, "Proper");
}

#[test]
fn never_saying_hello_times_out() {
    let _wd = watchdog(10, "never_saying_hello_times_out");
    let host = HostSession::bind(HostConfig { handshake_timeout: Duration::from_millis(400), ..host_config() }, local("Host")).unwrap();
    let mut hl = Vec::new();
    wait_connected(&host, &mut hl);
    let mut silent = Raw::connect(host.local_addr());
    let started = std::time::Instant::now();
    assert!(silent.eof(), "closed by the host");
    let took = started.elapsed();
    assert!(took >= Duration::from_millis(300) && took < Duration::from_secs(4), "{took:?}");
    assert_no_event(&host, &mut hl, Duration::from_millis(200), "Joined or Left", |e| matches!(e, SessionEvent::Joined(_) | SessionEvent::Left { .. }));
    assert!(host.participants().is_empty());
}

#[test]
fn a_second_hello_is_a_protocol_error() {
    let _wd = watchdog(10, "a_second_hello_is_a_protocol_error");
    let host = bind_host("Host");
    let mut hl = Vec::new();
    wait_connected(&host, &mut hl);
    let (mut raw, id) = Raw::handshake(host.local_addr(), "Twice");
    assert_eq!(id, 2);
    assert_eq!(wait_joined(&host, &mut hl).peer_id, 2);
    raw.send(&Raw::hello("Twice"));
    match raw.read_until(|m| matches!(m, Message::Error { .. })) {
        Some(Message::Error { text }) => assert!(text.contains("second Hello"), "{text}"),
        other => panic!("{other:?}"),
    }
    assert!(raw.eof());
    match wait_for(&host, &mut hl, "Left", |e| matches!(e, SessionEvent::Left { .. })) {
        SessionEvent::Left { peer_id, reason } => assert_eq!((peer_id, reason), (2, LeaveReason::ProtocolError)),
        other => panic!("{other:?}"),
    }
    assert!(host.participants().is_empty());
}

#[test]
fn a_spoofed_from_is_rewritten_by_the_host() {
    let _wd = watchdog(10, "a_spoofed_from_is_rewritten_by_the_host");
    let host = bind_host("Host");
    let mut hl = Vec::new();
    wait_connected(&host, &mut hl);
    let (mut liar, liar_id) = Raw::handshake(host.local_addr(), "Liar");
    let (mut witness, witness_id) = Raw::handshake(host.local_addr(), "Witness");
    assert_eq!((liar_id, witness_id), (2, 3));
    wait_joined(&host, &mut hl);
    wait_joined(&host, &mut hl);
    // The liar joined first, so it (not the witness) is told about the other's arrival.
    assert!(matches!(liar.read_until(|m| matches!(m, Message::PeerJoined { .. })), Some(Message::PeerJoined { .. })));

    liar.send(&Message::Stream { from: 7, first_frame: 5, bytes: frames(1) });
    liar.send(&Message::SyncHash { from: 1, frame: 5, hash: [9; 32] });
    liar.send(&Message::Snapshot(WireSnapshot::raw(&snapshot_at(5), 1, 3)));
    liar.send(&Message::RequestSnapshot { requester: 1, target: 3 });

    match witness.read_until(|m| matches!(m, Message::Stream { .. })) {
        Some(Message::Stream { from, first_frame, bytes }) => {
            assert_eq!(from, 2);
            assert_eq!(first_frame, 5);
            assert_eq!(bytes, frames(1));
        }
        other => panic!("{other:?}"),
    }
    match witness.read_until(|m| matches!(m, Message::SyncHash { .. })) {
        Some(Message::SyncHash { from, .. }) => assert_eq!(from, 2),
        other => panic!("{other:?}"),
    }
    match witness.read_until(|m| matches!(m, Message::Snapshot(_))) {
        Some(Message::Snapshot(s)) => {
            assert_eq!(s.from, 2);
            assert_eq!(s.target, 3);
            assert_eq!(s.into_snapshot(), Ok(snapshot_at(5)));
        }
        other => panic!("{other:?}"),
    }
    match witness.read_until(|m| matches!(m, Message::RequestSnapshot { .. })) {
        Some(Message::RequestSnapshot { requester, target }) => assert_eq!((requester, target), (2, 3)),
        other => panic!("{other:?}"),
    }
    // The host itself does not follow the liar, and a forbidden packet ends the liar.
    let mut bytes = Vec::new();
    supershuckie_replay_recorder::append_packet(
        &supershuckie_replay_recorder::Packet::Keyframe { metadata: Default::default(), state: vec![0u8; 8].into_iter().collect() },
        &mut bytes,
    );
    liar.send(&Message::Stream { from: 2, first_frame: 6, bytes });
    match liar.read_until(|m| matches!(m, Message::Error { .. })) {
        Some(Message::Error { text }) => assert!(text.contains("Keyframe"), "{text}"),
        other => panic!("{other:?}"),
    }
    match wait_for(&host, &mut hl, "Left", |e| matches!(e, SessionEvent::Left { .. })) {
        SessionEvent::Left { peer_id, reason } => assert_eq!((peer_id, reason), (2, LeaveReason::ProtocolError)),
        other => panic!("{other:?}"),
    }
    match witness.read_until(|m| matches!(m, Message::PeerLeft { .. })) {
        Some(Message::PeerLeft { peer_id, reason }) => assert_eq!((peer_id, reason), (2, LeaveReason::ProtocolError)),
        other => panic!("{other:?}"),
    }
}

#[test]
fn only_the_host_sets_sync_pause_and_a_pause_carries_its_real_sender() {
    let _wd = watchdog(10, "only_the_host_sets_sync_pause_and_a_pause_carries_its_real_sender");
    let host = bind_host("Host");
    let mut hl = Vec::new();
    wait_connected(&host, &mut hl);
    host.set_sync_pause(true, false).unwrap();
    let (mut liar, liar_id) = Raw::handshake(host.local_addr(), "Liar");
    let (mut witness, witness_id) = Raw::handshake(host.local_addr(), "Witness");
    assert_eq!((liar_id, witness_id), (2, 3));
    wait_joined(&host, &mut hl);
    wait_joined(&host, &mut hl);

    // Each was told the setting right after its Welcome.
    assert_eq!(witness.read_until(|m| matches!(m, Message::SyncPause { .. })), Some(Message::SyncPause { enabled: true, paused: false }));

    // A spoofed sender is rewritten.
    liar.send(&Message::Pause { from: 3, paused: true });
    assert_eq!(witness.read_until(|m| matches!(m, Message::Pause { .. })), Some(Message::Pause { from: 2, paused: true }));
    assert_eq!(wait_for(&host, &mut hl, "PauseChanged", |e| matches!(e, SessionEvent::PauseChanged { .. })), SessionEvent::PauseChanged { from: 2, paused: true });

    // A client that claims to set the session's start state, or its setting, is dropped.
    liar.send(&Message::StartState(WireStartState::cleared()));
    match liar.read_until(|m| matches!(m, Message::Error { .. })) {
        Some(Message::Error { text }) => assert!(text.contains("only the host"), "{text}"),
        other => panic!("{other:?}"),
    }
    assert!(liar.eof());
    match wait_for(&host, &mut hl, "Left", |e| matches!(e, SessionEvent::Left { .. })) {
        SessionEvent::Left { peer_id, reason } => assert_eq!((peer_id, reason), (2, LeaveReason::ProtocolError)),
        other => panic!("{other:?}"),
    }
    let (mut liar, _) = Raw::handshake(host.local_addr(), "Liar");
    wait_joined(&host, &mut hl);
    liar.send(&Message::SyncPause { enabled: false, paused: false });
    match liar.read_until(|m| matches!(m, Message::Error { .. })) {
        Some(Message::Error { text }) => assert!(text.contains("only the host"), "{text}"),
        other => panic!("{other:?}"),
    }
    assert!(liar.eof());
    match wait_for(&host, &mut hl, "Left", |e| matches!(e, SessionEvent::Left { .. })) {
        SessionEvent::Left { peer_id, reason } => assert_eq!((peer_id, reason), (4, LeaveReason::ProtocolError)),
        other => panic!("{other:?}"),
    }
    // The setting survives the attempts: a newcomer still hears it, and the host's speed after it.
    let (mut late, _) = Raw::handshake(host.local_addr(), "Late");
    assert_eq!(late.read_until(|m| matches!(m, Message::SyncPause { .. })), Some(Message::SyncPause { enabled: true, paused: true }));
    assert_eq!(late.read_until(|m| matches!(m, Message::LinkSpeed { .. })), Some(Message::LinkSpeed { speed: Speed::default() }));
    host.set_link_speed(Speed::from_multiplier_float(3.0)).unwrap();
    assert_eq!(late.read_until(|m| matches!(m, Message::LinkSpeed { .. })), Some(Message::LinkSpeed { speed: Speed::from_multiplier_float(3.0) }));
    // A client claiming to set it is thrown out like the pause liar.
    let (mut speeder, _) = Raw::handshake(host.local_addr(), "Speeder");
    wait_joined(&host, &mut hl);
    speeder.send(&Message::LinkSpeed { speed: Speed::from_multiplier_float(8.0) });
    match speeder.read_until(|m| matches!(m, Message::Error { .. })) {
        Some(Message::Error { text }) => assert!(text.contains("only the host"), "{text}"),
        other => panic!("{other:?}"),
    }
    assert!(speeder.eof());
}

#[test]
fn unknown_targets_are_ignored() {
    let _wd = watchdog(10, "unknown_targets_are_ignored");
    let host = bind_host("Host");
    let mut hl = Vec::new();
    wait_connected(&host, &mut hl);
    let (mut raw, _) = Raw::handshake(host.local_addr(), "Aim");
    wait_joined(&host, &mut hl);
    raw.send(&Message::Snapshot(WireSnapshot::raw(&snapshot_at(1), 2, 99)));
    raw.send(&Message::RequestSnapshot { requester: 0, target: 99 });
    raw.send(&Message::RequestSnapshot { requester: 0, target: 0 });
    raw.send(&Message::RequestSnapshot { requester: 0, target: 2 });
    // Still connected: a ping comes back.
    raw.send(&Message::Ping { nonce: 77, sent_unix_millis: 1 });
    match raw.read_until(|m| matches!(m, Message::Pong { nonce: 77, .. })) {
        Some(Message::Pong { nonce, sent_unix_millis }) => assert_eq!((nonce, sent_unix_millis), (77, 1)),
        other => panic!("{other:?}"),
    }
    assert_no_event(&host, &mut hl, Duration::from_millis(300), "anything but rtt", |e| {
        !matches!(e, SessionEvent::RttUpdated { .. })
    });
    assert_eq!(host.participants().len(), 1);

    // A snapshot for the host from a peer it does not follow is ignored; one it follows lands.
    raw.send(&Message::Snapshot(WireSnapshot::raw(&snapshot_at(2), 2, 1)));
    // The host handles messages in order: once the pong is back, the snapshot has been dropped.
    raw.send(&Message::Ping { nonce: 78, sent_unix_millis: 1 });
    assert!(raw.read_until(|m| matches!(m, Message::Pong { nonce: 78, .. })).is_some());
    let (rec, sink) = sink();
    host.subscribe(2, sink).unwrap();
    match raw.read_until(|m| matches!(m, Message::RequestSnapshot { .. })) {
        Some(Message::RequestSnapshot { requester, target }) => assert_eq!((requester, target), (1, 2)),
        other => panic!("{other:?}"),
    }
    raw.send(&Message::Snapshot(WireSnapshot::compressed(&snapshot_at(3), 2, 0)));
    raw.send(&Message::Stream { from: 2, first_frame: 3, bytes: frames(2) });
    wait_until("host follows the raw peer", || rec.lock().unwrap().frames == 2);
    let rec = rec.lock().unwrap();
    assert_eq!(rec.snapshots.len(), 1);
    assert_eq!(rec.snapshots[0].frame, 3);
}

#[test]
fn a_bad_snapshot_or_message_from_a_client_is_a_protocol_error() {
    let _wd = watchdog(10, "a_bad_snapshot_or_message_from_a_client_is_a_protocol_error");
    let host = bind_host("Host");
    let mut hl = Vec::new();
    wait_connected(&host, &mut hl);
    let (mut raw, _) = Raw::handshake(host.local_addr(), "Bad");
    wait_joined(&host, &mut hl);
    // Only the host sends Welcome.
    raw.send(&Message::Welcome { your_peer_id: 9, session_id: 1, your_display_name: "x".into(), your_color: 1, participants: vec![] });
    match raw.read_until(|m| matches!(m, Message::Error { .. })) {
        Some(Message::Error { text }) => assert!(text.contains("host"), "{text}"),
        other => panic!("{other:?}"),
    }
    assert!(raw.eof());
    wait_for(&host, &mut hl, "Left", |e| matches!(e, SessionEvent::Left { peer_id: 2, reason: LeaveReason::ProtocolError }));

    // A snapshot addressed to the host with a state that does not decompress.
    let (mut raw, _) = Raw::handshake(host.local_addr(), "Bad2");
    wait_joined(&host, &mut hl);
    let (_rec, sink) = sink();
    host.subscribe(3, sink).unwrap();
    let mut wire = WireSnapshot::compressed(&snapshot_at(1), 3, 1);
    wire.state = vec![1, 2, 3, 4];
    raw.send(&Message::Snapshot(wire));
    match raw.read_until(|m| matches!(m, Message::Error { .. })) {
        Some(Message::Error { text }) => assert!(text.contains("decode"), "{text}"),
        other => panic!("{other:?}"),
    }
    wait_for(&host, &mut hl, "Left", |e| matches!(e, SessionEvent::Left { peer_id: 3, reason: LeaveReason::ProtocolError }));
}
