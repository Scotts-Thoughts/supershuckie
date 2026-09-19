//! The link cable over real sessions: the request/accept/start handshake, frames reaching the
//! sinks on both ends, the host's roster (busy, console mismatch, departures), and misbehaving
//! peers.

mod common;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use supershuckie_play_together::protocol::Message;
use supershuckie_play_together::*;
use supershuckie_replay_recorder::replay_file::{ReplayConsoleType, REPLAY_VERSION};
use supershuckie_replay_recorder::{append_packet, Packet, Speed, TimestampMillis};

// ---------------------------------------------------------------------------------------------
// Helpers

#[derive(Default, Debug)]
struct LinkRecorded {
    frames: Vec<(u64, u64, Vec<Packet>, Option<(u64, Blake3Hash)>)>,
    ended: Vec<LeaveReason>,
}

type SharedLink = Arc<Mutex<LinkRecorded>>;

struct LinkRec(SharedLink);

impl LinkSink for LinkRec {
    fn frame(&mut self, frame: u64, elapsed_millis: u64, events: Vec<Packet>, pair_hash: Option<(u64, Blake3Hash)>) {
        self.0.lock().unwrap().frames.push((frame, elapsed_millis, events, pair_hash));
    }
    fn ended(&mut self, reason: LeaveReason) {
        self.0.lock().unwrap().ended.push(reason);
    }
}

fn link_sink() -> (SharedLink, Box<dyn LinkSink>) {
    let shared = Arc::new(Mutex::new(LinkRecorded::default()));
    (Arc::clone(&shared), Box::new(LinkRec(shared)))
}

fn gb(name: &str) -> LocalParticipant {
    local_with(name, publisher_info(ReplayConsoleType::GameBoyColor))
}

fn wait_link(session: &dyn Session, log: &mut Vec<SessionEvent>, what: &str, pred: impl Fn(&LinkEvent) -> bool) -> LinkEvent {
    match wait_for(session, log, what, |e| matches!(e, SessionEvent::Link(l) if pred(l))) {
        SessionEvent::Link(l) => l,
        other => panic!("expected a link event, got {other:?}"),
    }
}

fn input_change(value: u8) -> Packet {
    Packet::ChangeInput { data: [value].iter().copied().collect() }
}

const GBC: u32 = ReplayConsoleType::GameBoyColor as u32;

// ---------------------------------------------------------------------------------------------

