//! Real sessions over 127.0.0.1: joining, publishing, following, snapshots, resets, leaving,
//! refusals, gaps and ordering.

mod common;

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use supershuckie_play_together::protocol::Message;
use supershuckie_play_together::transport::{TcpConnection, TcpListenerHandle, TcpTransport};
use supershuckie_play_together::*;
use supershuckie_replay_recorder::replay_file::{ReplayConsoleType, ReplayPatchFormat, REPLAY_VERSION};
use supershuckie_replay_recorder::Packet;

#[test]
fn host_and_two_clients_exchange_frames() {
    let _wd = watchdog(10, "host_and_two_clients_exchange_frames");
    let host = bind_host("Host");
    let mut hl = Vec::new();
    assert_eq!(wait_connected(&host, &mut hl), 1);
    assert_eq!(host.role(), Role::Host);
    assert_eq!(host.local_peer_id(), 1);

    let a = connect_client(&host, "A");
    let b = connect_client(&host, "B");
    let (mut al, mut bl) = (Vec::new(), Vec::new());
    assert_eq!(wait_connected(&a, &mut al), 2);
    assert_eq!(wait_connected(&b, &mut bl), 3);
    assert_eq!(a.role(), Role::Client);
    assert_eq!(a.session_id(), host.session_id());
    assert_ne!(host.session_id(), 0);
    assert_eq!(wait_joined(&host, &mut hl).peer_id, 2);
    assert_eq!(wait_joined(&host, &mut hl).peer_id, 3);
    assert_eq!(wait_joined(&a, &mut al).peer_id, 3);
    assert_eq!(host.participants().len(), 2);
    assert_eq!(a.participants().iter().map(|p| p.peer_id).collect::<Vec<_>>(), vec![1, 3]);
    assert_eq!(b.participants().iter().map(|p| p.peer_id).collect::<Vec<_>>(), vec![1, 2]);
    assert!(a.is_connected() && b.is_connected() && host.is_connected());

    // Everybody follows everybody else.
    let sessions: [(&dyn Session, PeerId); 3] = [(&host, 1), (&a, 2), (&b, 3)];
    let mut sinks = Vec::new();
    for (follower, me) in sessions {
        for (_, publisher) in sessions {
            if publisher != me {
                let (rec, boxed) = sink();
                follower.subscribe(publisher, boxed).expect("subscribe");
                sinks.push((me, publisher, rec));
            }
        }
    }
    // Every publisher hears from both followers (in one coalesced request, or two when the
    // relayed one lands after the first flush) and answers with a broadcast snapshot at frame 0.
    for (session, me) in sessions {
        let mut log = Vec::new();
        let mut requesters = wait_snapshot_requested(session, &mut log);
        while requesters.len() < 2 {
            requesters.extend(wait_snapshot_requested(session, &mut log));
        }
        requesters.sort();
        let mut expected: Vec<PeerId> = sessions.iter().map(|(_, id)| *id).filter(|id| *id != me).collect();
        expected.sort();
        assert_eq!(requesters, expected);
        session.publisher().publish_snapshot(snapshot_at(0), 0).expect("snapshot");
    }
    wait_until("every sink has its snapshot", || sinks.iter().all(|(_, _, rec)| rec.lock().unwrap().snapshots.len() == 1));

    // 100 frames each, one publish per frame.
    for (session, _) in sessions {
        publish_frames(&session.publisher(), 0, 100);
    }
    wait_until("100 frames everywhere", || sinks.iter().all(|(_, _, rec)| rec.lock().unwrap().frames == 100));
    for (me, publisher, rec) in &sinks {
        let rec = rec.lock().unwrap();
        assert_eq!(rec.snapshots[0].frame, 0);
        let mut expected = 0;
        for (first_frame, packets) in &rec.packets {
            assert_eq!(*first_frame, expected, "follower {me} of {publisher}: contiguous first_frames");
            expected += count_frames(packets);
            assert!(packets.iter().all(|p| matches!(p, Packet::ChangeInput { .. } | Packet::NextFrame { .. })));
        }
        assert_eq!(expected, 100);
        assert_eq!(rec.order[0], "snapshot");
    }
    let stats = host.stats();
    assert!(stats.bytes_in > 0 && stats.bytes_out > 0 && stats.messages_in > 0 && stats.messages_out > 0);
    assert_eq!(stats.unknown_messages_skipped, 0);
}

