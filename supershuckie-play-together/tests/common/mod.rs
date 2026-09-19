//! Shared helpers for the integration tests: sample participants, a recording sink, event
//! waiting, a per-test watchdog, and packet-stream builders.

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use supershuckie_play_together::*;
use supershuckie_replay_recorder::replay_file::{ReplayConsoleType, ReplayFileMetadata, ReplayPatchFormat};
use supershuckie_replay_recorder::{append_packet, InputBuffer, Packet, Speed, TimestampMillis};

/// Sample metadata for a Game Boy Advance session.
pub fn metadata(console: ReplayConsoleType) -> ReplayFileMetadata {
    ReplayFileMetadata {
        console_type: console,
        rom_name: "POKEMON EMER".to_owned(),
        rom_filename: "emerald.gba".to_owned(),
        rom_checksum: [0x11; 32],
        bios_checksum: [0x22; 32],
        emulator_core_name: "mGBA 0.10.5".to_owned(),
        patch_format: ReplayPatchFormat::Unpatched,
        patch_target_checksum: [0; 32],
        crop_start: None,
        crop_end: None,
        timer_offset: None,
    }
}

pub fn publisher_info(console: ReplayConsoleType) -> PublisherInfo {
    PublisherInfo { metadata: metadata(console), initial_input: InputBuffer::new(), speed: Speed::default(), frame: 0 }
}

pub fn local(name: &str) -> LocalParticipant {
    LocalParticipant { display_name: name.to_owned(), color: 0, app_version: "test 0.4.14".to_owned(), publisher: publisher_info(ReplayConsoleType::GameBoyAdvance) }
}

pub fn local_with(name: &str, publisher: PublisherInfo) -> LocalParticipant {
    LocalParticipant { display_name: name.to_owned(), color: 0, app_version: "test 0.4.14".to_owned(), publisher }
}

/// A host config on a free loopback port with short timeouts.
pub fn host_config() -> HostConfig {
    HostConfig {
        bind_address: "127.0.0.1".to_owned(),
        port: 0,
        max_participants: MAX_PARTICIPANTS,
        allow_nintendo_ds: false,
        handshake_timeout: Duration::from_millis(500),
        idle_timeout: Duration::from_secs(3),
    }
}

pub fn client_config() -> ClientConfig {
    ClientConfig { connect_timeout: Duration::from_secs(2), handshake_timeout: Duration::from_secs(2), idle_timeout: Duration::from_secs(3) }
}

pub fn code_for(host: &HostSession) -> JoinCode {
    let addr = host.local_addr();
    JoinCode { host: addr.ip().to_string(), port: addr.port() }
}

pub fn bind_host(name: &str) -> HostSession {
    HostSession::bind(host_config(), local(name)).expect("bind")
}

/// Connect a client and wait for the handshake to settle (either way), so that clients connected
/// one after another get their peer ids in that order.
pub fn connect_client(host: &HostSession, name: &str) -> ClientSession {
    connect_client_with(host, local(name))
}

/// [`connect_client`] with a custom local participant.
pub fn connect_client_with(host: &HostSession, local: LocalParticipant) -> ClientSession {
    let client = ClientSession::connect(code_for(host), client_config(), local);
    let deadline = Instant::now() + WAIT;
    // A refused or failed connect never becomes connected; the caller's wait sees why.
    while !client.is_connected() && !client.has_disconnected() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    client
}

// ---------------------------------------------------------------------------------------------
// Recording sink

#[derive(Default, Debug)]
pub struct Recorded {
    pub packets: Vec<(u64, Vec<Packet>)>,
    pub packet_bytes: u64,
    pub frames: u64,
    pub snapshots: Vec<SnapshotData>,
    pub hashes: Vec<(u64, Blake3Hash)>,
    pub ended: Vec<LeaveReason>,
    /// "packets", "snapshot", "hash", "ended" in call order.
    pub order: Vec<String>,
    /// Keep the packets themselves (off for the big streams).
    pub store_packets: bool,
}

pub type SharedRecorded = Arc<Mutex<Recorded>>;

pub struct RecSink(pub SharedRecorded);

impl FollowerSink for RecSink {
    fn packets(&mut self, first_frame: u64, packets: Vec<Packet>) {
        let mut r = self.0.lock().unwrap();
        r.frames += count_frames(&packets);
        r.packet_bytes += packets
            .iter()
            .map(|p| match p {
                Packet::LoadSaveState { state } => state.len() as u64,
                _ => 1,
            })
            .sum::<u64>();
        r.order.push("packets".to_owned());
        if r.store_packets {
            r.packets.push((first_frame, packets));
        } else {
            r.packets.push((first_frame, Vec::new()));
        }
    }
    fn snapshot(&mut self, snapshot: SnapshotData) {
        let mut r = self.0.lock().unwrap();
        r.order.push("snapshot".to_owned());
        r.snapshots.push(snapshot);
    }
    fn sync_hash(&mut self, frame: u64, hash: Blake3Hash) {
        let mut r = self.0.lock().unwrap();
        r.order.push("hash".to_owned());
        r.hashes.push((frame, hash));
    }
    fn ended(&mut self, reason: LeaveReason) {
        let mut r = self.0.lock().unwrap();
        r.order.push("ended".to_owned());
        r.ended.push(reason);
    }
}