#[test]
fn a_link_is_requested_accepted_started_framed_and_unplugged_through_the_host() {
    let _wd = watchdog(15, "a_link_is_requested_accepted_started_framed_and_unplugged_through_the_host");
    let host = HostSession::bind(host_config(), gb("Host")).expect("bind");
    let mut hl = Vec::new();
    wait_connected(&host, &mut hl);
    let a = connect_client_with(&host, gb("A"));
    let b = connect_client_with(&host, gb("B"));
    let (mut al, mut bl) = (Vec::new(), Vec::new());
    assert_eq!(wait_connected(&a, &mut al), 2);
    assert_eq!(wait_connected(&b, &mut bl), 3);
    wait_joined(&host, &mut hl);
    wait_joined(&host, &mut hl);
    wait_joined(&a, &mut al);

    // A asks B (client to client: relayed twice).
    a.send_link(LinkMessage::Request { target: 3, nonce: 11, console: GBC }).expect("request");
    assert_eq!(wait_link(&b, &mut bl, "Requested at B", |_| true), LinkEvent::Requested { from: 2, nonce: 11, console: GBC });
    b.send_link(LinkMessage::Accept { target: 2, nonce: 11 }).expect("accept");
    assert_eq!(wait_link(&a, &mut al, "Accepted at A", |l| matches!(l, LinkEvent::Accepted { .. })), LinkEvent::Accepted { from: 3, nonce: 11 });
    // Everyone hears the pair, the host included.
    for (session, log, who) in [(&host as &dyn Session, &mut hl, "host"), (&a, &mut al, "A"), (&b, &mut bl, "B")] {
        assert_eq!(wait_link(session, log, &format!("PeerLinked at {who}"), |l| matches!(l, LinkEvent::PeerLinked { .. })), LinkEvent::PeerLinked { a: 2, b: 3 });
    }

    // Both hold and tell each other where.
    let input_a: supershuckie_replay_recorder::InputBuffer = [0x10u8, 0x20].iter().copied().collect();
    a.send_link(LinkMessage::Start { target: 3, nonce: 11, frame: 1000, input: input_a.clone(), rtt_millis: 20, delay_setting: 0, speed: Speed::default() }).expect("start");
    b.send_link(LinkMessage::Start { target: 2, nonce: 11, frame: 990, input: Default::default(), rtt_millis: 35, delay_setting: 4, speed: Speed::default() }).expect("start");
    assert_eq!(
        wait_link(&b, &mut bl, "Started at B", |l| matches!(l, LinkEvent::Started { .. })),
        LinkEvent::Started { from: 2, nonce: 11, frame: 1000, input: input_a, rtt_millis: 20, delay_setting: 0, speed: Speed::default() }
    );
    assert_eq!(
        wait_link(&a, &mut al, "Started at A", |l| matches!(l, LinkEvent::Started { .. })),
        LinkEvent::Started { from: 3, nonce: 11, frame: 990, input: Default::default(), rtt_millis: 35, delay_setting: 4, speed: Speed::default() }
    );

    // Frames go straight to the sinks, in order, with their pair hashes.
    let (rec_a, sink_a) = link_sink();
    let (rec_b, sink_b) = link_sink();
    a.set_link_sink(3, Some(sink_a));
    b.set_link_sink(2, Some(sink_b));
    for frame in 0..10u64 {
        let events = if frame % 3 == 0 { vec![input_change(frame as u8)] } else { Vec::new() };
        let pair_hash = if frame == 5 { Some((5, [0xAB; 32])) } else { None };
        a.send_link(LinkMessage::Frame { target: 3, frame, elapsed_millis: 5000, events, pair_hash }).expect("frame");
    }
    for frame in 0..4u64 {
        b.send_link(LinkMessage::Frame { target: 2, frame, elapsed_millis: 5000, events: vec![Packet::WriteMemory { address: 0xC000, data: [1u8, 2, 3].iter().copied().collect() }, Packet::ResetConsole], pair_hash: None })
            .expect("frame");
    }
    wait_until("B has A's 10 frames", || rec_b.lock().unwrap().frames.len() == 10);
    wait_until("A has B's 4 frames", || rec_a.lock().unwrap().frames.len() == 4);
    {
        let rec = rec_b.lock().unwrap();
        for (i, (frame, elapsed_millis, events, pair_hash)) in rec.frames.iter().enumerate() {
            assert_eq!(*elapsed_millis, 5000);
            assert_eq!(*frame, i as u64);
            if i % 3 == 0 {
                assert_eq!(events, &vec![input_change(i as u8)]);
            } else {
                assert!(events.is_empty());
            }
            assert_eq!(*pair_hash, if i == 5 { Some((5, [0xAB; 32])) } else { None });
        }
        assert!(rec.ended.is_empty());
        let rec = rec_a.lock().unwrap();
        assert_eq!(rec.frames[3].2, vec![Packet::WriteMemory { address: 0xC000, data: [1u8, 2, 3].iter().copied().collect() }, Packet::ResetConsole]);
    }
    wait_until("counters", || b.stats().link_frames_received == 10 && a.stats().link_frames_received == 4);
    assert!(a.stats().link_frames_sent >= 10);
    assert!(host.stats().link_frames_sent >= 14, "the host relays both directions: {:?}", host.stats());
    assert_eq!(host.stats().link_frames_received, 0);

    // A unplugs: B's sink ends and everyone hears it; A drops its own sink itself.
    a.send_link(LinkMessage::Unlink { target: 3, reason: UnlinkReason::Unplugged }).expect("unlink");
    a.set_link_sink(3, None);
    assert_eq!(wait_link(&b, &mut bl, "Unlinked at B", |l| matches!(l, LinkEvent::Unlinked { .. })), LinkEvent::Unlinked { from: 2, reason: UnlinkReason::Unplugged });
    assert_eq!(rec_b.lock().unwrap().ended, vec![LeaveReason::Left]);
    assert!(rec_a.lock().unwrap().ended.is_empty(), "a removed sink is dropped silently");
    for (session, log, who) in [(&host as &dyn Session, &mut hl, "host"), (&a, &mut al, "A"), (&b, &mut bl, "B")] {
        assert_eq!(wait_link(session, log, &format!("PeerUnlinked at {who}"), |l| matches!(l, LinkEvent::PeerUnlinked { .. })), LinkEvent::PeerUnlinked { a: 2, b: 3 });
    }
    // A late frame from B is dropped: the pair is no longer linked at the host.
    b.send_link(LinkMessage::Frame { target: 2, frame: 4, elapsed_millis: 5000, events: Vec::new(), pair_hash: None }).expect("frame");

    // B asks the host, which is one end of the link itself.
    b.send_link(LinkMessage::Request { target: 1, nonce: 5, console: GBC }).expect("request");
    assert_eq!(wait_link(&host, &mut hl, "Requested at host", |l| matches!(l, LinkEvent::Requested { .. })), LinkEvent::Requested { from: 3, nonce: 5, console: GBC });
    host.send_link(LinkMessage::Accept { target: 3, nonce: 5 }).expect("accept");
    assert_eq!(wait_link(&b, &mut bl, "Accepted at B", |l| matches!(l, LinkEvent::Accepted { .. })), LinkEvent::Accepted { from: 1, nonce: 5 });
    assert_eq!(wait_link(&a, &mut al, "PeerLinked at A", |l| matches!(l, LinkEvent::PeerLinked { .. })), LinkEvent::PeerLinked { a: 1, b: 3 });
    assert_eq!(wait_link(&host, &mut hl, "PeerLinked at host", |l| matches!(l, LinkEvent::PeerLinked { .. })), LinkEvent::PeerLinked { a: 1, b: 3 });
    host.send_link(LinkMessage::Start { target: 3, nonce: 5, frame: 7, input: Default::default(), rtt_millis: 0, delay_setting: 2, speed: Speed::default() }).expect("start");
    b.send_link(LinkMessage::Start { target: 1, nonce: 5, frame: 9, input: Default::default(), rtt_millis: 40, delay_setting: 0, speed: Speed::default() }).expect("start");
    assert!(matches!(wait_link(&b, &mut bl, "Started at B", |l| matches!(l, LinkEvent::Started { .. })), LinkEvent::Started { from: 1, frame: 7, delay_setting: 2, .. }));
    assert!(matches!(wait_link(&host, &mut hl, "Started at host", |l| matches!(l, LinkEvent::Started { .. })), LinkEvent::Started { from: 3, frame: 9, rtt_millis: 40, .. }));
    let (rec_h, sink_h) = link_sink();
    let (rec_b2, sink_b2) = link_sink();
    host.set_link_sink(3, Some(sink_h));
    b.set_link_sink(1, Some(sink_b2));
    host.send_link(LinkMessage::Frame { target: 3, frame: 0, elapsed_millis: 5000, events: vec![input_change(9)], pair_hash: None }).expect("frame");
    b.send_link(LinkMessage::Frame { target: 1, frame: 0, elapsed_millis: 5000, events: vec![input_change(8)], pair_hash: Some((0, [1; 32])) }).expect("frame");
    wait_until("both ends have a frame", || rec_h.lock().unwrap().frames.len() == 1 && rec_b2.lock().unwrap().frames.len() == 1);
    assert_eq!(rec_h.lock().unwrap().frames[0], (0, 5000, vec![input_change(8)], Some((0, [1; 32]))));
    assert_eq!(rec_b2.lock().unwrap().frames[0], (0, 5000, vec![input_change(9)], None));
    assert_eq!(host.stats().link_frames_received, 1);

    // B leaves: the host is told its partner left, its sink ends, and A hears the roster change.
    b.leave();
    assert_eq!(wait_link(&host, &mut hl, "Unlinked at host", |l| matches!(l, LinkEvent::Unlinked { .. })), LinkEvent::Unlinked { from: 3, reason: UnlinkReason::PeerLeft });
    assert!(!rec_h.lock().unwrap().ended.is_empty());
    assert_eq!(wait_link(&a, &mut al, "PeerUnlinked at A", |l| matches!(l, LinkEvent::PeerUnlinked { .. })), LinkEvent::PeerUnlinked { a: 1, b: 3 });
    assert_eq!(rec_b2.lock().unwrap().ended, vec![LeaveReason::Left], "our own leave ends our sink");
}