#[test]
fn targeted_snapshot_reaches_only_the_requester() {
    let _wd = watchdog(10, "targeted_snapshot_reaches_only_the_requester");
    let host = bind_host("Host");
    let a = connect_client(&host, "A");
    let b = connect_client(&host, "B");
    let (mut hl, mut al, mut bl) = (Vec::new(), Vec::new(), Vec::new());
    wait_connected(&a, &mut al);
    wait_connected(&b, &mut bl);
    wait_joined(&host, &mut hl);
    wait_joined(&host, &mut hl);

    let (rec_a, sink_a) = sink();
    a.subscribe(1, sink_a).unwrap();
    assert_eq!(wait_snapshot_requested(&host, &mut hl), vec![2]);
    // Streams before the snapshot are discarded by the follower.
    publish_frames(&host.publisher(), 0, 5);
    host.publisher().publish_snapshot(snapshot_at(5), 2).unwrap();
    publish_frames(&host.publisher(), 5, 5);
    wait_until("A live from frame 5", || rec_a.lock().unwrap().frames == 5);
    {
        let rec = rec_a.lock().unwrap();
        assert_eq!(rec.snapshots.len(), 1);
        assert_eq!(rec.snapshots[0].frame, 5);
        assert_eq!(rec.snapshots[0], snapshot_at(5), "the snapshot survives compression");
        assert_eq!(rec.packets[0].0, 5, "first stream after the snapshot starts at its frame");
        assert_eq!(rec.order[0], "snapshot");
    }

    // B subscribes later; its request is rate limited to two seconds after the last one.
    let (rec_b, sink_b) = sink();
    b.subscribe(1, sink_b).unwrap();
    assert_eq!(wait_snapshot_requested(&host, &mut hl), vec![3]);
    host.publisher().publish_snapshot(snapshot_at(10), 3).unwrap();
    publish_frames(&host.publisher(), 10, 3);
    wait_until("B live from frame 10", || rec_b.lock().unwrap().frames == 3);
    wait_until("A got frames 10..13 too", || rec_a.lock().unwrap().frames == 8);
    assert_eq!(rec_b.lock().unwrap().snapshots[0].frame, 10);
    assert_eq!(rec_a.lock().unwrap().snapshots.len(), 1, "the snapshot for B did not reach A");
}

#[test]
fn snapshot_requests_are_coalesced_and_a_broadcast_reaches_both() {
    let _wd = watchdog(10, "snapshot_requests_are_coalesced_and_a_broadcast_reaches_both");
    let host = bind_host("Host");
    let a = connect_client(&host, "A");
    let b = connect_client(&host, "B");
    let (mut hl, mut al, mut bl) = (Vec::new(), Vec::new(), Vec::new());
    wait_connected(&a, &mut al);
    wait_connected(&b, &mut bl);
    wait_joined(&host, &mut hl);
    wait_joined(&host, &mut hl);

    // Both follow A (a client publisher, relayed through the host).
    let (rec_h, sink_h) = sink();
    let (rec_b, sink_b) = sink();
    host.subscribe(2, sink_h).unwrap();
    b.subscribe(2, sink_b).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let mut requesters = wait_snapshot_requested(&a, &mut al);
    requesters.sort_unstable();
    assert_eq!(requesters, vec![1, 3], "one coalesced request with both requesters");
    a.publisher().publish_snapshot(snapshot_at(42), 0).unwrap();
    publish_frames(&a.publisher(), 42, 4);
    wait_until("host and B got the broadcast snapshot", || rec_h.lock().unwrap().frames == 4 && rec_b.lock().unwrap().frames == 4);
    assert_eq!(rec_h.lock().unwrap().snapshots[0].frame, 42);
    assert_eq!(rec_b.lock().unwrap().snapshots[0].frame, 42);
    assert_eq!(rec_b.lock().unwrap().packets[0].0, 42);
    assert_no_event(&a, &mut al, Duration::from_millis(200), "second SnapshotRequested", |e| matches!(e, SessionEvent::SnapshotRequested { .. }));
}

