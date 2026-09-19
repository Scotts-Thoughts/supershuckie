//! The host: listens, admits clients, relays every stream to every other participant.
//!
//! Threads: `pt-host-accept` (non-blocking accept, polled every 20 ms), `pt-host-tick` (flushes
//! the snapshot-request coalescer, reaps finished connection threads, backstops the timeouts),
//! and `pt-reader-N` / `pt-writer-N` per connection.
//!
//! The host also keeps the link cable roster: which pairs are linked, which requests are
//! outstanding. It declines a request on the target's behalf when either end is busy or the
//! consoles' families differ, records a pair on `LinkAccept`, and tells the survivor (and
//! everyone else) when one end unplugs or leaves. Its own link messages take the same path as
//! a client's.

use std::collections::HashMap;
use std::io::Read;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use supershuckie_replay_recorder::replay_file::{ReplayConsoleType, ReplayPatchFormat, REPLAY_VERSION};
use supershuckie_replay_recorder::Speed;

use crate::color::assign_color;
use crate::compat::{dedupe_display_name, describe_incompatibility, follow_compatibility, link_family, sanitize_display_name, FollowCompatibility};
use crate::conn::{writer_loop, ConnShared, Outbound, PushError, StartStateItem, Stats};
use crate::error::{DisconnectReason, PlayTogetherError, PublishError};
use crate::protocol::{
    decode_link_events, decode_packets, read_frame, LeaveReason, LinkDeclineReason, LinkMessage, Message, ParticipantInfo, ReadError, RefusalReason,
    UnlinkReason, WireStartState, PEER_ID_OFFSET, PROTOCOL_VERSION,
};
use crate::session::follow::{FollowSlot, StreamOutcome};
use crate::session::{
    base_stats, join_bounded, receive_link, spawn_session_thread, Coalescer, EventQueue, FollowerSink, HostConfig, LinkSink, LinkSinks, PublishBackend,
    PublisherHandle, Role, Session, SessionEvent, SessionStats, GOODBYE_GRACE, LEAVE_TIMEOUT, POLL_INTERVAL,
};
use crate::transport::{Connection, Listener, TcpTransport, Transport};
use crate::{LocalParticipant, PeerId, SessionId, StartStateData, MAX_PARTICIPANTS};

/// The host's own peer id.
pub const HOST_PEER_ID: PeerId = 1;

/// How often the accept thread polls the listener.
const ACCEPT_INTERVAL: Duration = Duration::from_millis(20);

/// A hosted session.
pub struct HostSession {
    shared: Arc<HostShared>,
    local_addr: SocketAddr,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

struct HostShared {
    config: HostConfig,
    local: ParticipantInfo,
    session_id: SessionId,
    local_id: Arc<AtomicU16>,
    running: AtomicBool,
    left: AtomicBool,
    inner: Mutex<HostInner>,
    events: EventQueue,
    stats: Arc<Stats>,
    coalescer: Mutex<Coalescer>,
    race_counter: AtomicU32,
    conn_counter: AtomicUsize,
    /// Whether pausing is shared and, while it is, the session's pause state (what a joining
    /// client is told).
    sync_pause: Mutex<SyncPauseState>,
    /// The host's game speed, which every linked pair runs at (what a joining client is told).
    link_speed: Mutex<Speed>,
    /// The save state everyone's own game is loaded from, while one is set (sent to each joining
    /// client, whose game must then match the host's).
    start_state: Mutex<Option<Arc<StartStateItem>>>,
    /// Where the host's own link partner's frames go.
    link_sinks: LinkSinks,
}

/// The session's sync-pause setting and state, as sent in a `SyncPause` message.
#[derive(Clone, Copy, Default)]
struct SyncPauseState {
    enabled: bool,
    paused: bool,
}

#[derive(Default)]
struct HostInner {
    /// Every connection that has not started closing, pending and admitted alike.
    conns: Vec<Arc<HostConn>>,
    next_peer_id: PeerId,
    follows: HashMap<PeerId, Arc<FollowSlot>>,
    /// Reader and writer threads of every connection ever accepted, reaped by the tick thread.
    threads: Vec<JoinHandle<()>>,
    /// Who is linked with whom (each pair once, in either order).
    links: Vec<(PeerId, PeerId)>,
    /// Link requests relayed and not yet answered: `(requester, target, nonce)`; one per
    /// requester.
    link_requests: Vec<(PeerId, PeerId, u32)>,
}

struct HostConn {
    shared: ConnShared,
    number: usize,
    info: Mutex<Option<ParticipantInfo>>,
    handshake_deadline: Instant,
}

impl HostConn {
    fn peer_id(&self) -> PeerId {
        self.shared.peer_id.load(Ordering::Acquire)
    }
    fn admitted(&self) -> bool {
        self.peer_id() != 0
    }
}

/// Why a reader stopped.
enum Stop {
    Closing,
    Timeout,
}

impl HostSession {
    /// Listen with plain TCP.
    pub fn bind(config: HostConfig, local: LocalParticipant) -> Result<HostSession, PlayTogetherError> {
        Self::bind_with(TcpTransport, config, local)
    }

