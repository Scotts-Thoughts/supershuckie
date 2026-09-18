//! Per-connection machinery shared by both roles: the bounded outbound queue, the writer thread
//! that drains it (merging streams and compressing snapshots as it goes), and the state the
//! reader and writer threads share.
//!
//! This module is public so the queue's merging can be tested from outside the crate; the
//! application does not use it.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::protocol::{compress_state, Message, StateEncoding, WireSnapshot, MAX_MESSAGE_LENGTH, STREAM_MERGE_LIMIT};
use crate::{PeerId, SnapshotData};

/// Most bytes an [`OutboundQueue`] holds before it overflows.
pub const MAX_QUEUE_BYTES: usize = 64 << 20;
/// Most items an [`OutboundQueue`] holds before it overflows.
pub const MAX_QUEUE_ITEMS: usize = 16_384;

/// How long a writer waits with nothing to send before it sends a `Ping`.
pub const PING_IDLE: Duration = Duration::from_secs(1);
/// How often a busy writer slips a `Ping` in anyway, so round-trip times keep updating while
/// streams flow.
pub const PING_INTERVAL: Duration = Duration::from_secs(2);

/// One thing waiting to be written.
#[derive(Clone, Debug)]
pub enum Outbound {
    /// A run of whole packets from the local publisher; consecutive ones are merged into one
    /// `Stream` message by the writer.
    Stream {
        /// The publisher's frame count before the first `NextFrame` in `bytes`.
        first_frame: u64,
        /// Whole packets.
        bytes: Arc<Vec<u8>>,
    },
    /// A snapshot from the local publisher; compressed on the writer thread.
    Snapshot {
        /// The snapshot (compressed at most once, shared between destinations).
        snapshot: Arc<SnapshotItem>,
        /// Who it is for (0 = everyone).
        target: PeerId,
    },
    /// An already-framed message (control messages, relays).
    Encoded(Arc<Vec<u8>>),
}

impl Outbound {
    /// The bytes this item counts for against [`MAX_QUEUE_BYTES`].
    pub fn cost(&self) -> usize {
        match self {
            Outbound::Stream { bytes, .. } => bytes.len() + 16,
            Outbound::Snapshot { snapshot, .. } => snapshot.data.state.len() + 64,
            Outbound::Encoded(bytes) => bytes.len(),
        }
    }
}

/// A snapshot waiting in one or more queues; its state is compressed by the first writer that
/// reaches it and reused by the others.
#[derive(Debug)]
pub struct SnapshotItem {
    /// The snapshot itself.
    pub data: SnapshotData,
    compressed: OnceLock<Option<Vec<u8>>>,
}

impl SnapshotItem {
    /// Wrap a snapshot.
    pub fn new(data: SnapshotData) -> SnapshotItem {
        SnapshotItem { data, compressed: OnceLock::new() }
    }

    /// The zstd-compressed state, computed on first use; `None` if compression failed.
    pub fn compressed_state(&self) -> Option<&[u8]> {
        self.compressed.get_or_init(|| compress_state(&self.data.state)).as_deref()
    }

    /// The `Snapshot` message for this item, framed.
    pub fn encode(&self, from: PeerId, target: PeerId) -> Vec<u8> {
        let wire = match self.compressed_state() {
            Some(state) => WireSnapshot::with_state(&self.data, from, target, StateEncoding::Zstd, state.to_vec()),
            None => WireSnapshot::raw(&self.data, from, target),
        };
        Message::Snapshot(wire).encoded()
    }
}

/// Why an item could not be queued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushError {
    /// The queue is over its byte or item cap.
    Full {
        /// Bytes waiting when it overflowed.
        queued_bytes: u64,
    },
    /// The connection is closing; nothing more is sent.
    Closed,
}

struct QueueState {
    items: VecDeque<Outbound>,
    bytes: usize,
    closed: bool,
}