#[test]
fn reset_all_reaches_everyone() {
    let _wd = watchdog(10, "reset_all_reaches_everyone");
    let host = bind_host("Host");
    let a = connect_client(&host, "A");
    let b = connect_client(&host, "B");
    let (mut hl, mut al, mut bl) = (Vec::new(), Vec::new(), Vec::new());
    wait_connected(&a, &mut al);
    wait_connected(&b, &mut bl);
    wait_joined(&host, &mut hl);
    wait_joined(&host, &mut hl);

    assert!(matches!(a.send_reset_all(Duration::from_secs(1)), Err(PlayTogetherError::NotHost)));
    assert!(matches!(a.kick(1), Err(PlayTogetherError::NotHost)));
    let race_id = host.send_reset_all(Duration::from_secs(3)).unwrap();
    for (session, log) in [(&host as &dyn Session, &mut hl), (&a, &mut al), (&b, &mut bl)] {
        match wait_for(session, log, "ResetAll", |e| matches!(e, SessionEvent::ResetAll { .. })) {
            SessionEvent::ResetAll { race_id: id, deadline } => {
                assert_eq!(id, race_id);
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                assert!(left > Duration::from_secs(2) && left <= Duration::from_secs(3), "{left:?}");
            }
            other => panic!("{other:?}"),
        }
    }
    let second = host.send_reset_all(Duration::ZERO).unwrap();
    assert_ne!(second, race_id);
}

#[test]
fn client_leave_is_seen_by_everyone_and_ends_sinks() {
    let _wd = watchdog(10, "client_leave_is_seen_by_everyone_and_ends_sinks");
    let host = bind_host("Host");
    let a = connect_client(&host, "A");
    let b = connect_client(&host, "B");
    let (mut hl, mut al, mut bl) = (Vec::new(), Vec::new(), Vec::new());
    wait_connected(&a, &mut al);
    wait_connected(&b, &mut bl);
    wait_joined(&host, &mut hl);
    wait_joined(&host, &mut hl);
    wait_joined(&a, &mut al);

    let (rec_h, sink_h) = sink();
    let (rec_b, sink_b) = sink();
    host.subscribe(2, sink_h).unwrap();
    b.subscribe(2, sink_b).unwrap();
    std::thread::sleep(Duration::from_millis(100));

    a.leave();
    assert!(!a.is_connected());
    assert!(matches!(a.publisher().publish(0, frames(1)), Err(PublishError::Disconnected(DisconnectReason::Left))));
    assert_eq!(wait_disconnected(&a, &mut al), DisconnectReason::Left);
    match wait_for(&host, &mut hl, "Left at host", |e| matches!(e, SessionEvent::Left { .. })) {
        SessionEvent::Left { peer_id, reason } => assert_eq!((peer_id, reason), (2, LeaveReason::Left)),
        other => panic!("{other:?}"),
    }
    match wait_for(&b, &mut bl, "Left at B", |e| matches!(e, SessionEvent::Left { .. })) {
        SessionEvent::Left { peer_id, reason } => assert_eq!((peer_id, reason), (2, LeaveReason::Left)),
        other => panic!("{other:?}"),
    }
    assert_eq!(rec_h.lock().unwrap().ended, vec![LeaveReason::Left]);
    assert_eq!(rec_b.lock().unwrap().ended, vec![LeaveReason::Left]);
    assert_eq!(host.participants().len(), 1);
    assert_eq!(b.participants().iter().map(|p| p.peer_id).collect::<Vec<_>>(), vec![1]);
    assert!(matches!(host.subscribe(2, sink().1), Err(PlayTogetherError::NoSuchPeer(2))));

    // A new client gets a fresh id, never A's.
    let c = connect_client(&host, "C");
    let mut cl = Vec::new();
    assert_eq!(wait_connected(&c, &mut cl), 4);
}