    /// Listen through `transport`.
    pub fn bind_with<T: Transport>(transport: T, config: HostConfig, local: LocalParticipant) -> Result<HostSession, PlayTogetherError> {
        let listener = transport.listen(&config.bind_address, config.port).map_err(|source| PlayTogetherError::Bind {
            address: format!("{}:{}", config.bind_address, config.port),
            source,
        })?;
        let local_addr = listener.local_addr().map_err(|source| PlayTogetherError::Bind {
            address: format!("{}:{}", config.bind_address, config.port),
            source,
        })?;

        let display_name = sanitize_display_name(&local.display_name);
        let session_id = new_session_id(local_addr);
        let color = assign_color(local.color, &[], session_id);
        let shared = Arc::new(HostShared {
            config: HostConfig { max_participants: config.max_participants.clamp(1, MAX_PARTICIPANTS), ..config },
            local: ParticipantInfo { peer_id: HOST_PEER_ID, display_name: display_name.clone(), color, app_version: local.app_version, publisher: local.publisher },
            session_id,
            local_id: Arc::new(AtomicU16::new(HOST_PEER_ID)),
            running: AtomicBool::new(true),
            left: AtomicBool::new(false),
            inner: Mutex::new(HostInner { next_peer_id: HOST_PEER_ID + 1, ..HostInner::default() }),
            events: EventQueue::default(),
            stats: Arc::new(Stats::default()),
            coalescer: Mutex::new(Coalescer::default()),
            race_counter: AtomicU32::new(1),
            conn_counter: AtomicUsize::new(1),
            sync_pause: Mutex::new(SyncPauseState::default()),
            link_speed: Mutex::new(Speed::default()),
            start_state: Mutex::new(None),
            link_sinks: LinkSinks::default(),
        });
        shared.events.push(SessionEvent::Connected {
            session_id,
            local_peer_id: HOST_PEER_ID,
            local_display_name: display_name,
            local_color: color,
            participants: Vec::new(),
        });

        let accept = {
            let shared = Arc::clone(&shared);
            spawn_session_thread("pt-host-accept".to_owned(), move || accept_loop(shared, listener)).map_err(PlayTogetherError::Io)?
        };
        let tick = {
            let shared = Arc::clone(&shared);
            spawn_session_thread("pt-host-tick".to_owned(), move || tick_loop(shared)).map_err(PlayTogetherError::Io)?
        };

        Ok(HostSession { shared, local_addr, threads: Mutex::new(vec![accept, tick]) })
    }