#[test]
fn the_host_declines_for_busy_or_mismatched_targets_and_answers_only_real_requests() {
    let _wd = watchdog(15, "the_host_declines_for_busy_or_mismatched_targets_and_answers_only_real_requests");
    let host = bind_host("Host"); // Game Boy Advance
    let mut hl = Vec::new();
    wait_connected(&host, &mut hl);
    let a = connect_client_with(&host, gb("A"));
    let b = connect_client_with(&host, gb("B"));
    let c = connect_client(&host, "C"); // Game Boy Advance
    let (mut al, mut bl, mut cl) = (Vec::new(), Vec::new(), Vec::new());
    assert_eq!(wait_connected(&a, &mut al), 2);
    assert_eq!(wait_connected(&b, &mut bl), 3);
    assert_eq!(wait_connected(&c, &mut cl), 4);
    for _ in 0..3 {
        wait_joined(&host, &mut hl);
    }
    wait_joined(&a, &mut al);
    wait_joined(&a, &mut al);
    wait_joined(&b, &mut bl);

    // Different families: declined on the target's behalf; the target never hears of it.
    a.send_link(LinkMessage::Request { target: 4, nonce: 1, console: GBC }).expect("request");
    assert_eq!(
        wait_link(&a, &mut al, "Declined (console)", |l| matches!(l, LinkEvent::Declined { .. })),
        LinkEvent::Declined { from: 4, nonce: 1, reason: LinkDeclineReason::ConsoleMismatch }
    );
    assert_no_event(&c, &mut cl, Duration::from_millis(200), "request at C", |e| matches!(e, SessionEvent::Link(_)));
    // A Game Boy asking the Game Boy Advance host: the same.
    a.send_link(LinkMessage::Request { target: 1, nonce: 2, console: GBC }).expect("request");
    assert_eq!(
        wait_link(&a, &mut al, "Declined (host console)", |l| matches!(l, LinkEvent::Declined { .. })),
        LinkEvent::Declined { from: 1, nonce: 2, reason: LinkDeclineReason::ConsoleMismatch }
    );
    // Nobody there.
    a.send_link(LinkMessage::Request { target: 9, nonce: 3, console: GBC }).expect_err("no such peer is refused locally");
    // Ourselves.
    assert!(a.send_link(LinkMessage::Request { target: 2, nonce: 3, console: GBC }).is_err());

    // An accept nobody asked for does nothing.
    b.send_link(LinkMessage::Accept { target: 2, nonce: 77 }).expect("accept");
    assert_no_event(&a, &mut al, Duration::from_millis(200), "phantom accept", |e| matches!(e, SessionEvent::Link(LinkEvent::Accepted { .. })));

    // A real request, declined by the player.
    a.send_link(LinkMessage::Request { target: 3, nonce: 4, console: GBC }).expect("request");
    assert_eq!(wait_link(&b, &mut bl, "Requested at B", |_| true), LinkEvent::Requested { from: 2, nonce: 4, console: GBC });
    b.send_link(LinkMessage::Decline { target: 2, nonce: 4, reason: LinkDeclineReason::Declined }).expect("decline");
    assert_eq!(wait_link(&a, &mut al, "Declined at A", |l| matches!(l, LinkEvent::Declined { .. })), LinkEvent::Declined { from: 3, nonce: 4, reason: LinkDeclineReason::Declined });
    // Accepting it afterwards is too late: the request is gone.
    b.send_link(LinkMessage::Accept { target: 2, nonce: 4 }).expect("accept");
    assert_no_event(&a, &mut al, Duration::from_millis(200), "late accept", |e| matches!(e, SessionEvent::Link(LinkEvent::Accepted { .. })));

    // Linked, then busy for everyone else.
    a.send_link(LinkMessage::Request { target: 3, nonce: 5, console: GBC }).expect("request");
    wait_link(&b, &mut bl, "Requested at B", |_| true);
    b.send_link(LinkMessage::Accept { target: 2, nonce: 5 }).expect("accept");
    assert_eq!(wait_link(&a, &mut al, "Accepted at A", |l| matches!(l, LinkEvent::Accepted { .. })), LinkEvent::Accepted { from: 3, nonce: 5 });
    wait_link(&c, &mut cl, "PeerLinked at C", |l| matches!(l, LinkEvent::PeerLinked { .. }));
    b.send_link(LinkMessage::Request { target: 2, nonce: 6, console: GBC }).expect("request");
    assert_eq!(wait_link(&b, &mut bl, "Declined (busy)", |l| matches!(l, LinkEvent::Declined { .. })), LinkEvent::Declined { from: 2, nonce: 6, reason: LinkDeclineReason::Busy });
    assert_no_event(&a, &mut al, Duration::from_millis(200), "request while linked", |e| matches!(e, SessionEvent::Link(LinkEvent::Requested { .. })));

    // Frames between the two flow; a frame to a third party does not.
    let (rec_c, sink_c) = link_sink();
    c.set_link_sink(2, Some(sink_c));
    a.send_link(LinkMessage::Frame { target: 4, frame: 0, elapsed_millis: 5000, events: Vec::new(), pair_hash: None }).expect("frame");
    let (rec_b, sink_b) = link_sink();
    b.set_link_sink(2, Some(sink_b));
    a.send_link(LinkMessage::Frame { target: 3, frame: 0, elapsed_millis: 5000, events: Vec::new(), pair_hash: None }).expect("frame");
    wait_until("B has the frame", || rec_b.lock().unwrap().frames.len() == 1);
    assert!(rec_c.lock().unwrap().frames.is_empty());

    // A leaves: B is unplugged by the host and C sees the roster change.
    a.leave();
    assert_eq!(wait_link(&b, &mut bl, "Unlinked at B", |l| matches!(l, LinkEvent::Unlinked { .. })), LinkEvent::Unlinked { from: 2, reason: UnlinkReason::PeerLeft });
    assert_eq!(rec_b.lock().unwrap().ended, vec![LeaveReason::Left]);
    assert_eq!(wait_link(&c, &mut cl, "PeerUnlinked at C", |l| matches!(l, LinkEvent::PeerUnlinked { .. })), LinkEvent::PeerUnlinked { a: 2, b: 3 });
    // B is free again.
    c.send_link(LinkMessage::Request { target: 3, nonce: 7, console: ReplayConsoleType::GameBoyAdvance as u32 }).expect("request");
    assert_eq!(wait_link(&c, &mut cl, "Declined (console)", |l| matches!(l, LinkEvent::Declined { .. })), LinkEvent::Declined { from: 3, nonce: 7, reason: LinkDeclineReason::ConsoleMismatch });

    // The host leaving ends every sink.
    let (rec_h, sink_h) = link_sink();
    host.set_link_sink(3, Some(sink_h));
    host.leave();
    assert_eq!(rec_h.lock().unwrap().ended, vec![LeaveReason::Left]);
    wait_disconnected(&b, &mut bl);
}