#[test]
fn host_leave_disconnects_clients_and_frees_the_port() {
    let _wd = watchdog(10, "host_leave_disconnects_clients_and_frees_the_port");
    let host = bind_host("Host");
    let port = host.local_addr().port();
    let a = connect_client(&host, "A");
    let (mut hl, mut al) = (Vec::new(), Vec::new());
    wait_connected(&a, &mut al);
    wait_joined(&host, &mut hl);
    let (rec, sink_a) = sink();
    a.subscribe(1, sink_a).unwrap();
    std::thread::sleep(Duration::from_millis(100));

    host.leave();
    assert!(!host.is_connected());
    assert_eq!(wait_disconnected(&a, &mut al), DisconnectReason::HostLeft);
    assert_eq!(wait_disconnected(&host, &mut hl), DisconnectReason::Left);
    assert_eq!(rec.lock().unwrap().ended, vec![LeaveReason::HostLeft]);
    assert!(!a.is_connected());
    assert!(matches!(a.publisher().publish(0, frames(1)), Err(PublishError::Disconnected(DisconnectReason::HostLeft))));
    assert!(matches!(host.publisher().publish(0, frames(1)), Err(PublishError::Disconnected(DisconnectReason::Left))));

    let again = HostSession::bind(HostConfig { port, ..host_config() }, local("Host")).expect("the port is free again");
    assert_eq!(again.local_addr().port(), port);
    drop(host);
    drop(again);
}

#[test]
fn kicked_client_learns_it_and_the_others_see_it() {
    let _wd = watchdog(10, "kicked_client_learns_it_and_the_others_see_it");
    let host = bind_host("Host");
    let a = connect_client(&host, "A");
    let b = connect_client(&host, "B");
    let (mut hl, mut al, mut bl) = (Vec::new(), Vec::new(), Vec::new());
    wait_connected(&a, &mut al);
    wait_connected(&b, &mut bl);
    wait_joined(&host, &mut hl);
    wait_joined(&host, &mut hl);
    assert!(matches!(host.kick(9), Err(PlayTogetherError::NoSuchPeer(9))));
    host.kick(2).unwrap();
    assert_eq!(wait_disconnected(&a, &mut al), DisconnectReason::Kicked);
    match wait_for(&b, &mut bl, "Left at B", |e| matches!(e, SessionEvent::Left { .. })) {
        SessionEvent::Left { peer_id, reason } => assert_eq!((peer_id, reason), (2, LeaveReason::Kicked)),
        other => panic!("{other:?}"),
    }
    match wait_for(&host, &mut hl, "Left at host", |e| matches!(e, SessionEvent::Left { .. })) {
        SessionEvent::Left { peer_id, reason } => assert_eq!((peer_id, reason), (2, LeaveReason::Kicked)),
        other => panic!("{other:?}"),
    }
}

/// A raw client that speaks the protocol by hand.
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
    fn hello(protocol_version: u32, replay_version: u32, publisher: PublisherInfo) -> Message {
        Message::Hello { protocol_version, replay_version, app_version: "raw".to_owned(), display_name: "Raw".to_owned(), publisher }
    }
}

