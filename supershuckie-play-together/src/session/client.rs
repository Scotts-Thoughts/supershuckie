//! The client: connects to a host, says `Hello`, then reads what the host relays.
//!
//! Threads: `pt-client-connect` (DNS, TCP connect per resolved address, spawns the others),
//! `pt-client-writer` and `pt-client-reader`.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use supershuckie_replay_recorder::replay_file::REPLAY_VERSION;

use crate::code::JoinCode;
use crate::compat::sanitize_display_name;
use crate::conn::{writer_loop, ConnShared, Outbound, PushError, Stats};
use crate::error::{DisconnectReason, Phase, PlayTogetherError, PublishError};
use crate::protocol::{decode_packets, read_frame, LeaveReason, Message, ParticipantInfo, ReadError, PROTOCOL_VERSION};
use crate::session::follow::{FollowSlot, StreamOutcome};
use crate::session::host::HOST_PEER_ID;
use crate::session::{
    base_stats, join_bounded, leave_reason_for, ClientConfig, Coalescer, EventQueue, FollowerSink, PublishBackend, PublisherHandle, Role,
    Session, SessionEvent, SessionStats, GOODBYE_GRACE, LEAVE_TIMEOUT, POLL_INTERVAL,
};
use crate::transport::{resolve, Connection, TcpTransport, Transport};
use crate::{LocalParticipant, PeerId, SessionId};

/// A session joined as a client.
pub struct ClientSession {
    shared: Arc<ClientShared>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

struct ClientShared {
    config: ClientConfig,
    local: LocalParticipant,
    code: JoinCode,
    local_id: Arc<AtomicU16>,
    session_id: AtomicU64,
    connected: AtomicBool,
    running: AtomicBool,
    inner: Mutex<ClientInner>,
    events: EventQueue,
    stats: Arc<Stats>,
    coalescer: Mutex<Coalescer>,
    disconnected: Mutex<Option<DisconnectReason>>,
}

#[derive(Default)]
struct ClientInner {
    participants: BTreeMap<PeerId, ParticipantInfo>,
    follows: HashMap<PeerId, Arc<FollowSlot>>,
    conn: Option<Arc<ConnShared>>,
    threads: Vec<JoinHandle<()>>,
}

/// Why the reader stopped.
enum Stop {
    Closing,
    Timeout(Phase),
}

impl ClientSession {
    /// Connect with plain TCP. Returns at once; the outcome arrives as a `Connected` or
    /// `Disconnected` event.
    pub fn connect(code: JoinCode, config: ClientConfig, local: LocalParticipant) -> ClientSession {
        Self::connect_with(TcpTransport, code, config, local)
    }

    /// Connect through `transport`.
    /// Whether the session has ended (for any reason) rather than never having started; the
    /// reason arrives as a `Disconnected` event.
    pub fn has_disconnected(&self) -> bool {
        self.shared.disconnected_reason().is_some()
    }

    pub fn connect_with<T: Transport>(transport: T, code: JoinCode, config: ClientConfig, local: LocalParticipant) -> ClientSession {
        let shared = Arc::new(ClientShared {
            config,
            local: LocalParticipant { display_name: sanitize_display_name(&local.display_name), ..local },
            code,
            local_id: Arc::new(AtomicU16::new(0)),
            session_id: AtomicU64::new(0),
            connected: AtomicBool::new(false),
            running: AtomicBool::new(true),
            inner: Mutex::new(ClientInner::default()),
            events: EventQueue::default(),
            stats: Arc::new(Stats::default()),
            coalescer: Mutex::new(Coalescer::default()),
            disconnected: Mutex::new(None),
        });
        let connect = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new().name("pt-client-connect".to_owned()).spawn(move || connect_thread(shared, transport))
        };
        let threads = match connect {
            Ok(handle) => vec![handle],
            Err(e) => {
                shared.disconnect(DisconnectReason::ConnectFailed(format!("could not start the connect thread: {e}")));
                Vec::new()
            }
        };
        ClientSession { shared, threads: Mutex::new(threads) }
    }
}

impl Drop for ClientSession {
    fn drop(&mut self) {
        self.leave();
    }
}