// ---------------------------------------------------------------------------------------------
// Misbehaving peers

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
    fn handshake(addr: SocketAddr, name: &str) -> (Raw, PeerId) {
        let mut raw = Raw::connect(addr);
        raw.send(&Message::Hello {
            protocol_version: PROTOCOL_VERSION,
            replay_version: REPLAY_VERSION,
            app_version: "raw".to_owned(),
            display_name: name.to_owned(),
            color: 0,
            publisher: publisher_info(ReplayConsoleType::GameBoyColor),
        });
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
fn spoofed_unlinked_and_forbidden_link_messages_are_handled() {
    let _wd = watchdog(15, "spoofed_unlinked_and_forbidden_link_messages_are_handled");
    let host = HostSession::bind(host_config(), gb("Host")).expect("bind");
    let mut hl = Vec::new();
    wait_connected(&host, &mut hl);
    let victim = connect_client_with(&host, gb("Victim"));
    let mut vl = Vec::new();
    assert_eq!(wait_connected(&victim, &mut vl), 2);
    let (mut liar, liar_id) = Raw::handshake(host.local_addr(), "Liar");
    assert_eq!(liar_id, 3);
    wait_joined(&host, &mut hl);
    wait_joined(&host, &mut hl);
    wait_joined(&victim, &mut vl);

    // A frame to someone we are not linked with is dropped, whatever `from` claims.
    let (rec, sink) = link_sink();
    victim.set_link_sink(3, Some(sink));
    liar.send(&Message::LinkFrame { from: 1, target: 2, frame: 0, elapsed_millis: 7, events: Vec::new(), pair_hash_frame: 0, pair_hash: [0; 32] });
    liar.send(&Message::LinkStart { from: 1, target: 2, nonce: 1, frame: 0, input: Default::default(), rtt_millis: 0, delay_setting: 0, speed: Speed::default() });
    assert_no_event(&victim, &mut vl, Duration::from_millis(200), "start from a stranger", |e| matches!(e, SessionEvent::Link(_)));
    assert!(rec.lock().unwrap().frames.is_empty());

    // A request with a spoofed `from` reaches the target with the real sender; the answer
    // comes back the same way.
    liar.send(&Message::LinkRequest { from: 1, target: 2, nonce: 9, console: GBC });
    assert_eq!(wait_link(&victim, &mut vl, "Requested", |_| true), LinkEvent::Requested { from: 3, nonce: 9, console: GBC });
    victim.send_link(LinkMessage::Accept { target: 3, nonce: 9 }).expect("accept");
    match liar.read_until(|m| matches!(m, Message::LinkAccept { .. })) {
        Some(Message::LinkAccept { from, target, nonce }) => assert_eq!((from, target, nonce), (2, 3, 9)),
        other => panic!("{other:?}"),
    }
    match liar.read_until(|m| matches!(m, Message::PeerLinked { .. })) {
        Some(Message::PeerLinked { a, b }) => assert_eq!((a, b), (2, 3)),
        other => panic!("{other:?}"),
    }
    assert_eq!(wait_link(&victim, &mut vl, "PeerLinked", |l| matches!(l, LinkEvent::PeerLinked { .. })), LinkEvent::PeerLinked { a: 2, b: 3 });
    // Now linked: a spoofed frame arrives with the real sender.
    liar.send(&Message::LinkFrame { from: 1, target: 2, frame: 0, elapsed_millis: 7, events: encode_link_events(&[input_change(1)]), pair_hash_frame: 0, pair_hash: [0; 32] });
    wait_until("the frame", || rec.lock().unwrap().frames.len() == 1);
    assert_eq!(rec.lock().unwrap().frames[0], (0, 7, vec![input_change(1)], None));
    // A frame to the wrong target is dropped.
    liar.send(&Message::LinkFrame { from: 3, target: 1, frame: 1, elapsed_millis: 7, events: Vec::new(), pair_hash_frame: 0, pair_hash: [0; 32] });
    assert_no_event(&host, &mut hl, Duration::from_millis(200), "frame at the host", |e| matches!(e, SessionEvent::Warning(_)));

    // A frame carrying a packet a link never sends ends the liar; the victim is unplugged.
    let mut events = Vec::new();
    append_packet(&Packet::NextFrame { timestamp_delta: TimestampMillis(16) }, &mut events);
    liar.send(&Message::LinkFrame { from: 3, target: 2, frame: 1, elapsed_millis: 7, events, pair_hash_frame: 0, pair_hash: [0; 32] });
    match liar.read_until(|m| matches!(m, Message::Error { .. })) {
        Some(Message::Error { text }) => assert!(text.contains("NextFrame"), "{text}"),
        other => panic!("{other:?}"),
    }
    assert!(liar.eof());
    assert_eq!(wait_link(&victim, &mut vl, "Unlinked", |l| matches!(l, LinkEvent::Unlinked { .. })), LinkEvent::Unlinked { from: 3, reason: UnlinkReason::PeerLeft });
    // The host's `Unlink` travels on the urgent lane ahead of the `PeerLeft`, so the sink
    // ends through it (once).
    assert_eq!(rec.lock().unwrap().ended, vec![LeaveReason::Left]);
    wait_link(&victim, &mut vl, "PeerUnlinked", |l| matches!(l, LinkEvent::PeerUnlinked { .. }));

    // Only the host announces pairs.
    let (mut liar2, _) = Raw::handshake(host.local_addr(), "Liar2");
    liar2.send(&Message::PeerLinked { a: 1, b: 2 });
    match liar2.read_until(|m| matches!(m, Message::Error { .. })) {
        Some(Message::Error { text }) => assert!(text.contains("only the host"), "{text}"),
        other => panic!("{other:?}"),
    }
    assert!(liar2.eof());
    assert_no_event(&victim, &mut vl, Duration::from_millis(200), "a fake PeerLinked", |e| matches!(e, SessionEvent::Link(LinkEvent::PeerLinked { .. })));
}