#[test]
fn refusals() {
    let _wd = watchdog(15, "refusals");
    let host = bind_host("Host");
    let mut hl = Vec::new();
    wait_connected(&host, &mut hl);
    let addr = host.local_addr();

    // Protocol and replay version, via a raw client (a real one always sends the right ones).
    for (hello, expected) in [
        (Raw::hello(2, REPLAY_VERSION, publisher_info(ReplayConsoleType::GameBoy)), RefusalReason::ProtocolVersion),
        (Raw::hello(1, REPLAY_VERSION + 1, publisher_info(ReplayConsoleType::GameBoy)), RefusalReason::ReplayVersion),
        (Raw::hello(1, REPLAY_VERSION, publisher_info(ReplayConsoleType::Unknown)), RefusalReason::ConsoleUnsupported),
    ] {
        let mut raw = Raw::connect(addr);
        raw.send(&hello);
        match raw.read() {
            Some(Ok(Message::Refused { reason, text })) => {
                assert_eq!(reason, expected);
                assert!(!text.is_empty());
            }
            other => panic!("expected Refused({expected:?}), got {other:?}"),
        }
        assert_eq!(raw.read(), None, "closed after the refusal");
    }

    // Nintendo DS and a patched ROM, via real clients.
    let mut nds = publisher_info(ReplayConsoleType::NintendoDS);
    nds.metadata.emulator_core_name = "melonDS".to_owned();
    let mut patched = publisher_info(ReplayConsoleType::GameBoyAdvance);
    patched.metadata.patch_format = ReplayPatchFormat::BPS;
    for (publisher, expected, needle) in [(nds, RefusalReason::ConsoleUnsupported, "Nintendo DS"), (patched, RefusalReason::PatchedRomUnsupported, "patched")] {
        let client = ClientSession::connect(code_for(&host), client_config(), local_with("X", publisher));
        let mut log = Vec::new();
        match wait_disconnected(&client, &mut log) {
            DisconnectReason::Refused { reason, text } => {
                assert_eq!(reason, expected);
                assert!(text.contains(needle), "{text}");
            }
            other => panic!("{other:?}"),
        }
        assert!(!client.is_connected());
        assert_eq!(client.local_peer_id(), 0);
        assert!(matches!(client.publisher().publish(0, frames(1)), Err(PublishError::Disconnected(_))));
    }

    // A ninth participant.
    let clients: Vec<ClientSession> = (0..7).map(|i| connect_client(&host, &format!("C{i}"))).collect();
    for client in &clients {
        wait_connected(client, &mut Vec::new());
    }
    let ninth = connect_client(&host, "Ninth");
    let mut nl = Vec::new();
    match wait_disconnected(&ninth, &mut nl) {
        DisconnectReason::Refused { reason, .. } => assert_eq!(reason, RefusalReason::SessionFull),
        other => panic!("{other:?}"),
    }
    let joined: Vec<PeerId> = std::iter::from_fn(|| Some(wait_joined(&host, &mut hl).peer_id)).take(7).collect();
    assert_eq!(joined, (2..=8).collect::<Vec<PeerId>>());
    assert_no_event(&host, &mut hl, Duration::from_millis(300), "Joined for a refused client", |e| matches!(e, SessionEvent::Joined(_)));
    assert_eq!(host.participants().len(), 7);
}

#[test]
fn display_names_are_deduplicated_by_the_host() {
    let _wd = watchdog(10, "display_names_are_deduplicated_by_the_host");
    let host = bind_host("  Ash\u{7} ");
    let mut hl = Vec::new();
    match wait_for(&host, &mut hl, "Connected", |e| matches!(e, SessionEvent::Connected { .. })) {
        SessionEvent::Connected { local_display_name, .. } => assert_eq!(local_display_name, "Ash"),
        other => panic!("{other:?}"),
    }
    let a = connect_client(&host, "Ash");
    let b = connect_client(&host, "Ash");
    let (mut al, mut bl) = (Vec::new(), Vec::new());
    let name_of = |event: SessionEvent| match event {
        SessionEvent::Connected { local_display_name, participants, .. } => (local_display_name, participants.len()),
        other => panic!("{other:?}"),
    };
    let connected = |session: &dyn Session, log: &mut Vec<SessionEvent>| wait_for(session, log, "Connected", |e| matches!(e, SessionEvent::Connected { .. }));
    assert_eq!(name_of(connected(&a, &mut al)), ("Ash (2)".to_owned(), 1));
    assert_eq!(name_of(connected(&b, &mut bl)), ("Ash (3)".to_owned(), 2));
}

// ---------------------------------------------------------------------------------------------
// A transport that drops one Stream message on request (the gap test).

#[derive(Clone)]
struct DroppingTransport {
    drop_next_stream: Arc<AtomicBool>,
}

struct DroppingConnection {
    inner: TcpConnection,
    drop_next_stream: Arc<AtomicBool>,
}

struct DroppingWrite {
    inner: TcpStream,
    drop_next_stream: Arc<AtomicBool>,
    pending: Vec<u8>,
}