/// What a writer got from one [`OutboundQueue::wait_drain`].
#[derive(Debug, Default)]
pub struct Drained {
    /// The items, in order.
    pub items: Vec<Outbound>,
    /// Whether the queue is closed: these are the last items and the writer should stop.
    pub closed: bool,
}

/// A bounded FIFO between anyone who sends on a connection and its writer thread.
pub struct OutboundQueue {
    state: Mutex<QueueState>,
    cv: Condvar,
    queued_bytes: AtomicU64,
}

impl Default for OutboundQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl OutboundQueue {
    /// An empty, open queue.
    pub fn new() -> OutboundQueue {
        OutboundQueue {
            state: Mutex::new(QueueState { items: VecDeque::new(), bytes: 0, closed: false }),
            cv: Condvar::new(),
            queued_bytes: AtomicU64::new(0),
        }
    }

    /// Queue an item. Never blocks beyond the mutex; over the caps it fails with `Full` and
    /// leaves the queue as it was.
    pub fn try_push(&self, item: Outbound) -> Result<(), PushError> {
        let cost = item.cost();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return Err(PushError::Closed);
        }
        if state.bytes + cost > MAX_QUEUE_BYTES || state.items.len() + 1 > MAX_QUEUE_ITEMS {
            return Err(PushError::Full { queued_bytes: state.bytes as u64 });
        }
        state.bytes += cost;
        state.items.push_back(item);
        self.queued_bytes.store(state.bytes as u64, Ordering::Relaxed);
        drop(state);
        self.cv.notify_one();
        Ok(())
    }

    /// Wait up to `timeout` for something to send (or for the queue to close), then take
    /// everything.
    pub fn wait_drain(&self, timeout: Duration) -> Drained {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.items.is_empty() && !state.closed {
            let (guard, _) = self.cv.wait_timeout(state, timeout).unwrap_or_else(|e| e.into_inner());
            state = guard;
        }
        let items: Vec<Outbound> = state.items.drain(..).collect();
        state.bytes = 0;
        self.queued_bytes.store(0, Ordering::Relaxed);
        Drained { items, closed: state.closed }
    }

    /// Close the queue: no more pushes; the writer sends what is left (unless `discard`) and
    /// stops.
    pub fn close(&self, discard: bool) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.closed = true;
        if discard {
            state.items.clear();
            state.bytes = 0;
            self.queued_bytes.store(0, Ordering::Relaxed);
        }
        drop(state);
        self.cv.notify_all();
    }

    /// Whether [`close`](Self::close) has been called.
    pub fn is_closed(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).closed
    }

    /// Bytes waiting right now.
    pub fn queued_bytes(&self) -> u64 {
        self.queued_bytes.load(Ordering::Relaxed)
    }
}

/// What [`encode_outbound`] produced.
#[derive(Debug, Default)]
pub struct EncodedBatch {
    /// The wire bytes, ready for `write_all`.
    pub bytes: Vec<u8>,
    /// How many messages `bytes` holds.
    pub messages: u64,
    /// Items that could not be sent (a snapshot too large for one message), described.
    pub dropped: Vec<String>,
}