fn connect_thread<T: Transport>(shared: Arc<ClientShared>, transport: T) {
    let addresses = match resolve(&shared.code.host, shared.code.port) {
        Ok(addresses) if !addresses.is_empty() => addresses,
        Ok(_) => return shared.disconnect(DisconnectReason::ConnectFailed(format!("{} does not resolve to any address", shared.code.host))),
        Err(e) => return shared.disconnect(DisconnectReason::ConnectFailed(format!("could not resolve {}: {e}", shared.code.host))),
    };
    let mut last_error = None;
    let mut connection = None;
    for address in addresses {
        if !shared.running.load(Ordering::Acquire) {
            return;
        }
        match transport.connect(address, shared.config.connect_timeout) {
            Ok(c) => {
                connection = Some(c);
                break;
            }
            Err(e) => last_error = Some(e),
        }
    }
    let Some(connection) = connection else {
        let detail = last_error.map(|e| e.to_string()).unwrap_or_else(|| "no address to try".to_owned());
        return shared.disconnect(DisconnectReason::ConnectFailed(format!("{}: {detail}", shared.code.format())));
    };
    if !shared.running.load(Ordering::Acquire) {
        connection.shutdown();
        return;
    }

    let label = connection.peer_label();
    if let Err(e) = connection.set_timeouts(Some(POLL_INTERVAL), Some(shared.config.idle_timeout)) {
        connection.shutdown();
        return shared.disconnect(DisconnectReason::IoError(format!("could not configure the connection to {label}: {e}")));
    }
    let (read, write) = match connection.split() {
        Ok(halves) => halves,
        Err(e) => {
            connection.shutdown();
            return shared.disconnect(DisconnectReason::IoError(format!("could not set up the connection to {label}: {e}")));
        }
    };
    let connection = Arc::new(connection);
    let shutdown = {
        let connection = Arc::clone(&connection);
        Box::new(move || connection.shutdown()) as Box<dyn Fn() + Send + Sync>
    };
    let conn = Arc::new(ConnShared::new(label, Arc::clone(&shared.local_id), Arc::clone(&shared.stats), shutdown));
    conn.peer_id.store(HOST_PEER_ID, Ordering::Release);

    {
        let mut inner = shared.lock();
        if !shared.running.load(Ordering::Acquire) {
            drop(inner);
            connection.shutdown();
            return;
        }
        inner.conn = Some(Arc::clone(&conn));
    }

    let writer = {
        let shared = Arc::clone(&shared);
        let conn = Arc::clone(&conn);
        std::thread::Builder::new().name("pt-client-writer".to_owned()).spawn(move || {
            let warn = |text: String| shared.events.push(SessionEvent::Warning(text));
            if let Err(e) = writer_loop(write, &conn, &warn) {
                shared.disconnect(DisconnectReason::IoError(e.to_string()));
            }
        })
    };
    let hello = Message::Hello {
        protocol_version: PROTOCOL_VERSION,
        replay_version: REPLAY_VERSION,
        app_version: shared.local.app_version.clone(),
        display_name: shared.local.display_name.clone(),
        publisher: shared.local.publisher.clone(),
    };
    let _ = conn.send(&hello);
    let reader = {
        let shared = Arc::clone(&shared);
        let conn = Arc::clone(&conn);
        std::thread::Builder::new().name("pt-client-reader".to_owned()).spawn(move || reader_thread(shared, conn, read))
    };
    let mut inner = shared.lock();
    match (reader, writer) {
        (Ok(reader), Ok(writer)) => inner.threads.extend([reader, writer]),
        (reader, writer) => {
            inner.threads.extend(reader.into_iter().chain(writer));
            drop(inner);
            shared.disconnect(DisconnectReason::IoError("could not start the connection's threads".to_owned()));
        }
    }
}

impl ClientShared {
    fn lock(&self) -> std::sync::MutexGuard<'_, ClientInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn disconnected_reason(&self) -> Option<DisconnectReason> {
        self.disconnected.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// End the session for `reason`. Idempotent: the first reason wins.
    fn disconnect(&self, reason: DisconnectReason) {
        {
            let mut slot = self.disconnected.lock().unwrap_or_else(|e| e.into_inner());
            if slot.is_some() {
                return;
            }
            *slot = Some(reason.clone());
        }
        self.running.store(false, Ordering::Release);
        self.connected.store(false, Ordering::Release);
        let (follows, conn) = {
            let mut inner = self.lock();
            (std::mem::take(&mut inner.follows), inner.conn.clone())
        };
        let ended = leave_reason_for(&reason);
        for slot in follows.values() {
            slot.end(ended);
        }
        // A Goodbye or Error we queued should still get out; anything else is moot.
        let flush = matches!(reason, DisconnectReason::Left | DisconnectReason::ProtocolError(_));
        if let Some(conn) = conn {
            conn.begin_close(flush);
        }
        self.events.push(SessionEvent::Disconnected { reason });
    }