impl Write for DroppingWrite {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.pending.extend_from_slice(buf);
        loop {
            if self.pending.len() < 4 {
                break;
            }
            let len = u32::from_le_bytes(self.pending[..4].try_into().unwrap()) as usize;
            if self.pending.len() < 4 + len {
                break;
            }
            let frame: Vec<u8> = self.pending.drain(..4 + len).collect();
            if frame[4] == 0x10 && self.drop_next_stream.swap(false, Ordering::AcqRel) {
                continue;
            }
            self.inner.write_all(&frame)?;
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl Connection for DroppingConnection {
    type Read = TcpStream;
    type Write = DroppingWrite;
    fn peer_label(&self) -> String {
        self.inner.peer_label()
    }
    fn set_timeouts(&self, read: Option<Duration>, write: Option<Duration>) -> io::Result<()> {
        self.inner.set_timeouts(read, write)
    }
    fn split(&self) -> io::Result<(Self::Read, Self::Write)> {
        let (read, write) = self.inner.split()?;
        Ok((read, DroppingWrite { inner: write, drop_next_stream: Arc::clone(&self.drop_next_stream), pending: Vec::new() }))
    }
    fn shutdown(&self) {
        self.inner.shutdown()
    }
}

struct DroppingListener {
    inner: TcpListenerHandle,
    drop_next_stream: Arc<AtomicBool>,
}

impl Listener for DroppingListener {
    type Connection = DroppingConnection;
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
    fn try_accept(&self) -> io::Result<Option<Self::Connection>> {
        Ok(self.inner.try_accept()?.map(|inner| DroppingConnection { inner, drop_next_stream: Arc::clone(&self.drop_next_stream) }))
    }
}

impl Transport for DroppingTransport {
    type Listener = DroppingListener;
    type Connection = DroppingConnection;
    fn listen(&self, bind_address: &str, port: u16) -> io::Result<Self::Listener> {
        Ok(DroppingListener { inner: TcpTransport.listen(bind_address, port)?, drop_next_stream: Arc::clone(&self.drop_next_stream) })
    }
    fn connect(&self, address: SocketAddr, timeout: Duration) -> io::Result<Self::Connection> {
        Ok(DroppingConnection { inner: TcpTransport.connect(address, timeout)?, drop_next_stream: Arc::clone(&self.drop_next_stream) })
    }
}

#[test]
fn a_gap_in_a_stream_asks_for_a_new_snapshot() {
    let _wd = watchdog(10, "a_gap_in_a_stream_asks_for_a_new_snapshot");
    let host = bind_host("Host");
    let drop_next = Arc::new(AtomicBool::new(false));
    let a = ClientSession::connect_with(DroppingTransport { drop_next_stream: Arc::clone(&drop_next) }, code_for(&host), client_config(), local("A"));
    let (mut hl, mut al) = (Vec::new(), Vec::new());
    wait_connected(&a, &mut al);
    wait_joined(&host, &mut hl);

    let (rec, sink_h) = sink();
    host.subscribe(2, sink_h).unwrap();
    assert_eq!(wait_snapshot_requested(&a, &mut al), vec![1]);
    let publisher = a.publisher();
    publisher.publish_snapshot(snapshot_at(0), 1).unwrap();
    publish_frames(&publisher, 0, 10);
    wait_until("host has 10 frames", || rec.lock().unwrap().frames == 10);

    drop_next.store(true, Ordering::Release);
    publisher.publish(10, frames(1)).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    publisher.publish(11, frames(1)).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    publisher.publish(12, frames(1)).unwrap();

    match wait_for(&host, &mut hl, "gap warning", |e| matches!(e, SessionEvent::Warning(_))) {
        SessionEvent::Warning(text) => assert!(text.contains("expected frame 10"), "{text}"),
        other => panic!("{other:?}"),
    }
    // The host went back to awaiting a snapshot: nothing past the gap reached the sink.
    assert_eq!(rec.lock().unwrap().frames, 10);
    // The publisher hears the (rate-limited) second request within two seconds.
    assert_eq!(wait_snapshot_requested(&a, &mut al), vec![1]);
    publisher.publish_snapshot(snapshot_at(13), 1).unwrap();
    publisher.publish(13, frames(1)).unwrap();
    wait_until("host live again", || rec.lock().unwrap().frames == 11);
    let rec = rec.lock().unwrap();
    assert_eq!(rec.snapshots.len(), 2);
    assert_eq!(rec.packets.last().unwrap().0, 13);
}

#[test]
fn sync_hashes_keep_their_place_between_streams() {
    let _wd = watchdog(10, "sync_hashes_keep_their_place_between_streams");
    let host = bind_host("Host");
    let a = connect_client(&host, "A");
    let (mut hl, mut al) = (Vec::new(), Vec::new());
    wait_connected(&a, &mut al);
    wait_joined(&host, &mut hl);

    let (rec, sink_a) = sink();
    a.subscribe(1, sink_a).unwrap();
    assert_eq!(wait_snapshot_requested(&host, &mut hl), vec![2]);
    let publisher = host.publisher();
    // A hash before the snapshot is discarded (not live yet).
    publisher.publish_sync_hash(0, [1; 32]).unwrap();
    publisher.publish_snapshot(snapshot_at(0), 2).unwrap();
    publisher.publish(0, frames(5)).unwrap();
    publisher.publish_sync_hash(5, [5; 32]).unwrap();
    publisher.publish(5, frames(5)).unwrap();
    publisher.publish_sync_hash(10, [10; 32]).unwrap();
    wait_until("two hashes", || rec.lock().unwrap().hashes.len() == 2);
    let rec = rec.lock().unwrap();
    assert_eq!(rec.order, vec!["snapshot", "packets", "hash", "packets", "hash"]);
    assert_eq!(rec.hashes, vec![(5, [5; 32]), (10, [10; 32])]);
    assert_eq!(rec.frames, 10);
}

#[test]
fn connect_failures_are_reported() {
    let _wd = watchdog(10, "connect_failures_are_reported");
    // A port nobody listens on.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let client = ClientSession::connect(JoinCode { host: "127.0.0.1".into(), port }, client_config(), local("A"));
    let mut log = Vec::new();
    assert!(matches!(wait_disconnected(&client, &mut log), DisconnectReason::ConnectFailed(_)));
    assert!(!client.is_connected());
    assert!(matches!(client.subscribe(1, sink().1), Err(PlayTogetherError::Disconnected(_))));

    // A host that never answers the Hello: handshake timeout.
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = silent.local_addr().unwrap().port();
    let keep = std::thread::spawn(move || {
        let (stream, _) = silent.accept().unwrap();
        std::thread::sleep(Duration::from_secs(3));
        drop(stream);
    });
    let config = ClientConfig { handshake_timeout: Duration::from_millis(500), ..client_config() };
    let client = ClientSession::connect(JoinCode { host: "127.0.0.1".into(), port }, config, local("A"));
    let mut log = Vec::new();
    assert_eq!(wait_disconnected(&client, &mut log), DisconnectReason::Timeout(Phase::Handshake));
    assert!(matches!(client.publisher().publish(0, frames(1)), Err(PublishError::Disconnected(_))));
    drop(client);
    keep.join().unwrap();
}

#[test]
fn publishing_before_connected_is_refused() {
    let _wd = watchdog(10, "publishing_before_connected_is_refused");
    let host = bind_host("Host");
    let a = connect_client(&host, "A");
    // Before Connected the client has no id: nothing can be published or followed yet.
    let first = a.publisher().publish(0, frames(1));
    assert!(matches!(first, Ok(()) | Err(PublishError::NotConnected)), "{first:?}");
    let mut al = Vec::new();
    wait_connected(&a, &mut al);
    a.publisher().publish(0, frames(1)).unwrap();
    a.publisher().publish_sync_hash(1, [0; 32]).unwrap();
    assert!(matches!(a.subscribe(7, sink().1), Err(PlayTogetherError::NoSuchPeer(7))));
    a.unsubscribe(7);
    a.request_snapshot(7);
}

#[test]
fn rtt_is_measured_when_idle() {
    let _wd = watchdog(10, "rtt_is_measured_when_idle");
    let host = bind_host("Host");
    let a = connect_client(&host, "A");
    let (mut hl, mut al) = (Vec::new(), Vec::new());
    wait_connected(&a, &mut al);
    wait_joined(&host, &mut hl);
    match wait_for(&a, &mut al, "RttUpdated", |e| matches!(e, SessionEvent::RttUpdated { .. })) {
        SessionEvent::RttUpdated { peer_id, rtt } => {
            assert_eq!(peer_id, 1);
            assert!(rtt < Duration::from_secs(1));
        }
        other => panic!("{other:?}"),
    }
    wait_for(&host, &mut hl, "RttUpdated at host", |e| matches!(e, SessionEvent::RttUpdated { peer_id: 2, .. }));
    wait_until("stats carry the rtt", || !a.stats().rtt.is_empty() && !host.stats().rtt.is_empty());
    assert_eq!(a.stats().rtt[0].0, 1);
    assert_eq!(host.stats().rtt[0].0, 2);
}