/// Turn drained items into wire bytes: consecutive `Stream` items are merged into one `Stream`
/// message up to [`STREAM_MERGE_LIMIT`] (splitting between items, never inside one), snapshots
/// are compressed and framed, encoded items are copied through.
pub fn encode_outbound(items: &[Outbound], from: PeerId) -> EncodedBatch {
    let mut batch = EncodedBatch::default();
    let mut pending: Option<(u64, Vec<u8>)> = None;

    fn flush(pending: &mut Option<(u64, Vec<u8>)>, from: PeerId, batch: &mut EncodedBatch) {
        if let Some((first_frame, bytes)) = pending.take() {
            Message::Stream { from, first_frame, bytes }.encode(&mut batch.bytes);
            batch.messages += 1;
        }
    }

    for item in items {
        match item {
            Outbound::Stream { first_frame, bytes } => {
                match &mut pending {
                    Some((_, buf)) => buf.extend_from_slice(bytes),
                    None => pending = Some((*first_frame, bytes.as_slice().to_vec())),
                }
                // A merged message closes once it reaches the limit; a single item larger than
                // the limit goes out with whatever it was merged into (never split inside).
                if pending.as_ref().is_some_and(|(_, buf)| buf.len() >= STREAM_MERGE_LIMIT) {
                    flush(&mut pending, from, &mut batch);
                }
            }
            Outbound::Snapshot { snapshot, target } => {
                flush(&mut pending, from, &mut batch);
                let encoded = snapshot.encode(from, *target);
                if encoded.len() - 4 > MAX_MESSAGE_LENGTH as usize {
                    batch.dropped.push(format!(
                        "a snapshot of {} bytes (frame {}) is too large to send",
                        snapshot.data.state.len(),
                        snapshot.data.frame
                    ));
                } else {
                    batch.bytes.extend_from_slice(&encoded);
                    batch.messages += 1;
                }
            }
            Outbound::Encoded(bytes) => {
                flush(&mut pending, from, &mut batch);
                batch.bytes.extend_from_slice(bytes);
                batch.messages += 1;
            }
        }
    }
    flush(&mut pending, from, &mut batch);
    batch
}

/// Byte and message counters for one session.
#[derive(Debug, Default)]
pub(crate) struct Stats {
    pub bytes_in: AtomicU64,
    pub bytes_out: AtomicU64,
    pub messages_in: AtomicU64,
    pub messages_out: AtomicU64,
    pub unknown_messages_skipped: AtomicU64,
}

impl Stats {
    pub(crate) fn add_in(&self, bytes: usize) {
        self.bytes_in.fetch_add(bytes as u64, Ordering::Relaxed);
        self.messages_in.fetch_add(1, Ordering::Relaxed);
    }
}

/// What the reader thread, the writer thread and the session share about one connection.
pub(crate) struct ConnShared {
    /// The peer's address, for logs.
    pub label: String,
    /// What is waiting to be written.
    pub queue: OutboundQueue,
    /// Set once when the connection starts closing; readers and writers stop when they see it.
    pub closing: AtomicBool,
    /// Set by the writer thread when it has exited.
    pub writer_done: AtomicBool,
    /// The remote peer's id (0 until known).
    pub peer_id: std::sync::atomic::AtomicU16,
    /// The local peer id, written into `from` fields by the writer.
    pub local_id: Arc<std::sync::atomic::AtomicU16>,
    /// The session's counters.
    pub stats: Arc<Stats>,
    /// The last ping sent and not yet answered.
    pending_ping: Mutex<Option<(u32, Instant)>>,
    /// The most recent round-trip time.
    last_rtt: Mutex<Option<Duration>>,
    ping_nonce: AtomicU32,
    shutdown: Box<dyn Fn() + Send + Sync>,
}

impl ConnShared {
    pub(crate) fn new(label: String, local_id: Arc<std::sync::atomic::AtomicU16>, stats: Arc<Stats>, shutdown: Box<dyn Fn() + Send + Sync>) -> ConnShared {
        ConnShared {
            label,
            queue: OutboundQueue::new(),
            closing: AtomicBool::new(false),
            writer_done: AtomicBool::new(false),
            peer_id: std::sync::atomic::AtomicU16::new(0),
            local_id,
            stats,
            pending_ping: Mutex::new(None),
            last_rtt: Mutex::new(None),
            ping_nonce: AtomicU32::new(1),
            shutdown,
        }
    }

    /// Queue an already-framed message.
    pub(crate) fn send(&self, message: &Message) -> Result<(), PushError> {
        self.queue.try_push(Outbound::Encoded(Arc::new(message.encoded())))
    }

    /// Whether closing has begun.
    pub(crate) fn is_closing(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }

    /// Begin closing: returns `true` the first time. With `flush`, the writer sends what is
    /// queued first (so a `Goodbye` or `Error` gets out); without it the queue is dropped. The
    /// socket is shut down by the writer when it exits, or here if it already has.
    pub(crate) fn begin_close(&self, flush: bool) -> bool {
        if self.closing.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.queue.close(!flush);
        if self.writer_done.load(Ordering::Acquire) {
            (self.shutdown)();
        }
        true
    }

    /// Shut the socket down now (used when the writer has stopped, or to abort a stuck one).
    pub(crate) fn shutdown_now(&self) {
        (self.shutdown)();
    }

    /// The `Ping` to send now.
    fn make_ping(&self) -> Message {
        let nonce = self.ping_nonce.fetch_add(1, Ordering::Relaxed);
        *self.pending_ping.lock().unwrap_or_else(|e| e.into_inner()) = Some((nonce, Instant::now()));
        Message::Ping { nonce, sent_unix_millis: unix_millis() }
    }

    /// Record a `Pong`; the round-trip time if it answers our outstanding ping.
    pub(crate) fn on_pong(&self, nonce: u32) -> Option<Duration> {
        let mut pending = self.pending_ping.lock().unwrap_or_else(|e| e.into_inner());
        match *pending {
            Some((expected, sent)) if expected == nonce => {
                *pending = None;
                let rtt = sent.elapsed();
                *self.last_rtt.lock().unwrap_or_else(|e| e.into_inner()) = Some(rtt);
                Some(rtt)
            }
            _ => None,
        }
    }

    /// The most recent round-trip time.
    pub(crate) fn last_rtt(&self) -> Option<Duration> {
        *self.last_rtt.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Milliseconds since the Unix epoch, for `Ping`.
pub(crate) fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The writer thread: drain the queue, encode, write, ping when idle; stop when the queue
/// closes or the socket fails. Shuts the socket down on exit.
pub(crate) fn writer_loop<W: Write>(mut w: W, conn: &ConnShared, on_warning: &dyn Fn(String)) -> io::Result<()> {
    let result = writer_body(&mut w, conn, on_warning);
    conn.writer_done.store(true, Ordering::Release);
    if conn.is_closing() || result.is_err() {
        conn.shutdown_now();
    }
    result
}

fn writer_body<W: Write>(w: &mut W, conn: &ConnShared, on_warning: &dyn Fn(String)) -> io::Result<()> {
    let mut last_write = Instant::now();
    let mut last_ping = Instant::now();
    loop {
        let drained = conn.queue.wait_drain(PING_IDLE);
        let from = conn.local_id.load(Ordering::Acquire);
        let mut batch = encode_outbound(&drained.items, from);
        for text in batch.dropped.drain(..) {
            on_warning(text);
        }
        if !drained.closed {
            let now = Instant::now();
            if now.duration_since(last_write) >= PING_IDLE || now.duration_since(last_ping) >= PING_INTERVAL {
                conn.make_ping().encode(&mut batch.bytes);
                batch.messages += 1;
                last_ping = now;
            }
        }
        if !batch.bytes.is_empty() {
            write_all_polling(w, &batch.bytes, conn)?;
            w.flush()?;
            last_write = Instant::now();
            conn.stats.bytes_out.fetch_add(batch.bytes.len() as u64, Ordering::Relaxed);
            conn.stats.messages_out.fetch_add(batch.messages, Ordering::Relaxed);
        }
        if drained.closed {
            return Ok(());
        }
    }
}

/// `write_all` that treats a socket write timeout as a failure only if the connection is
/// already closing (otherwise the peer is merely slow and the write is retried).
fn write_all_polling<W: Write>(w: &mut W, mut buf: &[u8], conn: &ConnShared) -> io::Result<()> {
    while !buf.is_empty() {
        match w.write(buf) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "socket accepted no bytes")),
            Ok(n) => buf = &buf[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if matches!(e.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) => {
                if conn.is_closing() {
                    return Err(e);
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