    fn protocol_error(&self, conn: &ConnShared, text: String) {
        let _ = conn.send(&Message::Error { text: text.clone() });
        self.disconnect(DisconnectReason::ProtocolError(text));
    }

    fn conn(&self) -> Option<Arc<ConnShared>> {
        self.lock().conn.clone()
    }

    fn follow_slot(&self, peer: PeerId) -> Option<Arc<FollowSlot>> {
        self.lock().follows.get(&peer).cloned()
    }

    fn flush_coalescer(&self) {
        let requesters = self.coalescer.lock().unwrap_or_else(|e| e.into_inner()).flush();
        if let Some(requesters) = requesters {
            self.events.push(SessionEvent::SnapshotRequested { requesters });
        }
    }

    fn request_snapshot_from(&self, publisher: PeerId, force: bool) {
        let Some(slot) = self.follow_slot(publisher) else { return };
        if !slot.may_request(force) {
            return;
        }
        if let Some(conn) = self.conn() {
            let _ = conn.send(&Message::RequestSnapshot { requester: 0, target: publisher });
        }
    }

    fn name_of(&self, peer: PeerId) -> String {
        self.lock().participants.get(&peer).map(|p| p.display_name.clone()).unwrap_or_else(|| format!("peer {peer}"))
    }

    fn on_welcome(&self, your_peer_id: PeerId, session_id: SessionId, your_display_name: String, participants: Vec<ParticipantInfo>) {
        {
            let mut inner = self.lock();
            for p in &participants {
                inner.follows.insert(p.peer_id, Arc::new(FollowSlot::new()));
                inner.participants.insert(p.peer_id, p.clone());
            }
        }
        self.local_id.store(your_peer_id, Ordering::Release);
        self.session_id.store(session_id, Ordering::Release);
        self.connected.store(true, Ordering::Release);
        self.events.push(SessionEvent::Connected { session_id, local_peer_id: your_peer_id, local_display_name: your_display_name, participants });
    }

    /// One message after admission.
    fn handle_message(&self, conn: &ConnShared, message: Message) {
        let local_id = self.local_id.load(Ordering::Acquire);
        match message {
            Message::Hello { .. } | Message::Welcome { .. } | Message::Refused { .. } => {
                self.protocol_error(conn, "unexpected handshake message after Welcome".to_owned())
            }
            Message::PeerJoined { participant } => {
                {
                    let mut inner = self.lock();
                    inner.follows.entry(participant.peer_id).or_insert_with(|| Arc::new(FollowSlot::new()));
                    inner.participants.insert(participant.peer_id, participant.clone());
                }
                self.events.push(SessionEvent::Joined(participant));
            }
            Message::PeerLeft { peer_id, reason } => {
                if peer_id == local_id {
                    let reason = match reason {
                        LeaveReason::Kicked => DisconnectReason::Kicked,
                        LeaveReason::HostLeft => DisconnectReason::HostLeft,
                        LeaveReason::Timeout => DisconnectReason::Timeout(Phase::Idle),
                        LeaveReason::TooSlow => DisconnectReason::TooSlow { queued_bytes: 0 },
                        LeaveReason::ProtocolError => DisconnectReason::ProtocolError("the host reported a protocol error".to_owned()),
                        LeaveReason::IoError => DisconnectReason::IoError("the host reported a connection error".to_owned()),
                        LeaveReason::Left => DisconnectReason::Left,
                    };
                    return self.disconnect(reason);
                }
                let slot = {
                    let mut inner = self.lock();
                    inner.participants.remove(&peer_id);
                    inner.follows.remove(&peer_id)
                };
                self.coalescer.lock().unwrap_or_else(|e| e.into_inner()).forget(peer_id);
                self.events.push(SessionEvent::Left { peer_id, reason });
                if let Some(slot) = slot {
                    slot.end(reason);
                }
            }
            Message::ResetAll { race_id, countdown_millis } => {
                self.events.push(SessionEvent::ResetAll { race_id, deadline: Instant::now() + Duration::from_millis(u64::from(countdown_millis)) });
            }
            Message::Stream { from, first_frame, bytes } => {
                let Some(slot) = self.follow_slot(from) else { return };
                if !slot.is_live() {
                    return;
                }
                let packets = match decode_packets(&bytes) {
                    Ok(packets) => packets,
                    Err(e) => return self.protocol_error(conn, e.to_string()),
                };
                if let StreamOutcome::Gap { expected, got } = slot.on_stream(first_frame, packets) {
                    self.events.push(SessionEvent::Warning(format!(
                        "lost part of {}'s stream (expected frame {expected}, got {got}); asking for a new snapshot",
                        self.name_of(from)
                    )));
                    self.request_snapshot_from(from, false);
                }
            }
            Message::Snapshot(wire) => {
                if wire.target != 0 && wire.target != local_id {
                    return;
                }
                let Some(slot) = self.follow_slot(wire.from) else { return };
                if !slot.has_sink() {
                    return;
                }
                match wire.into_snapshot() {
                    Ok(snapshot) => {
                        slot.on_snapshot(snapshot);
                    }
                    Err(e) => self.protocol_error(conn, e.to_string()),
                }
            }
            Message::SyncHash { from, frame, hash } => {
                if let Some(slot) = self.follow_slot(from) {
                    slot.on_sync_hash(frame, hash);
                }
            }
            Message::RequestSnapshot { requester, target } => {
                if target == local_id || target == 0 {
                    self.coalescer.lock().unwrap_or_else(|e| e.into_inner()).request(requester);
                }
            }
            Message::Ping { nonce, sent_unix_millis } => {
                let _ = conn.send(&Message::Pong { nonce, sent_unix_millis });
            }
            Message::Pong { nonce, .. } => {
                if let Some(rtt) = conn.on_pong(nonce) {
                    self.events.push(SessionEvent::RttUpdated { peer_id: HOST_PEER_ID, rtt });
                }
            }
            Message::Goodbye => self.disconnect(DisconnectReason::HostLeft),
            Message::Error { text } => self.disconnect(DisconnectReason::ProtocolError(format!("the host reported: {text}"))),
        }
    }
}