    /// Where we listen (useful when the configured port was 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

impl Drop for HostSession {
    fn drop(&mut self) {
        self.leave();
    }
}

/// A session id nobody can predict from the outside: time, address and process hashed.
fn new_session_id(addr: SocketAddr) -> SessionId {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let mut hasher = blake3::Hasher::new();
    hasher.update(&nanos.to_le_bytes());
    hasher.update(addr.to_string().as_bytes());
    hasher.update(&std::process::id().to_le_bytes());
    hasher.update(&Instant::now().elapsed().as_nanos().to_le_bytes());
    let hash = hasher.finalize();
    u64::from_le_bytes(hash.as_bytes()[..8].try_into().expect("eight bytes")).max(1)
}

// ---------------------------------------------------------------------------------------------
// Threads

fn accept_loop<L: Listener>(shared: Arc<HostShared>, listener: L) {
    while shared.running.load(Ordering::Acquire) {
        match listener.try_accept() {
            Ok(Some(connection)) => shared.add_connection(connection),
            Ok(None) => std::thread::sleep(ACCEPT_INTERVAL),
            Err(e) => {
                shared.events.push(SessionEvent::Warning(format!("accepting a connection failed: {e}")));
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
    // Dropping the listener here frees the port.
}

fn tick_loop(shared: Arc<HostShared>) {
    let interval = Duration::from_secs(1).min(shared.config.handshake_timeout / 2).min(shared.config.idle_timeout / 2).max(Duration::from_millis(50));
    while shared.running.load(Ordering::Acquire) {
        std::thread::sleep(interval);
        shared.flush_coalescer();
        shared.reap_threads();
        let now = Instant::now();
        let stale: Vec<Arc<HostConn>> = shared
            .lock()
            .conns
            .iter()
            .filter(|c| !c.admitted() && now > c.handshake_deadline)
            .cloned()
            .collect();
        for conn in stale {
            shared.close_connection(&conn, LeaveReason::Timeout, false);
        }
    }
}

impl HostShared {
    fn lock(&self) -> std::sync::MutexGuard<'_, HostInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn add_connection<C: Connection>(self: &Arc<Self>, connection: C) {
        let number = self.conn_counter.fetch_add(1, Ordering::Relaxed);
        let label = connection.peer_label();
        if let Err(e) = connection.set_timeouts(Some(POLL_INTERVAL), Some(self.config.idle_timeout)) {
            self.events.push(SessionEvent::Warning(format!("could not configure the connection from {label}: {e}")));
            connection.shutdown();
            return;
        }
        let (read, write) = match connection.split() {
            Ok(halves) => halves,
            Err(e) => {
                self.events.push(SessionEvent::Warning(format!("could not set up the connection from {label}: {e}")));
                connection.shutdown();
                return;
            }
        };
        let connection = Arc::new(connection);
        let shutdown = {
            let connection = Arc::clone(&connection);
            Box::new(move || connection.shutdown()) as Box<dyn Fn() + Send + Sync>
        };
        let conn = Arc::new(HostConn {
            shared: ConnShared::new(label, Arc::clone(&self.local_id), Arc::clone(&self.stats), shutdown),
            number,
            info: Mutex::new(None),
            handshake_deadline: Instant::now() + self.config.handshake_timeout,
        });

        let reader = {
            let shared = Arc::clone(self);
            let conn = Arc::clone(&conn);
            spawn_session_thread(format!("pt-reader-{number}"), move || reader_thread(shared, conn, read))
        };
        let writer = {
            let shared = Arc::clone(self);
            let conn = Arc::clone(&conn);
            spawn_session_thread(format!("pt-writer-{number}"), move || {
                let warn = |text: String| shared.events.push(SessionEvent::Warning(text));
                if writer_loop(write, &conn.shared, &warn).is_err() {
                    shared.close_connection(&conn, LeaveReason::IoError, false);
                }
            })
        };
        let mut inner = self.lock();
        match (reader, writer) {
            (Ok(reader), Ok(writer)) => {
                inner.conns.push(conn);
                inner.threads.push(reader);
                inner.threads.push(writer);
            }
            (reader, writer) => {
                self.events.push(SessionEvent::Warning("could not start the connection's threads".to_owned()));
                conn.shared.begin_close(false);
                conn.shared.shutdown_now();
                inner.threads.extend(reader.into_iter().chain(writer));
            }
        }
    }

    fn reap_threads(&self) {
        let mut inner = self.lock();
        let (finished, running): (Vec<_>, Vec<_>) = inner.threads.drain(..).partition(|h| h.is_finished());
        inner.threads = running;
        drop(inner);
        for handle in finished {
            let _ = handle.join();
        }
    }

    fn flush_coalescer(&self) {
        let requesters = self.coalescer.lock().unwrap_or_else(|e| e.into_inner()).flush();
        if let Some(requesters) = requesters {
            self.events.push(SessionEvent::SnapshotRequested { requesters });
        }
    }

    fn admitted_conns(&self) -> Vec<Arc<HostConn>> {
        self.lock().conns.iter().filter(|c| c.admitted()).cloned().collect()
    }

    fn conn_for(&self, peer: PeerId) -> Option<Arc<HostConn>> {
        if peer == 0 {
            return None;
        }
        self.lock().conns.iter().find(|c| c.peer_id() == peer).cloned()
    }

    fn participants(&self) -> Vec<ParticipantInfo> {
        self.lock().conns.iter().filter_map(|c| c.info.lock().unwrap_or_else(|e| e.into_inner()).clone()).collect()
    }

    /// Queue `item` on `conn`; a full queue closes the connection as too slow.
    fn push_to(&self, conn: &Arc<HostConn>, item: Outbound) {
        match conn.shared.queue.try_push(item) {
            Ok(()) | Err(PushError::Closed) => {}
            Err(PushError::Full { .. }) => self.close_connection(conn, LeaveReason::TooSlow, false),
        }
    }

    /// Send a control message to every admitted connection except `except`.
    fn broadcast(&self, message: &Message, except: PeerId) {
        let encoded = Arc::new(message.encoded());
        for conn in self.admitted_conns() {
            if conn.peer_id() != except {
                self.push_to(&conn, Outbound::Encoded(Arc::clone(&encoded)));
            }
        }
    }

    /// Close a connection for `reason`. Idempotent. An admitted peer's departure is announced to
    /// everyone else, its sink is ended, and a `Left` event is queued.
    fn close_connection(&self, conn: &Arc<HostConn>, reason: LeaveReason, flush: bool) {
        if !conn.shared.begin_close(flush) {
            return;
        }
        let peer_id = conn.peer_id();
        let slot = {
            let mut inner = self.lock();
            inner.conns.retain(|c| !Arc::ptr_eq(c, conn));
            if peer_id != 0 { inner.follows.remove(&peer_id) } else { None }
        };
        if peer_id != 0 {
            self.coalescer.lock().unwrap_or_else(|e| e.into_inner()).forget(peer_id);
            self.events.push(SessionEvent::Left { peer_id, reason });
            if self.running.load(Ordering::Acquire) {
                // The survivor's `Unlink` goes on the urgent lane, so queueing it first keeps it
                // ahead of the `PeerLeft` on the wire: the survivor's link sink always ends
                // through the `Unlink`, before it hears of the departure.
                self.unlink_departed(peer_id);
                self.broadcast(&Message::PeerLeft { peer_id, reason }, peer_id);
            }
            self.link_sinks.end(peer_id, reason);
            if let Some(slot) = slot {
                slot.end(reason);
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Link cable roster

    /// The console of a participant (the host itself included), if it is in the session.
    fn console_of(&self, peer: PeerId) -> Option<ReplayConsoleType> {
        if peer == HOST_PEER_ID {
            return Some(self.local.publisher.metadata.console_type);
        }
        self.conn_for(peer).and_then(|c| c.info.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|p| p.publisher.metadata.console_type))
    }

    /// Whether `peer` is one end of a recorded link.
    fn is_linked(inner: &HostInner, peer: PeerId) -> bool {
        inner.links.iter().any(|(a, b)| *a == peer || *b == peer)
    }

    /// Whether `a` and `b` are linked with each other.
    fn are_linked(inner: &HostInner, a: PeerId, b: PeerId) -> bool {
        inner.links.iter().any(|(x, y)| (*x == a && *y == b) || (*x == b && *y == a))
    }

    /// Hand a link cable message to `target`: the host's own event queue and sink when it is
    /// the host, else the target's urgent lane. `raw` is the framed message with `from`
    /// already stamped.
    fn deliver_link(&self, target: PeerId, message: Message, raw: Arc<Vec<u8>>) {
        if target == HOST_PEER_ID {
            if let Err(text) = receive_link(&self.events, &self.link_sinks, &self.stats, message) {
                self.events.push(SessionEvent::Warning(format!("a link cable message could not be read: {text}")));
            }
        } else if let Some(conn) = self.conn_for(target) {
            match conn.shared.send_urgent(raw) {
                Ok(()) | Err(PushError::Closed) => {}
                Err(PushError::Full { .. }) => self.close_connection(&conn, LeaveReason::TooSlow, false),
            }
        }
    }

    /// Tell everyone (the host's own event queue included) that `a` and `b` are linked, or no
    /// longer are.
    fn announce_link(&self, a: PeerId, b: PeerId, linked: bool) {
        let (a, b) = (a.min(b), a.max(b));
        let message = if linked { Message::PeerLinked { a, b } } else { Message::PeerUnlinked { a, b } };
        self.broadcast(&message, 0);
        let _ = receive_link(&self.events, &self.link_sinks, &self.stats, message);
    }

    /// Route a link cable message `from` sent (a client's, stamped, or the host's own): decline
    /// or drop what the roster forbids, keep the roster, and deliver the rest to its target.
    fn route_link(&self, from: PeerId, message: Message, raw: Vec<u8>) {
        let Some((_, target)) = message.link_endpoints() else { return };
        if target == from {
            return;
        }
        let raw = Arc::new(raw);
        match &message {
            Message::LinkRequest { nonce, .. } => {
                let nonce = *nonce;
                let decline = |reason: LinkDeclineReason| {
                    let answer = Message::LinkDecline { from: target, target: from, nonce, reason };
                    let encoded = Arc::new(answer.encoded());
                    self.deliver_link(from, answer, encoded);
                };
                let Some(their_console) = self.console_of(target) else {
                    return decline(LinkDeclineReason::Unavailable);
                };
                let our_console = self.console_of(from).unwrap_or(ReplayConsoleType::Unknown);
                {
                    let mut inner = self.lock();
                    if Self::is_linked(&inner, from) || Self::is_linked(&inner, target) {
                        drop(inner);
                        return decline(LinkDeclineReason::Busy);
                    }
                    match (link_family(our_console), link_family(their_console)) {
                        (Some(a), Some(b)) if a == b => {}
                        _ => {
                            drop(inner);
                            return decline(LinkDeclineReason::ConsoleMismatch);
                        }
                    }
                    // One outstanding request per requester: a new one replaces the old.
                    inner.link_requests.retain(|(requester, _, _)| *requester != from);
                    inner.link_requests.push((from, target, nonce));
                }
                self.deliver_link(target, message, raw);
            }
            Message::LinkAccept { nonce, .. } => {
                let nonce = *nonce;
                let mut inner = self.lock();
                let Some(index) = inner.link_requests.iter().position(|r| *r == (target, from, nonce)) else {
                    // Nothing to accept (the request was answered, withdrawn, or never relayed).
                    return;
                };
                inner.link_requests.remove(index);
                if Self::is_linked(&inner, from) || Self::is_linked(&inner, target) {
                    drop(inner);
                    let answer = Message::Unlink { from: target, target: from, reason: UnlinkReason::Busy };
                    let encoded = Arc::new(answer.encoded());
                    return self.deliver_link(from, answer, encoded);
                }
                inner.links.push((target, from));
                drop(inner);
                self.deliver_link(target, message, raw);
                self.announce_link(target, from, true);
            }
            Message::LinkDecline { nonce, .. } => {
                let nonce = *nonce;
                let mut inner = self.lock();
                let Some(index) = inner.link_requests.iter().position(|r| *r == (target, from, nonce)) else {
                    return;
                };
                inner.link_requests.remove(index);
                drop(inner);
                self.deliver_link(target, message, raw);
            }
            Message::LinkStart { .. } | Message::LinkFrame { .. } => {
                if !Self::are_linked(&self.lock(), from, target) {
                    return;
                }
                self.deliver_link(target, message, raw);
            }
            Message::Unlink { .. } => {
                let mut inner = self.lock();
                inner.link_requests.retain(|(requester, t, _)| !((*requester == from && *t == target) || (*requester == target && *t == from)));
                let was_linked = Self::are_linked(&inner, from, target);
                inner.links.retain(|(a, b)| !((*a == from && *b == target) || (*a == target && *b == from)));
                drop(inner);
                if !was_linked {
                    return;
                }
                self.deliver_link(target, message, raw);
                self.announce_link(from, target, false);
            }
            _ => {}
        }
    }

    /// `peer` left: unplug whatever it was linked with and forget its requests.
    fn unlink_departed(&self, peer: PeerId) {
        let partners: Vec<PeerId> = {
            let mut inner = self.lock();
            inner.link_requests.retain(|(requester, target, _)| *requester != peer && *target != peer);
            let partners = inner.links.iter().filter_map(|(a, b)| if *a == peer { Some(*b) } else if *b == peer { Some(*a) } else { None }).collect();
            inner.links.retain(|(a, b)| *a != peer && *b != peer);
            partners
        };
        for partner in partners {
            let message = Message::Unlink { from: peer, target: partner, reason: UnlinkReason::PeerLeft };
            let encoded = Arc::new(message.encoded());
            self.deliver_link(partner, message, encoded);
            self.announce_link(peer, partner, false);
        }
    }

    /// Tell the peer it broke the protocol and close.
    fn protocol_error(&self, conn: &Arc<HostConn>, text: String) {
        self.events.push(SessionEvent::Warning(format!("connection {} ({}) broke the protocol: {text}", conn.number, conn.shared.label)));
        let _ = conn.shared.send(&Message::Error { text });
        self.close_connection(conn, LeaveReason::ProtocolError, true);
    }

    /// Refuse a pending connection and close it.
    fn refuse(&self, conn: &Arc<HostConn>, reason: RefusalReason, text: String) {
        let _ = conn.shared.send(&Message::Refused { reason, text });
        self.close_connection(conn, LeaveReason::Left, true);
    }

    /// Admit a pending connection on its `Hello`.
    fn admit(&self, conn: &Arc<HostConn>, protocol_version: u32, replay_version: u32, app_version: String, display_name: String, color: u8, publisher: crate::PublisherInfo) {
        if !self.running.load(Ordering::Acquire) {
            return self.refuse(conn, RefusalReason::HostShuttingDown, "the host is shutting down".to_owned());
        }
        if protocol_version != PROTOCOL_VERSION {
            return self.refuse(
                conn,
                RefusalReason::ProtocolVersion,
                format!("this host speaks Play Together protocol {PROTOCOL_VERSION}, but you speak {protocol_version}; one of you needs to update"),
            );
        }
        if replay_version != REPLAY_VERSION {
            return self.refuse(
                conn,
                RefusalReason::ReplayVersion,
                format!("this host records replay format {REPLAY_VERSION}, but you record {replay_version}; one of you needs to update"),
            );
        }
        match publisher.metadata.console_type {
            ReplayConsoleType::NintendoDS if !self.config.allow_nintendo_ds => {
                return self.refuse(conn, RefusalReason::ConsoleUnsupported, "Nintendo DS is not supported by Play Together yet".to_owned());
            }
            ReplayConsoleType::Unknown => {
                return self.refuse(conn, RefusalReason::ConsoleUnsupported, "your session's console is unknown".to_owned());
            }
            _ => {}
        }
        if publisher.metadata.patch_format != ReplayPatchFormat::Unpatched {
            return self.refuse(conn, RefusalReason::PatchedRomUnsupported, "patched ROMs are not supported by Play Together yet".to_owned());
        }
        // Everyone starts from the host's save state: the joiner's game has to be able to load it.
        let start_state = self.start_state.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if start_state.is_some() {
            let compat = follow_compatibility(&publisher.metadata, &self.local.publisher.metadata);
            if compat != FollowCompatibility::Ok {
                let why = describe_incompatibility(&compat, "The host");
                return self.refuse(conn, RefusalReason::StartStateMismatch, format!("{why} Everyone in this session starts from the host's save state."));
            }
        }

        let (participant, welcome) = {
            let mut inner = self.lock();
            let admitted: Vec<ParticipantInfo> = inner.conns.iter().filter_map(|c| c.info.lock().unwrap_or_else(|e| e.into_inner()).clone()).collect();
            if admitted.len() + 1 >= self.config.max_participants {
                drop(inner);
                return self.refuse(conn, RefusalReason::SessionFull, format!("the session already has {} participants", self.config.max_participants));
            }
            let peer_id = inner.next_peer_id;
            inner.next_peer_id += 1;
            let taken: Vec<String> = std::iter::once(self.local.display_name.clone()).chain(admitted.iter().map(|p| p.display_name.clone())).collect();
            let display_name = dedupe_display_name(&display_name, &taken);
            let taken_colors: Vec<u8> = std::iter::once(self.local.color).chain(admitted.iter().map(|p| p.color)).collect();
            let color = assign_color(color, &taken_colors, self.session_id ^ u64::from(peer_id).wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let participant = ParticipantInfo { peer_id, display_name: display_name.clone(), color, app_version, publisher };
            *conn.info.lock().unwrap_or_else(|e| e.into_inner()) = Some(participant.clone());
            let mut participants = Vec::with_capacity(admitted.len() + 1);
            participants.push(self.local.clone());
            participants.extend(admitted);
            let welcome = Message::Welcome { your_peer_id: peer_id, session_id: self.session_id, your_display_name: display_name, your_color: color, participants };
            // Queued before the connection counts as admitted (still under the lock): another
            // connection's admission broadcasting its `PeerJoined` meanwhile must not get in
            // ahead of this `Welcome`.
            let _ = conn.shared.send(&welcome);
            let _ = conn.shared.send(&self.sync_pause_message());
            let _ = conn.shared.send(&Message::LinkSpeed { speed: *self.link_speed.lock().unwrap_or_else(|e| e.into_inner()) });
            if let Some(item) = start_state {
                let _ = conn.shared.queue.try_push(Outbound::StartState(item));
            }
            conn.shared.peer_id.store(peer_id, Ordering::Release);
            inner.follows.insert(peer_id, Arc::new(FollowSlot::new()));
            (participant, welcome)
        };
        let _ = welcome;
        self.broadcast(&Message::PeerJoined { participant: participant.clone() }, participant.peer_id);
        self.events.push(SessionEvent::Joined(participant));
    }

    fn sync_pause_state(&self) -> SyncPauseState {
        *self.sync_pause.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The `SyncPause` message describing the current setting and state.
    fn sync_pause_message(&self) -> Message {
        let state = self.sync_pause_state();
        Message::SyncPause { enabled: state.enabled, paused: state.paused }
    }

    /// Note the session's pause state (only while sync pause is enabled). Returns whether it is.
    fn note_pause(&self, paused: bool) -> bool {
        let mut state = self.sync_pause.lock().unwrap_or_else(|e| e.into_inner());
        if state.enabled {
            state.paused = paused;
        }
        state.enabled
    }

    fn follow_slot(&self, peer: PeerId) -> Option<Arc<FollowSlot>> {
        self.lock().follows.get(&peer).cloned()
    }

    /// Ask `publisher` for a snapshot, unless one was asked for less than two seconds ago.
    fn request_snapshot_from(&self, publisher: PeerId, force: bool) {
        let Some(slot) = self.follow_slot(publisher) else { return };
        if !slot.may_request(force) {
            return;
        }
        if let Some(conn) = self.conn_for(publisher) {
            let _ = conn.shared.send(&Message::RequestSnapshot { requester: HOST_PEER_ID, target: publisher });
        }
    }

    /// One message from an admitted peer. `raw` is the whole frame, for relaying.
    fn handle_admitted(&self, conn: &Arc<HostConn>, from: PeerId, message: Message, mut raw: Vec<u8>) {
        match message {
            Message::Hello { .. } => self.protocol_error(conn, "a second Hello on an admitted connection".to_owned()),
            Message::Welcome { .. }
            | Message::Refused { .. }
            | Message::PeerJoined { .. }
            | Message::PeerLeft { .. }
            | Message::ResetAll { .. }
            | Message::SyncPause { .. }
            | Message::LinkSpeed { .. }
            | Message::StartState(_)
            | Message::PeerLinked { .. }
            | Message::PeerUnlinked { .. } => self.protocol_error(conn, "only the host sends that message".to_owned()),
            Message::LinkRequest { .. } | Message::LinkAccept { .. } | Message::LinkDecline { .. } | Message::LinkStart { .. } | Message::LinkFrame { .. } | Message::Unlink { .. } => {
                if let Message::LinkFrame { events, .. } = &message {
                    if let Err(e) = decode_link_events(events) {
                        return self.protocol_error(conn, e.to_string());
                    }
                }
                stamp_peer_id(&mut raw, from);
                let message = match message.link_endpoints() {
                    Some(_) => stamp_from(message, from),
                    None => return,
                };
                self.route_link(from, message, raw);
            }
            Message::Pause { paused, .. } => {
                // Dropped while sync pause is off: the client may not have heard that it was
                // turned off yet.
                if self.note_pause(paused) {
                    stamp_peer_id(&mut raw, from);
                    let raw = Arc::new(raw);
                    for other in self.admitted_conns() {
                        if other.peer_id() != from {
                            self.push_to(&other, Outbound::Encoded(Arc::clone(&raw)));
                        }
                    }
                    self.events.push(SessionEvent::PauseChanged { from, paused });
                }
            }
            Message::Stream { first_frame, bytes, .. } => {
                let packets = match decode_packets(&bytes) {
                    Ok(packets) => packets,
                    Err(e) => return self.protocol_error(conn, e.to_string()),
                };
                stamp_peer_id(&mut raw, from);
                let raw = Arc::new(raw);
                for other in self.admitted_conns() {
                    if other.peer_id() != from {
                        self.push_to(&other, Outbound::Encoded(Arc::clone(&raw)));
                    }
                }
                if let Some(slot) = self.follow_slot(from) {
                    if let StreamOutcome::Gap { expected, got } = slot.on_stream(first_frame, packets) {
                        self.events.push(SessionEvent::Warning(format!(
                            "lost part of {}'s stream (expected frame {expected}, got {got}); asking for a new snapshot",
                            self.name_of(from)
                        )));
                        self.request_snapshot_from(from, false);
                    }
                }
            }
            Message::SyncHash { frame, hash, .. } => {
                stamp_peer_id(&mut raw, from);
                let raw = Arc::new(raw);
                for other in self.admitted_conns() {
                    if other.peer_id() != from {
                        self.push_to(&other, Outbound::Encoded(Arc::clone(&raw)));
                    }
                }
                if let Some(slot) = self.follow_slot(from) {
                    slot.on_sync_hash(frame, hash);
                }
            }
            Message::Snapshot(wire) => {
                let target = wire.target;
                stamp_peer_id(&mut raw, from);
                let raw = Arc::new(raw);
                match target {
                    0 => {
                        for other in self.admitted_conns() {
                            if other.peer_id() != from {
                                self.push_to(&other, Outbound::Encoded(Arc::clone(&raw)));
                            }
                        }
                    }
                    HOST_PEER_ID => {}
                    other => {
                        if let Some(other) = self.conn_for(other) {
                            self.push_to(&other, Outbound::Encoded(raw));
                        }
                    }
                }
                if target == 0 || target == HOST_PEER_ID {
                    if let Some(slot) = self.follow_slot(from) {
                        if slot.has_sink() {
                            match wire.into_snapshot() {
                                Ok(snapshot) => {
                                    slot.on_snapshot(snapshot);
                                }
                                Err(e) => self.protocol_error(conn, e.to_string()),
                            }
                        }
                    }
                }
            }
            Message::RequestSnapshot { target, .. } => {
                stamp_peer_id(&mut raw, from);
                if target == HOST_PEER_ID {
                    self.coalescer.lock().unwrap_or_else(|e| e.into_inner()).request(from);
                } else if let Some(other) = self.conn_for(target) {
                    self.push_to(&other, Outbound::Encoded(Arc::new(raw)));
                }
            }
            Message::Ping { nonce, sent_unix_millis } => {
                let _ = conn.shared.send(&Message::Pong { nonce, sent_unix_millis });
            }
            Message::Pong { nonce, .. } => {
                if let Some(rtt) = conn.shared.on_pong(nonce) {
                    self.events.push(SessionEvent::RttUpdated { peer_id: from, rtt });
                }
            }
            Message::Goodbye => self.close_connection(conn, LeaveReason::Left, false),
            Message::Error { text } => {
                self.events.push(SessionEvent::Warning(format!("{} reported a protocol error: {text}", self.name_of(from))));
                self.close_connection(conn, LeaveReason::ProtocolError, false);
            }
        }
    }

    fn name_of(&self, peer: PeerId) -> String {
        self.conn_for(peer)
            .and_then(|c| c.info.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|p| p.display_name.clone()))
            .unwrap_or_else(|| format!("peer {peer}"))
    }
}

/// Overwrite the peer-id field at [`PEER_ID_OFFSET`] with the sender's real id.
fn stamp_peer_id(frame: &mut [u8], id: PeerId) {
    if let Some(field) = frame.get_mut(PEER_ID_OFFSET..PEER_ID_OFFSET + 2) {
        field.copy_from_slice(&id.to_le_bytes());
    }
}

/// The decoded counterpart of [`stamp_peer_id`] for a link cable message.
fn stamp_from(message: Message, id: PeerId) -> Message {
    match message {
        Message::LinkRequest { target, nonce, console, .. } => Message::LinkRequest { from: id, target, nonce, console },
        Message::LinkAccept { target, nonce, .. } => Message::LinkAccept { from: id, target, nonce },
        Message::LinkDecline { target, nonce, reason, .. } => Message::LinkDecline { from: id, target, nonce, reason },
        Message::LinkStart { target, nonce, frame, input, rtt_millis, delay_setting, speed, .. } => {
            Message::LinkStart { from: id, target, nonce, frame, input, rtt_millis, delay_setting, speed }
        }
        Message::LinkFrame { target, frame, elapsed_millis, events, pair_hash_frame, pair_hash, .. } => {
            Message::LinkFrame { from: id, target, frame, elapsed_millis, events, pair_hash_frame, pair_hash }
        }
        Message::Unlink { target, reason, .. } => Message::Unlink { from: id, target, reason },
        other => other,
    }
}

fn reader_thread<R: Read>(shared: Arc<HostShared>, conn: Arc<HostConn>, mut read: R) {
    let idle_timeout = shared.config.idle_timeout;
    let mut last_rx = Instant::now();
    loop {
        let mut last_got = 0usize;
        let mut poll = |got: usize| -> Result<(), Stop> {
            if !shared.running.load(Ordering::Acquire) || conn.shared.is_closing() {
                return Err(Stop::Closing);
            }
            let now = Instant::now();
            if got > last_got {
                last_got = got;
                last_rx = now;
            }
            if !conn.admitted() && now > conn.handshake_deadline {
                return Err(Stop::Timeout);
            }
            if now.duration_since(last_rx) > idle_timeout {
                return Err(Stop::Timeout);
            }
            Ok(())
        };
        let frame = match read_frame(&mut read, &mut poll) {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                shared.close_connection(&conn, LeaveReason::IoError, false);
                return;
            }
            Err(ReadError::Io(_)) => {
                shared.close_connection(&conn, LeaveReason::IoError, false);
                return;
            }
            Err(ReadError::Decode(e)) => {
                shared.protocol_error(&conn, e.to_string());
                return;
            }
            Err(ReadError::Stopped(Stop::Closing)) => return,
            Err(ReadError::Stopped(Stop::Timeout)) => {
                shared.close_connection(&conn, LeaveReason::Timeout, false);
                return;
            }
        };
        last_rx = Instant::now();
        shared.stats.add_in(frame.len());
        if conn.shared.is_closing() {
            return;
        }
        let message = match Message::decode_frame(&frame) {
            Ok(message) => message,
            Err(crate::error::DecodeError::UnknownTag(_)) => {
                shared.stats.unknown_messages_skipped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            Err(e) => {
                shared.protocol_error(&conn, e.to_string());
                return;
            }
        };
        let peer_id = conn.peer_id();
        if peer_id == 0 {
            match message {
                Message::Hello { protocol_version, replay_version, app_version, display_name, color, publisher } => {
                    shared.admit(&conn, protocol_version, replay_version, app_version, display_name, color, publisher);
                }
                Message::Ping { nonce, sent_unix_millis } => {
                    let _ = conn.shared.send(&Message::Pong { nonce, sent_unix_millis });
                }
                _ => shared.protocol_error(&conn, "expected Hello first".to_owned()),
            }
        } else {
            shared.handle_admitted(&conn, peer_id, message, frame);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Publishing

impl PublishBackend for HostShared {
    fn publish(&self, item: Outbound, target: PeerId) -> Result<(), PublishError> {
        if self.left.load(Ordering::Acquire) {
            return Err(PublishError::Disconnected(DisconnectReason::Left));
        }
        for conn in self.admitted_conns() {
            if target == 0 || conn.peer_id() == target {
                self.push_to(&conn, item.clone());
            }
        }
        Ok(())
    }

    fn local_id(&self) -> PeerId {
        HOST_PEER_ID
    }
}

// ---------------------------------------------------------------------------------------------
// Session

impl Session for HostSession {
    fn role(&self) -> Role {
        Role::Host
    }

    fn session_id(&self) -> SessionId {
        self.shared.session_id
    }

    fn local_peer_id(&self) -> PeerId {
        HOST_PEER_ID
    }

    fn participants(&self) -> Vec<ParticipantInfo> {
        self.shared.participants()
    }

    fn is_connected(&self) -> bool {
        !self.shared.left.load(Ordering::Acquire)
    }

    fn poll_events(&self) -> Vec<SessionEvent> {
        self.shared.flush_coalescer();
        self.shared.events.drain()
    }

    fn publisher(&self) -> PublisherHandle {
        PublisherHandle::new(Arc::clone(&self.shared) as Arc<dyn PublishBackend>)
    }

    fn subscribe(&self, publisher: PeerId, sink: Box<dyn FollowerSink>) -> Result<(), PlayTogetherError> {
        if self.shared.left.load(Ordering::Acquire) {
            return Err(PlayTogetherError::Disconnected(DisconnectReason::Left));
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

    fn send_reset_all(&self, countdown: Duration) -> Result<u32, PlayTogetherError> {
        if self.shared.left.load(Ordering::Acquire) {
            return Err(PlayTogetherError::Disconnected(DisconnectReason::Left));
        }
        let race_id = self.shared.race_counter.fetch_add(1, Ordering::Relaxed);
        let countdown_millis = countdown.as_millis().min(u128::from(u32::MAX)) as u32;
        let deadline = Instant::now() + countdown;
        self.shared.broadcast(&Message::ResetAll { race_id, countdown_millis }, 0);
        self.shared.events.push(SessionEvent::ResetAll { race_id, deadline });
        Ok(race_id)
    }

    fn set_sync_pause(&self, enabled: bool, paused: bool) -> Result<(), PlayTogetherError> {
        if self.shared.left.load(Ordering::Acquire) {
            return Err(PlayTogetherError::Disconnected(DisconnectReason::Left));
        }
        *self.shared.sync_pause.lock().unwrap_or_else(|e| e.into_inner()) = SyncPauseState { enabled, paused };
        self.shared.broadcast(&Message::SyncPause { enabled, paused }, 0);
        Ok(())
    }

    fn set_link_speed(&self, speed: Speed) -> Result<(), PlayTogetherError> {
        if self.shared.left.load(Ordering::Acquire) {
            return Err(PlayTogetherError::Disconnected(DisconnectReason::Left));
        }
        *self.shared.link_speed.lock().unwrap_or_else(|e| e.into_inner()) = speed;
        self.shared.broadcast(&Message::LinkSpeed { speed }, 0);
        Ok(())
    }

    fn send_pause(&self, paused: bool) -> Result<(), PlayTogetherError> {
        if self.shared.left.load(Ordering::Acquire) {
            return Err(PlayTogetherError::Disconnected(DisconnectReason::Left));
        }
        if self.shared.note_pause(paused) {
            self.shared.broadcast(&Message::Pause { from: HOST_PEER_ID, paused }, 0);
        }
        Ok(())
    }

    fn set_start_state(&self, state: Option<StartStateData>) -> Result<(), PlayTogetherError> {
        if self.shared.left.load(Ordering::Acquire) {
            return Err(PlayTogetherError::Disconnected(DisconnectReason::Left));
        }
        let item = state.map(|data| Arc::new(StartStateItem::new(data)));
        *self.shared.start_state.lock().unwrap_or_else(|e| e.into_inner()) = item.clone();
        match item {
            Some(item) => {
                for conn in self.shared.admitted_conns() {
                    self.shared.push_to(&conn, Outbound::StartState(Arc::clone(&item)));
                }
            }
            None => self.shared.broadcast(&Message::StartState(WireStartState::cleared()), 0),
        }
        Ok(())
    }

    fn kick(&self, peer: PeerId) -> Result<(), PlayTogetherError> {
        if self.shared.left.load(Ordering::Acquire) {
            return Err(PlayTogetherError::Disconnected(DisconnectReason::Left));
        }
        let conn = self.shared.conn_for(peer).ok_or(PlayTogetherError::NoSuchPeer(peer))?;
        let _ = conn.shared.send(&Message::PeerLeft { peer_id: peer, reason: LeaveReason::Kicked });
        self.shared.close_connection(&conn, LeaveReason::Kicked, true);
        Ok(())
    }

    fn send_link(&self, message: LinkMessage) -> Result<(), PlayTogetherError> {
        if self.shared.left.load(Ordering::Acquire) {
            return Err(PlayTogetherError::Disconnected(DisconnectReason::Left));
        }
        let target = message.target();
        if target == HOST_PEER_ID || target == 0 {
            return Err(PlayTogetherError::NoSuchPeer(target));
        }
        if self.shared.conn_for(target).is_none() {
            return Err(PlayTogetherError::NoSuchPeer(target));
        }
        let message = Message::from_link(HOST_PEER_ID, message);
        let raw = message.encoded();
        self.shared.route_link(HOST_PEER_ID, message, raw);
        Ok(())
    }

    fn set_link_sink(&self, peer: PeerId, sink: Option<Box<dyn LinkSink>>) {
        self.shared.link_sinks.set(peer, sink);
    }

    fn stats(&self) -> SessionStats {
        let mut stats = base_stats(&self.shared.stats);
        for conn in self.shared.lock().conns.iter() {
            stats.outbound_queued_bytes += conn.shared.queue.queued_bytes();
            if let (id, Some(rtt)) = (conn.peer_id(), conn.shared.last_rtt()) {
                if id != 0 {
                    stats.rtt.push((id, rtt));
                }
            }
        }
        stats
    }

    fn leave(&self) {
        let shared = &self.shared;
        if shared.left.swap(true, Ordering::AcqRel) {
            return;
        }
        shared.running.store(false, Ordering::Release);

        let (conns, follows) = {
            let mut inner = shared.lock();
            (std::mem::take(&mut inner.conns), std::mem::take(&mut inner.follows))
        };
        let goodbye = Arc::new(Message::Goodbye.encoded());
        for conn in &conns {
            let _ = conn.shared.queue.try_push(Outbound::Encoded(Arc::clone(&goodbye)));
            conn.shared.begin_close(true);
        }
        for slot in follows.values() {
            slot.end(LeaveReason::Left);
        }
        shared.link_sinks.end_all(LeaveReason::Left);
        shared.events.push(SessionEvent::Disconnected { reason: DisconnectReason::Left });

        // Give the writers a moment to get the Goodbye out, then cut the sockets so blocked
        // readers and writers return.
        let grace = Instant::now() + GOODBYE_GRACE;
        while Instant::now() < grace && conns.iter().any(|c| !c.shared.writer_done.load(Ordering::Acquire)) {
            std::thread::sleep(Duration::from_millis(10));
        }
        for conn in &conns {
            conn.shared.shutdown_now();
        }

        let mut handles: Vec<JoinHandle<()>> = self.threads.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect();
        handles.extend(shared.lock().threads.drain(..));
        join_bounded(handles, LEAVE_TIMEOUT);
    }
}