pub fn sink() -> (SharedRecorded, Box<dyn FollowerSink>) {
    let shared = Arc::new(Mutex::new(Recorded { store_packets: true, ..Recorded::default() }));
    (Arc::clone(&shared), Box::new(RecSink(shared)))
}

pub fn counting_sink() -> (SharedRecorded, Box<dyn FollowerSink>) {
    let shared = Arc::new(Mutex::new(Recorded::default()));
    (Arc::clone(&shared), Box::new(RecSink(shared)))
}

// ---------------------------------------------------------------------------------------------
// Waiting

pub const WAIT: Duration = Duration::from_secs(8);

/// Poll `session` until an event satisfies `pred`. `log` holds the events seen and not yet
/// waited for: a matching event is taken out of it, the rest stay for later waits (several
/// events often arrive in one poll).
pub fn wait_for(session: &dyn Session, log: &mut Vec<SessionEvent>, what: &str, pred: impl Fn(&SessionEvent) -> bool) -> SessionEvent {
    let deadline = Instant::now() + WAIT;
    loop {
        log.extend(session.poll_events());
        if let Some(index) = log.iter().position(|e| pred(e)) {
            return log.remove(index);
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}; events so far: {log:#?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Poll `session` for `for_` and assert no event satisfies `pred`.
pub fn assert_no_event(session: &dyn Session, log: &mut Vec<SessionEvent>, for_: Duration, what: &str, pred: impl Fn(&SessionEvent) -> bool) {
    let deadline = Instant::now() + for_;
    for event in log.iter() {
        assert!(!pred(event), "unexpected {what}: {event:?}");
    }
    while Instant::now() < deadline {
        for event in session.poll_events() {
            assert!(!pred(&event), "unexpected {what}: {event:?}");
            log.push(event);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Wait until `cond` holds.
pub fn wait_until(what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

pub fn wait_connected(session: &dyn Session, log: &mut Vec<SessionEvent>) -> PeerId {
    match wait_for(session, log, "Connected", |e| matches!(e, SessionEvent::Connected { .. } | SessionEvent::Disconnected { .. })) {
        SessionEvent::Connected { local_peer_id, .. } => local_peer_id,
        other => panic!("expected Connected, got {other:?}"),
    }
}

pub fn wait_joined(session: &dyn Session, log: &mut Vec<SessionEvent>) -> ParticipantInfo {
    match wait_for(session, log, "Joined", |e| matches!(e, SessionEvent::Joined(_))) {
        SessionEvent::Joined(p) => p,
        other => panic!("expected Joined, got {other:?}"),
    }
}

pub fn wait_snapshot_requested(session: &dyn Session, log: &mut Vec<SessionEvent>) -> Vec<PeerId> {
    match wait_for(session, log, "SnapshotRequested", |e| matches!(e, SessionEvent::SnapshotRequested { .. })) {
        SessionEvent::SnapshotRequested { requesters } => requesters,
        other => panic!("expected SnapshotRequested, got {other:?}"),
    }
}

pub fn wait_disconnected(session: &dyn Session, log: &mut Vec<SessionEvent>) -> DisconnectReason {
    match wait_for(session, log, "Disconnected", |e| matches!(e, SessionEvent::Disconnected { .. })) {
        SessionEvent::Disconnected { reason } => reason,
        other => panic!("expected Disconnected, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------------
// Watchdog

/// Aborts the test binary if a test runs longer than its budget (a hang would otherwise stall
/// the whole suite).
pub struct Watchdog(Arc<AtomicBool>);

pub fn watchdog(secs: u64, name: &'static str) -> Watchdog {
    let done = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&done);
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
            if flag.load(Ordering::Acquire) {
                return;
            }
        }
        eprintln!("watchdog: test {name} exceeded {secs} s; aborting");
        std::process::abort();
    });
    Watchdog(done)
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------------------------
// Packet streams

/// `n` frames of input changes and `NextFrame`s, as `Stream.bytes`.
pub fn frames(n: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..n {
        append_packet(&Packet::ChangeInput { data: [i as u8].iter().copied().collect() }, &mut out);
        append_packet(&Packet::NextFrame { timestamp_delta: TimestampMillis(16) }, &mut out);
    }
    out
}

/// One frame carrying a `LoadSaveState` of `state_len` bytes (a big packet).
pub fn big_frame(state_len: usize) -> Vec<u8> {
    let mut out = Vec::new();
    append_packet(&Packet::LoadSaveState { state: vec![0xAB; state_len].into_iter().collect() }, &mut out);
    append_packet(&Packet::NextFrame { timestamp_delta: TimestampMillis(16) }, &mut out);
    out
}

pub fn snapshot_at(frame: u64) -> SnapshotData {
    SnapshotData {
        frame,
        elapsed_millis: frame * 16,
        input: [1u8, 2].iter().copied().collect(),
        speed: Speed::default(),
        counters: vec![("resets".to_owned(), 3)],
        state: (0..4096u32).map(|i| (i % 251) as u8).collect(),
    }
}

/// Publish `n` frames one call per frame starting at `first`.
pub fn publish_frames(publisher: &PublisherHandle, first: u64, n: u64) {
    for i in 0..n {
        publisher.publish(first + i, frames(1)).expect("publish");
    }
}