fn reader_thread<R: Read>(shared: Arc<ClientShared>, conn: Arc<ConnShared>, mut read: R) {
    let idle_timeout = shared.config.idle_timeout;
    let handshake_deadline = Instant::now() + shared.config.handshake_timeout;
    let mut last_rx = Instant::now();
    loop {
        let in_handshake = !shared.connected.load(Ordering::Acquire);
        let mut last_got = 0usize;
        let mut poll = |got: usize| -> Result<(), Stop> {
            if !shared.running.load(Ordering::Acquire) || conn.is_closing() {
                return Err(Stop::Closing);
            }
            let now = Instant::now();
            if got > last_got {
                last_got = got;
                last_rx = now;
            }
            if in_handshake && now > handshake_deadline {
                return Err(Stop::Timeout(Phase::Handshake));
            }
            if now.duration_since(last_rx) > idle_timeout {
                return Err(Stop::Timeout(Phase::Idle));
            }
            Ok(())
        };
        let frame = match read_frame(&mut read, &mut poll) {
            Ok(Some(frame)) => frame,
            Ok(None) => return shared.disconnect(DisconnectReason::IoError("the host closed the connection".to_owned())),
            Err(ReadError::Io(e)) => return shared.disconnect(DisconnectReason::IoError(e.to_string())),
            Err(ReadError::Decode(e)) => return shared.protocol_error(&conn, e.to_string()),
            Err(ReadError::Stopped(Stop::Closing)) => return,
            Err(ReadError::Stopped(Stop::Timeout(phase))) => return shared.disconnect(DisconnectReason::Timeout(phase)),
        };
        last_rx = Instant::now();
        shared.stats.add_in(frame.len());
        if !shared.running.load(Ordering::Acquire) {
            return;
        }
        let message = match Message::decode_frame(&frame) {
            Ok(message) => message,
            Err(crate::error::DecodeError::UnknownTag(_)) => {
                shared.stats.unknown_messages_skipped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            Err(e) => return shared.protocol_error(&conn, e.to_string()),
        };
        if in_handshake {
            match message {
                Message::Welcome { your_peer_id, session_id, your_display_name, participants } => {
                    shared.on_welcome(your_peer_id, session_id, your_display_name, participants);
                }
                Message::Refused { reason, text } => return shared.disconnect(DisconnectReason::Refused { reason, text }),
                Message::Error { text } => return shared.disconnect(DisconnectReason::ProtocolError(format!("the host reported: {text}"))),
                Message::Ping { nonce, sent_unix_millis } => {
                    let _ = conn.send(&Message::Pong { nonce, sent_unix_millis });
                }
                Message::Goodbye => return shared.disconnect(DisconnectReason::HostLeft),
                _ => return shared.protocol_error(&conn, "expected Welcome or Refused first".to_owned()),
            }
        } else {
            shared.handle_message(&conn, message);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Publishing

impl PublishBackend for ClientShared {
    fn publish(&self, item: Outbound, _target: PeerId) -> Result<(), PublishError> {
        if let Some(reason) = self.disconnected_reason() {
            return Err(PublishError::Disconnected(reason));
        }
        if !self.connected.load(Ordering::Acquire) {
            return Err(PublishError::NotConnected);
        }
        let Some(conn) = self.conn() else {
            return Err(PublishError::NotConnected);
        };
        match conn.queue.try_push(item) {
            Ok(()) => Ok(()),
            Err(PushError::Full { queued_bytes }) => {
                self.disconnect(DisconnectReason::TooSlow { queued_bytes });
                Err(PublishError::QueueFull { queued_bytes })
            }
            Err(PushError::Closed) => Err(PublishError::Disconnected(self.disconnected_reason().unwrap_or(DisconnectReason::Left))),
        }
    }

    fn local_id(&self) -> PeerId {
        self.local_id.load(Ordering::Acquire)
    }
}

// ---------------------------------------------------------------------------------------------
// Session

impl Session for ClientSession {
    fn role(&self) -> Role {
        Role::Client
    }

    fn session_id(&self) -> SessionId {
        self.shared.session_id.load(Ordering::Acquire)
    }

    fn local_peer_id(&self) -> PeerId {
        self.shared.local_id.load(Ordering::Acquire)
    }

    fn participants(&self) -> Vec<ParticipantInfo> {
        self.shared.lock().participants.values().cloned().collect()
    }

    fn is_connected(&self) -> bool {
        self.shared.connected.load(Ordering::Acquire) && self.shared.disconnected_reason().is_none()
    }

    fn poll_events(&self) -> Vec<SessionEvent> {
        self.shared.flush_coalescer();
        self.shared.events.drain()
    }

    fn publisher(&self) -> PublisherHandle {
        PublisherHandle::new(Arc::clone(&self.shared) as Arc<dyn PublishBackend>)
    }

    fn subscribe(&self, publisher: PeerId, sink: Box<dyn FollowerSink>) -> Result<(), PlayTogetherError> {
        if let Some(reason) = self.shared.disconnected_reason() {
            return Err(PlayTogetherError::Disconnected(reason));
        }
        if !self.shared.connected.load(Ordering::Acquire) {
            return Err(PlayTogetherError::NotConnected);
        }
        let slot = self.shared.follow_slot(publisher).ok_or(PlayTogetherError::NoSuchPeer(publisher))?;
        slot.set_sink(sink);
        self.shared.request_snapshot_from(publisher, true);
        Ok(())
    }

    fn unsubscribe(&self, publisher: PeerId) {
        if let Some(slot) = self.shared.follow_slot(publisher) {
            slot.clear_sink();
        }
    }

    fn request_snapshot(&self, publisher: PeerId) {
        self.shared.request_snapshot_from(publisher, false);
    }

    fn send_reset_all(&self, _countdown: Duration) -> Result<u32, PlayTogetherError> {
        Err(PlayTogetherError::NotHost)
    }

    fn kick(&self, _peer: PeerId) -> Result<(), PlayTogetherError> {
        Err(PlayTogetherError::NotHost)
    }

    fn stats(&self) -> SessionStats {
        let mut stats = base_stats(&self.shared.stats);
        if let Some(conn) = self.shared.conn() {
            stats.outbound_queued_bytes = conn.queue.queued_bytes();
            if let Some(rtt) = conn.last_rtt() {
                stats.rtt.push((HOST_PEER_ID, rtt));
            }
        }
        stats
    }

    fn leave(&self) {
        let shared = &self.shared;
        let conn = shared.conn();
        if shared.disconnected_reason().is_none() {
            if let Some(conn) = &conn {
                let _ = conn.send(&Message::Goodbye);
            }
            shared.disconnect(DisconnectReason::Left);
        }
        shared.running.store(false, Ordering::Release);
        if let Some(conn) = &conn {
            let grace = Instant::now() + GOODBYE_GRACE;
            while Instant::now() < grace && !conn.writer_done.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(10));
            }
            conn.begin_close(false);
            conn.shutdown_now();
        }
        let mut handles: Vec<JoinHandle<()>> = self.threads.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect();
        handles.extend(shared.lock().threads.drain(..));
        join_bounded(handles, LEAVE_TIMEOUT);
    }
}
