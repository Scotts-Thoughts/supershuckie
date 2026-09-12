//! Live memory access for the RAM tools (viewer, search, watch, editing, freezing).
//!
//! The UI side describes what it wants in a [`MonitorRequest`] held by a shared
//! [`MemoryMonitorShared`]; the core thread services it at frame boundaries through
//! [`MemoryMonitorLocal::service`]. The core thread only ever uses atomics and `try_lock` on the
//! shared state (it never waits for the UI), fills buffers it already owns and swaps them out,
//! and copies only what was asked for: the bytes on screen at a wall-clock rate independent of
//! emulation speed, the handful of traced or frozen values once per frame, and a full copy of
//! memory only when a search asks for one.
//!
//! Everything happens between frames in a fixed order: traces observe what the game did, pause
//! conditions are checked, freezes restore their values, pending edits are applied, and finally
//! the sample shows the state the next frame will start from.

use crate::emulator::{locate_memory, memory_slice, EmulatorCore, MemoryRegionInfo};
use crate::SuperShuckieCore;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};
use std::vec::Vec;

/// Viewer windows that can be sampled at once (one per viewer window).
pub const MAX_VIEW_WINDOWS: usize = 4;

/// Bytes sampled across all viewer windows per sample.
pub const MAX_VIEW_BYTES: usize = 16 * 1024;

/// Scattered values (visible watch and search rows) sampled per sample.
pub const MAX_PROBES: usize = 512;

/// Longest value a probe, trace or freeze entry covers.
pub const MAX_VALUE_LEN: usize = 64;

/// Watches compared every emulated frame.
pub const MAX_TRACES: usize = 64;

/// Freeze entries held at once.
pub const MAX_FREEZES: usize = 256;

/// Bytes all freeze entries may hold in total.
pub const MAX_FREEZE_BYTES: usize = 4096;

/// Largest single edit.
pub const MAX_EDIT_LEN: usize = 4096;

/// Pointer dereferences an [`AddressPath`] may go through.
pub const MAX_POINTER_DEREFS: usize = 4;

/// Events kept for the UI before the oldest are dropped.
pub const EVENT_RING_CAPACITY: usize = 8192;

/// Events buffered on the core thread between flushes before further ones are dropped.
const LOCAL_EVENT_CAPACITY: usize = 1024;

/// An address, optionally reached through pointers: start at `base`, then for each offset read a
/// little-endian 32-bit pointer at the current address and add the offset to it.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct AddressPath {
    /// Where the path starts.
    pub base: u32,
    derefs: [i32; MAX_POINTER_DEREFS],
    deref_count: u8
}

impl AddressPath {
    /// A plain address.
    #[inline]
    pub const fn direct(address: u32) -> Self {
        Self { base: address, derefs: [0; MAX_POINTER_DEREFS], deref_count: 0 }
    }

    /// `base`, then through a pointer per offset (`[[base] + a] + b`…). `None` if there are more
    /// than [`MAX_POINTER_DEREFS`] offsets.
    pub fn with_derefs(base: u32, offsets: &[i32]) -> Option<Self> {
        if offsets.len() > MAX_POINTER_DEREFS {
            return None
        }
        let mut path = Self::direct(base);
        path.derefs[..offsets.len()].copy_from_slice(offsets);
        path.deref_count = offsets.len() as u8;
        Some(path)
    }

    /// The pointer offsets, in the order they are applied.
    #[inline]
    pub fn derefs(&self) -> &[i32] {
        &self.derefs[..self.deref_count as usize]
    }

    /// Whether this is a plain address.
    #[inline]
    pub const fn is_direct(&self) -> bool {
        self.deref_count == 0
    }

    /// The final address, or `None` if a pointer on the way is unmapped.
    pub fn resolve(&self, core: &(impl EmulatorCore + ?Sized)) -> Option<u32> {
        let mut address = self.base;
        for &offset in self.derefs() {
            let pointer = memory_slice(core, address, 4)?;
            address = u32::from_le_bytes([pointer[0], pointer[1], pointer[2], pointer[3]]).wrapping_add_signed(offset);
        }
        Some(address)
    }
}

/// A byte range the viewer wants to see.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ViewWindow {
    /// First address.
    pub address: u32,
    /// Bytes wanted (capped by [`MAX_VIEW_BYTES`] across all windows).
    pub len: u32
}

/// A value to read with every sample.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Probe {
    /// Where the value lives.
    pub path: AddressPath,
    /// Its length in bytes (1..=[`MAX_VALUE_LEN`]).
    pub len: u8
}

/// How to turn a traced value's bytes into a number for [`TraceCondition`]s.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ValueDecode {
    /// Most significant byte first.
    pub big_endian: bool,
    /// Two's complement.
    pub signed: bool,
    /// Binary-coded decimal (two digits per byte).
    pub bcd: bool
}

impl ValueDecode {
    /// The number held by `bytes` (at most 8 of them), or `None` for invalid BCD.
    pub fn decode(&self, bytes: &[u8]) -> Option<i64> {
        let len = bytes.len().min(8);
        let bytes = &bytes[..len];
        if len == 0 {
            return Some(0)
        }

        if self.bcd {
            let mut value: i64 = 0;
            let mut push = |byte: u8| -> Option<()> {
                let (hi, lo) = (byte >> 4, byte & 0xF);
                if hi > 9 || lo > 9 {
                    return None
                }
                value = value * 100 + (hi * 10 + lo) as i64;
                Some(())
            };
            if self.big_endian {
                for &b in bytes { push(b)?; }
            }
            else {
                for &b in bytes.iter().rev() { push(b)?; }
            }
            return Some(value)
        }

        let mut raw = 0u64;
        if self.big_endian {
            for &b in bytes { raw = (raw << 8) | b as u64; }
        }
        else {
            for &b in bytes.iter().rev() { raw = (raw << 8) | b as u64; }
        }

        if self.signed && len < 8 {
            let shift = 64 - len as u32 * 8;
            Some(((raw << shift) as i64) >> shift)
        }
        else {
            Some(raw as i64)
        }
    }
}

/// When a traced watch pauses emulation. Conditions fire on the frame the value starts meeting
/// them, not on every frame it keeps meeting them.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TraceCondition {
    /// Any change.
    Changes,
    /// Becomes this value.
    Equals(i64),
    /// Stops being this value.
    NotEquals(i64),
    /// Rises above this value.
    GreaterThan(i64),
    /// Falls below this value.
    LessThan(i64),
    /// Goes up by exactly this much in one frame.
    IncreasedBy(i64),
    /// Goes down by exactly this much in one frame.
    DecreasedBy(i64)
}

impl TraceCondition {
    /// Whether going from `old` to `new` meets the condition. `None` values could not be decoded
    /// (invalid BCD); only [`TraceCondition::Changes`] can fire for them.
    pub fn fires(&self, old: Option<i64>, new: Option<i64>, changed: bool) -> bool {
        if !changed {
            return false
        }
        let (Some(old), Some(new)) = (old, new) else {
            return matches!(self, TraceCondition::Changes)
        };
        match *self {
            TraceCondition::Changes => true,
            TraceCondition::Equals(v) => new == v && old != v,
            TraceCondition::NotEquals(v) => new != v && old == v,
            TraceCondition::GreaterThan(v) => new > v && old <= v,
            TraceCondition::LessThan(v) => new < v && old >= v,
            TraceCondition::IncreasedBy(d) => new.wrapping_sub(old) == d,
            TraceCondition::DecreasedBy(d) => old.wrapping_sub(new) == d
        }
    }
}

/// A watch compared every emulated frame.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TraceSpec {
    /// The watch's id, echoed in events.
    pub id: u32,
    /// Where the value lives.
    pub path: AddressPath,
    /// Its length in bytes (1..=[`MAX_VALUE_LEN`]). Values longer than 8 bytes are compared by hash.
    pub len: u8,
    /// How to read it as a number for `pause_when`.
    pub decode: ValueDecode,
    /// Pause emulation when this is met.
    pub pause_when: Option<TraceCondition>
}

/// A value held in place.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FreezeSpec {
    /// The freeze's id (the watch holding it), used to keep its restore count across requests.
    pub id: u32,
    /// Where the value lives.
    pub path: AddressPath,
    len: u8,
    bytes: [u8; MAX_VALUE_LEN]
}

impl FreezeSpec {
    /// Hold `bytes` (1..=[`MAX_VALUE_LEN`] of them) at `path`.
    pub fn new(id: u32, path: AddressPath, bytes: &[u8]) -> Option<Self> {
        if bytes.is_empty() || bytes.len() > MAX_VALUE_LEN {
            return None
        }
        let mut spec = Self { id, path, len: bytes.len() as u8, bytes: [0; MAX_VALUE_LEN] };
        spec.bytes[..bytes.len()].copy_from_slice(bytes);
        Some(spec)
    }

    /// The bytes held.
    #[inline]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }
}

/// Everything the UI side wants from the core thread.
#[derive(Clone, Debug)]
pub struct MonitorRequest {
    /// Whether to take samples at all (viewer windows and probes).
    pub sampling: bool,
    /// Wall-clock time between samples while emulation runs.
    pub interval: Duration,
    /// Viewer windows, by viewer slot.
    pub windows: [Option<ViewWindow>; MAX_VIEW_WINDOWS],
    /// Values to read with every sample.
    pub probes: Vec<Probe>,
    /// Watches compared every frame.
    pub traces: Vec<TraceSpec>,
    /// Values held in place.
    pub freezes: Vec<FreezeSpec>,
    /// Keep freezes but do not apply them (replay playback).
    pub freezes_suspended: bool
}

impl Default for MonitorRequest {
    fn default() -> Self {
        Self {
            sampling: false,
            interval: Duration::from_millis(33),
            windows: [None; MAX_VIEW_WINDOWS],
            probes: Vec::new(),
            traces: Vec::new(),
            freezes: Vec::new(),
            freezes_suspended: false
        }
    }
}

impl MonitorRequest {
    /// Enforce the limits (lengths, counts, byte budgets). Applied by the core thread to every
    /// request it takes, so a bad request costs no more than a good one.
    pub fn clamp(&mut self) {
        let mut view_budget = MAX_VIEW_BYTES as u32;
        for window in self.windows.iter_mut().flatten() {
            window.len = window.len.min(view_budget);
            view_budget -= window.len;
        }
        self.probes.truncate(MAX_PROBES);
        for probe in &mut self.probes {
            probe.len = probe.len.clamp(1, MAX_VALUE_LEN as u8);
        }
        self.traces.truncate(MAX_TRACES);
        for trace in &mut self.traces {
            trace.len = trace.len.clamp(1, MAX_VALUE_LEN as u8);
        }
        self.freezes.truncate(MAX_FREEZES);
        let mut freeze_budget = MAX_FREEZE_BYTES;
        self.freezes.retain(|f| {
            let keep = f.len as usize <= freeze_budget;
            if keep {
                freeze_budget -= f.len as usize;
            }
            keep
        });
        self.interval = self.interval.max(Duration::from_millis(1));
    }

    /// Whether this request needs the core thread to do anything at all.
    pub fn is_idle(&self) -> bool {
        !self.sampling && self.traces.is_empty() && self.freezes.is_empty()
    }
}

/// One viewer window's bytes in a [`MonitorSample`].
#[derive(Clone, Debug, Default)]
pub struct WindowSample {
    /// Whether this viewer slot asked for anything.
    pub active: bool,
    /// First address.
    pub address: u32,
    /// The bytes; only the first `valid_len` are mapped.
    pub bytes: Vec<u8>,
    /// How many bytes from the start are mapped memory.
    pub valid_len: u32
}

/// What the core thread read for a request, at one frame boundary.
#[derive(Clone, Debug, Default)]
pub struct MonitorSample {
    /// Increases with every sample published; 0 means "no sample".
    pub generation: u64,
    /// Emulated frame the sample was taken after.
    pub frame: u64,
    /// [`SuperShuckieCore::state_epoch`] when it was taken.
    pub state_epoch: u64,
    /// The request generation it answered (older samples may describe other windows).
    pub request_generation: u64,
    /// Viewer windows, by slot.
    pub windows: [WindowSample; MAX_VIEW_WINDOWS],
    /// Each probe's bytes, concatenated in request order (`probe_offsets` gives each start).
    pub probe_values: Vec<u8>,
    /// Start of each probe's bytes in `probe_values`.
    pub probe_offsets: Vec<u32>,
    /// Whether each probe could be read.
    pub probe_ok: Vec<bool>,
    /// Each probe's final address (after pointers).
    pub probe_addresses: Vec<u32>,
    /// Per freeze in request order: frames on which its value had to be restored, since the freeze
    /// was added.
    pub freeze_restores: Vec<u32>,
    /// Per freeze in request order: whether its address resolved at the last boundary.
    pub freeze_ok: Vec<bool>
}

impl MonitorSample {
    /// The bytes of probe `index`, if it could be read.
    pub fn probe(&self, index: usize) -> Option<&[u8]> {
        if !*self.probe_ok.get(index)? {
            return None
        }
        let start = self.probe_offsets[index] as usize;
        let end = self.probe_offsets.get(index + 1).map(|e| *e as usize).unwrap_or(self.probe_values.len());
        self.probe_values.get(start..end)
    }
}

/// Why an edit was not applied.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WriteFailure {
    /// The address (or part of the range) is not mapped.
    Unmapped,
    /// The region cannot be written by the tools.
    ReadOnly,
    /// A pointer on the way to the address is unmapped.
    PointerInvalid,
    /// A replay is being played back.
    Playback,
    /// Empty, or longer than [`MAX_EDIT_LEN`].
    BadLength
}

/// Something the core thread observed or did.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum MonitorEvent {
    /// A traced watch changed. Values of up to 8 bytes are packed little-endian into `old`/`new`;
    /// longer ones are 64-bit FNV-1a hashes.
    Changed {
        frame: u64,
        id: u32,
        old: u64,
        new: u64
    },
    /// Memory was replaced wholesale (state load, reset, seek): traces started over rather than
    /// reporting changes.
    Discontinuity {
        frame: u64
    },
    /// A traced watch's condition paused emulation.
    PausedByCondition {
        frame: u64,
        id: u32
    },
    /// An edit was applied. `old` holds the exact bytes it replaced.
    Written {
        frame: u64,
        edit_id: u64,
        address: u32,
        old: Vec<u8>,
        new: Vec<u8>
    },
    /// An edit was rejected.
    WriteFailed {
        edit_id: u64,
        reason: WriteFailure
    }
}

/// A one-time write requested by the UI.
#[derive(Clone, Debug)]
pub struct MemoryEdit {
    /// Echoed in the resulting event.
    pub edit_id: u64,
    /// Where to write.
    pub path: AddressPath,
    /// What to write.
    pub data: Vec<u8>
}

/// A region's place in a [`FullSnapshot`].
#[derive(Copy, Clone, Debug)]
pub struct SnapshotRegion {
    /// The region.
    pub info: MemoryRegionInfo,
    /// Where its `info.len` bytes start in [`FullSnapshot::bytes`].
    pub offset: usize,
    /// How many of them are mapped memory; the rest are zero.
    pub available: usize
}

/// A copy of every memory region at one frame boundary, for searching.
#[derive(Debug)]
pub struct FullSnapshot {
    /// The id passed to [`MemoryMonitorShared::request_snapshot`].
    pub job_id: u64,
    /// Emulated frame it was taken after.
    pub frame: u64,
    /// [`SuperShuckieCore::state_epoch`] when it was taken.
    pub state_epoch: u64,
    /// The regions, in order.
    pub regions: Vec<SnapshotRegion>,
    /// All regions' bytes, concatenated.
    pub bytes: Vec<u8>
}

#[derive(Debug, Default)]
struct EventRing {
    events: VecDeque<MonitorEvent>,
    dropped: u64
}

/// State shared between the UI side and the core thread.
pub struct MemoryMonitorShared {
    request: Mutex<MonitorRequest>,
    request_generation: AtomicU64,

    sample: Mutex<MonitorSample>,
    sample_generation: AtomicU64,

    events: Mutex<EventRing>,
    events_generation: AtomicU64,

    edits: Mutex<Vec<MemoryEdit>>,
    edits_generation: AtomicU64,

    snapshot_requested: AtomicU64,
    snapshot_sender: Mutex<Option<Sender<FullSnapshot>>>,
    snapshot_pool: Mutex<Vec<Vec<u8>>>
}

/// Lock a mutex from the UI side, ignoring poisoning (the data stays usable).
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Lock a mutex from the core thread without waiting.
fn try_lock<T>(mutex: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    match mutex.try_lock() {
        Ok(guard) => Some(guard),
        Err(TryLockError::WouldBlock) => None,
        Err(TryLockError::Poisoned(e)) => Some(e.into_inner())
    }
}

impl MemoryMonitorShared {
    /// A monitor with an idle request.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            request: Mutex::new(MonitorRequest::default()),
            request_generation: AtomicU64::new(1),
            sample: Mutex::new(MonitorSample::default()),
            sample_generation: AtomicU64::new(0),
            events: Mutex::new(EventRing::default()),
            events_generation: AtomicU64::new(0),
            edits: Mutex::new(Vec::new()),
            edits_generation: AtomicU64::new(0),
            snapshot_requested: AtomicU64::new(0),
            snapshot_sender: Mutex::new(None),
            snapshot_pool: Mutex::new(Vec::new())
        })
    }

    /// Change the request. The core thread picks it up at its next frame boundary (wake it if it
    /// may be paused).
    pub fn update_request<R>(&self, f: impl FnOnce(&mut MonitorRequest) -> R) -> R {
        let mut request = lock(&self.request);
        let r = f(&mut request);
        self.request_generation.fetch_add(1, Ordering::Relaxed);
        r
    }

    /// A copy of the current request.
    pub fn request(&self) -> MonitorRequest {
        lock(&self.request).clone()
    }

    /// The current request's generation.
    #[inline]
    pub fn request_generation(&self) -> u64 {
        self.request_generation.load(Ordering::Relaxed)
    }

    /// If a sample newer than `last_seen` has been published, swap it into `into` and return its
    /// generation. `into`'s old buffers go back to the core thread, so nothing is allocated.
    pub fn take_sample(&self, last_seen: u64, into: &mut MonitorSample) -> Option<u64> {
        if self.sample_generation.load(Ordering::Relaxed) <= last_seen {
            return None
        }
        let mut sample = lock(&self.sample);
        if sample.generation <= last_seen {
            return None
        }
        core::mem::swap(&mut *sample, into);
        Some(into.generation)
    }

    /// Move every event observed so far into `into`, returning how many were dropped because
    /// nobody drained them in time.
    pub fn drain_events(&self, into: &mut Vec<MonitorEvent>) -> u64 {
        if self.events_generation.load(Ordering::Relaxed) == 0 {
            return 0
        }
        let mut ring = lock(&self.events);
        into.extend(ring.events.drain(..));
        core::mem::take(&mut ring.dropped)
    }

    /// Queue a one-time write for the next frame boundary.
    pub fn push_edit(&self, edit: MemoryEdit) {
        lock(&self.edits).push(edit);
        self.edits_generation.fetch_add(1, Ordering::Relaxed);
    }

    /// Ask for a [`FullSnapshot`] at the next frame boundary, delivered to the sender set with
    /// [`set_snapshot_sender`](Self::set_snapshot_sender). `job_id` must not be zero; a newer
    /// request replaces one not taken yet.
    pub fn request_snapshot(&self, job_id: u64) {
        debug_assert_ne!(job_id, 0);
        self.snapshot_requested.store(job_id, Ordering::Relaxed);
    }

    /// Where full snapshots go.
    pub fn set_snapshot_sender(&self, sender: Option<Sender<FullSnapshot>>) {
        *lock(&self.snapshot_sender) = sender;
    }

    /// Give a snapshot buffer back so the next snapshot does not allocate.
    pub fn recycle_snapshot_buffer(&self, mut buffer: Vec<u8>) {
        let mut pool = lock(&self.snapshot_pool);
        if pool.len() < 4 {
            buffer.clear();
            pool.push(buffer);
        }
    }
}

/// What servicing the monitor asks the core thread to do.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ServiceOutcome {
    /// A trace's condition was met: pause emulation.
    pub pause: bool
}

/// The part of a core the monitor needs. Implemented by [`SuperShuckieCore`]; benchmarks and
/// tests can implement it over a bare [`EmulatorCore`].
pub trait MonitoredCore {
    /// The emulator, for reading memory.
    fn emulator(&self) -> &dyn EmulatorCore;
    /// Frames emulated so far.
    fn frame(&self) -> u64;
    /// See [`SuperShuckieCore::state_epoch`].
    fn state_epoch(&self) -> u64;
    /// Whether the core is between frames' slices (Game Boy sub-frame stepping).
    fn is_mid_frame(&self) -> bool;
    /// Whether a replay is being played back.
    fn is_playing_back(&self) -> bool;
    /// Run until the current frame is complete.
    fn finish_current_frame(&mut self);
    /// Write `data` at `address` (recorded into a replay being recorded). Returns whether it was
    /// written.
    fn write(&mut self, address: u32, data: &[u8]) -> bool;
    /// Write only if the bytes there differ. Returns whether it wrote.
    fn write_if_changed(&mut self, address: u32, data: &[u8]) -> bool;
}

impl MonitoredCore for SuperShuckieCore {
    #[inline]
    fn emulator(&self) -> &dyn EmulatorCore {
        self.get_core()
    }
    #[inline]
    fn frame(&self) -> u64 {
        self.total_frames()
    }
    #[inline]
    fn state_epoch(&self) -> u64 {
        SuperShuckieCore::state_epoch(self)
    }
    #[inline]
    fn is_mid_frame(&self) -> bool {
        SuperShuckieCore::is_mid_frame(self)
    }
    #[inline]
    fn is_playing_back(&self) -> bool {
        SuperShuckieCore::is_playing_back(self)
    }
    #[inline]
    fn finish_current_frame(&mut self) {
        SuperShuckieCore::finish_current_frame(self)
    }
    #[inline]
    fn write(&mut self, address: u32, data: &[u8]) -> bool {
        self.enqueue_write(address, data.into())
    }
    #[inline]
    fn write_if_changed(&mut self, address: u32, data: &[u8]) -> bool {
        SuperShuckieCore::write_if_changed(self, address, data)
    }
}

#[derive(Copy, Clone)]
struct TraceState {
    spec: TraceSpec,
    value: u64,
    decoded: Option<i64>,
    valid: bool
}

/// 64-bit FNV-1a.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// A traced value's identity for change detection: the bytes packed little-endian, or a hash.
fn pack_value(bytes: &[u8]) -> u64 {
    if bytes.len() <= 8 {
        let mut packed = [0u8; 8];
        packed[..bytes.len()].copy_from_slice(bytes);
        u64::from_le_bytes(packed)
    }
    else {
        fnv1a(bytes)
    }
}

/// Copy the mapped bytes at the start of `[address, address + out.len())` into `out`, returning
/// how many there were (the rest of `out` is left zeroed).
fn copy_mapped_prefix(core: &dyn EmulatorCore, address: u32, out: &mut [u8]) -> u32 {
    out.fill(0);
    for (index, region) in core.memory_regions().iter().enumerate() {
        if address < region.base_address || address as u64 >= region.end_address() {
            continue
        }
        let offset = (address - region.base_address) as usize;
        let Some(data) = core.memory_region_data(index) else {
            return 0
        };
        let available = data.len().min(region.len as usize).saturating_sub(offset);
        let n = available.min(out.len());
        out[..n].copy_from_slice(&data[offset..offset + n]);
        return n as u32
    }
    0
}

/// The core thread's half of a memory monitor.
pub struct MemoryMonitorLocal {
    shared: Arc<MemoryMonitorShared>,

    request: MonitorRequest,
    seen_request_generation: u64,
    seen_edits_generation: u64,

    sample: MonitorSample,
    sample_unpublished: bool,
    next_sample_generation: u64,
    next_sample_at: Option<Instant>,

    initialized: bool,
    last_frame: u64,
    last_epoch: u64,

    traces: Vec<TraceState>,
    trace_scratch: [u8; MAX_VALUE_LEN],
    /// `(freeze id, restores)`, in request order.
    freeze_restores: Vec<(u32, u32)>,
    freeze_ok: Vec<bool>,

    pending_edits: Vec<MemoryEdit>,

    events: Vec<MonitorEvent>,
    events_dropped: u64,
    events_flush_at: Option<Instant>,
    flush_events_now: bool
}

impl MemoryMonitorLocal {
    /// The core thread's half of `shared`.
    pub fn new(shared: Arc<MemoryMonitorShared>) -> Self {
        // A monitor outlives the cores it is attached to; keep sample generations increasing so the
        // UI side, which remembers the last one it saw, takes this core's samples too.
        let next_sample_generation = shared.sample_generation.load(Ordering::Relaxed) + 1;
        Self {
            shared,
            request: MonitorRequest::default(),
            seen_request_generation: 0,
            seen_edits_generation: 0,
            sample: MonitorSample::default(),
            sample_unpublished: false,
            next_sample_generation,
            next_sample_at: None,
            initialized: false,
            last_frame: 0,
            last_epoch: 0,
            traces: Vec::with_capacity(MAX_TRACES),
            trace_scratch: [0; MAX_VALUE_LEN],
            freeze_restores: Vec::with_capacity(MAX_FREEZES),
            freeze_ok: Vec::with_capacity(MAX_FREEZES),
            pending_edits: Vec::new(),
            events: Vec::with_capacity(LOCAL_EVENT_CAPACITY),
            events_dropped: 0,
            events_flush_at: None,
            flush_events_now: false
        }
    }

    /// The shared half.
    #[inline]
    pub fn shared(&self) -> &Arc<MemoryMonitorShared> {
        &self.shared
    }

    /// Service the monitor. Call it on the core thread on every loop iteration; it does nothing
    /// in the middle of a running frame and next to nothing when there is nothing new.
    pub fn service<C: MonitoredCore + ?Sized>(&mut self, core: &mut C, running: bool) -> ServiceOutcome {
        let mut outcome = ServiceOutcome::default();
        if running && core.is_mid_frame() {
            return outcome
        }

        let request_changed = self.pull_request();
        let edits_arrived = self.pull_edits();

        // A paused Game Boy can sit in the middle of a frame; writes belong between frames.
        let wants_write = !self.pending_edits.is_empty() || (request_changed && !self.request.freezes.is_empty());
        if wants_write && core.is_mid_frame() && !core.is_playing_back() {
            core.finish_current_frame();
        }

        let frame = core.frame();
        let epoch = core.state_epoch();
        let jumped = self.initialized && epoch != self.last_epoch;
        let new_frame = !self.initialized || frame != self.last_frame || jumped;
        self.initialized = true;
        let playing_back = core.is_playing_back();
        let mut wrote = false;
        let mut edited = false;

        // 1-2. Observe what the game did, and whether that pauses.
        if new_frame && !self.traces.is_empty() {
            if jumped {
                self.push_event(MonitorEvent::Discontinuity { frame });
            }
            outcome.pause = self.run_traces(core.emulator(), frame, !jumped);
        }

        // 3. Freezes.
        if (new_frame || request_changed) && !playing_back && !self.request.freezes_suspended && !self.request.freezes.is_empty() {
            wrote |= self.apply_freezes(core);
        }

        // 4. Edits.
        if !self.pending_edits.is_empty() {
            edited = self.apply_edits(core, frame, playing_back);
            wrote |= edited;
        }

        // What was just written is the new baseline, not a change the game made.
        if wrote && !self.traces.is_empty() {
            self.run_traces(core.emulator(), frame, false);
        }

        // 5. Sample.
        if self.request.sampling {
            // Edits are shown right away; freeze restores are not a reason to sample more often.
            let due = request_changed || edits_arrived || edited || (new_frame && (!running || self.next_sample_at.is_none_or(|at| Instant::now() >= at)));
            if due {
                self.fill_sample(core.emulator(), frame, epoch);
                self.next_sample_at = Some(Instant::now() + self.request.interval);
            }
        }
        if self.sample_unpublished {
            self.try_publish_sample();
        }

        if !self.events.is_empty() || self.events_dropped != 0 {
            self.try_flush_events(new_frame);
        }

        let job = self.shared.snapshot_requested.swap(0, Ordering::Relaxed);
        if job != 0 {
            self.send_snapshot(core.emulator(), job, frame, epoch);
        }

        self.last_frame = frame;
        self.last_epoch = epoch;
        outcome
    }

    fn pull_request(&mut self) -> bool {
        let generation = self.shared.request_generation.load(Ordering::Relaxed);
        if generation == self.seen_request_generation {
            return false
        }
        let Some(request) = try_lock(&self.shared.request) else {
            // Being edited right now; pick it up on the next pass.
            return false
        };
        self.request.clone_from(&request);
        drop(request);
        self.seen_request_generation = generation;
        self.request.clamp();

        // Keep the baselines of traces that did not change, and restore counts by freeze id.
        let old_traces = core::mem::take(&mut self.traces);
        self.traces.extend(self.request.traces.iter().map(|spec| {
            old_traces.iter()
                .find(|t| t.spec.id == spec.id && t.spec.path == spec.path && t.spec.len == spec.len)
                .map(|t| TraceState { spec: *spec, ..*t })
                .unwrap_or(TraceState { spec: *spec, value: 0, decoded: None, valid: false })
        }));

        let old_restores = core::mem::take(&mut self.freeze_restores);
        self.freeze_restores.extend(self.request.freezes.iter().map(|f| {
            (f.id, old_restores.iter().find(|(id, _)| *id == f.id).map(|(_, n)| *n).unwrap_or(0))
        }));
        self.freeze_ok.clear();
        self.freeze_ok.resize(self.request.freezes.len(), true);
        true
    }

    fn pull_edits(&mut self) -> bool {
        let generation = self.shared.edits_generation.load(Ordering::Relaxed);
        if generation == self.seen_edits_generation {
            return false
        }
        let Some(mut edits) = try_lock(&self.shared.edits) else {
            return false
        };
        self.pending_edits.append(&mut edits);
        drop(edits);
        self.seen_edits_generation = generation;
        true
    }

    /// Compare every trace with its baseline and move the baseline along. With `report` unset the
    /// baselines are only refreshed. Returns whether a pause condition fired.
    fn run_traces(&mut self, core: &dyn EmulatorCore, frame: u64, report: bool) -> bool {
        let mut pause = false;
        for index in 0..self.traces.len() {
            let spec = self.traces[index].spec;
            let len = spec.len as usize;
            let Some(address) = spec.path.resolve(core) else {
                self.traces[index].valid = false;
                continue
            };
            let Some(bytes) = memory_slice(core, address, len) else {
                self.traces[index].valid = false;
                continue
            };
            self.trace_scratch[..len].copy_from_slice(bytes);
            let bytes = &self.trace_scratch[..len];

            let value = pack_value(bytes);
            let decoded = spec.decode.decode(bytes);
            let state = self.traces[index];

            if report && state.valid && state.value != value {
                if self.events.len() < LOCAL_EVENT_CAPACITY {
                    self.events.push(MonitorEvent::Changed { frame, id: spec.id, old: state.value, new: value });
                }
                else {
                    self.events_dropped += 1;
                }
                if let Some(condition) = spec.pause_when && condition.fires(state.decoded, decoded, true) {
                    pause = true;
                    self.push_event(MonitorEvent::PausedByCondition { frame, id: spec.id });
                }
            }

            self.traces[index].value = value;
            self.traces[index].decoded = decoded;
            self.traces[index].valid = true;
        }
        pause
    }

    fn apply_freezes<C: MonitoredCore + ?Sized>(&mut self, core: &mut C) -> bool {
        let mut wrote = false;
        for index in 0..self.request.freezes.len() {
            let freeze = self.request.freezes[index];
            let Some(address) = freeze.path.resolve(core.emulator()) else {
                self.freeze_ok[index] = false;
                continue
            };
            let writable = locate_memory(core.emulator().memory_regions(), address, freeze.len as usize)
                .is_some_and(|(region, _)| core.emulator().memory_regions()[region].writable);
            self.freeze_ok[index] = writable;
            if writable && core.write_if_changed(address, freeze.bytes()) {
                self.freeze_restores[index].1 = self.freeze_restores[index].1.saturating_add(1);
                wrote = true;
            }
        }
        wrote
    }

    fn apply_edits<C: MonitoredCore + ?Sized>(&mut self, core: &mut C, frame: u64, playing_back: bool) -> bool {
        let mut wrote = false;
        let edits = core::mem::take(&mut self.pending_edits);
        for edit in edits {
            let result = (|| {
                if playing_back {
                    return Err(WriteFailure::Playback)
                }
                if edit.data.is_empty() || edit.data.len() > MAX_EDIT_LEN {
                    return Err(WriteFailure::BadLength)
                }
                let address = edit.path.resolve(core.emulator()).ok_or(WriteFailure::PointerInvalid)?;
                let regions = core.emulator().memory_regions();
                let (region, _) = locate_memory(regions, address, edit.data.len()).ok_or(WriteFailure::Unmapped)?;
                if !regions[region].writable {
                    return Err(WriteFailure::ReadOnly)
                }
                let old = memory_slice(core.emulator(), address, edit.data.len()).ok_or(WriteFailure::Unmapped)?.to_vec();
                if old != edit.data && !core.write(address, &edit.data) {
                    return Err(WriteFailure::Playback)
                }
                Ok((address, old))
            })();

            match result {
                Ok((address, old)) => {
                    wrote = true;
                    self.push_event(MonitorEvent::Written { frame, edit_id: edit.edit_id, address, old, new: edit.data });
                }
                Err(reason) => self.push_event(MonitorEvent::WriteFailed { edit_id: edit.edit_id, reason })
            }
            self.flush_events_now = true;
        }
        wrote
    }

    /// Queue an event that must not be lost to the local capacity limit (rare, user-driven ones).
    fn push_event(&mut self, event: MonitorEvent) {
        self.events.push(event);
    }

    fn fill_sample(&mut self, core: &dyn EmulatorCore, frame: u64, epoch: u64) {
        let sample = &mut self.sample;
        sample.generation = self.next_sample_generation;
        sample.frame = frame;
        sample.state_epoch = epoch;
        sample.request_generation = self.seen_request_generation;

        for (slot, window) in self.request.windows.iter().enumerate() {
            let out = &mut sample.windows[slot];
            match window {
                Some(window) => {
                    out.active = true;
                    out.address = window.address;
                    out.bytes.resize(window.len as usize, 0);
                    out.valid_len = copy_mapped_prefix(core, window.address, &mut out.bytes);
                }
                None => {
                    out.active = false;
                    out.bytes.clear();
                    out.valid_len = 0;
                }
            }
        }

        sample.probe_values.clear();
        sample.probe_offsets.clear();
        sample.probe_ok.clear();
        sample.probe_addresses.clear();
        for probe in &self.request.probes {
            let len = probe.len as usize;
            sample.probe_offsets.push(sample.probe_values.len() as u32);
            let address = probe.path.resolve(core);
            sample.probe_addresses.push(address.unwrap_or(0));
            match address.and_then(|a| memory_slice(core, a, len)) {
                Some(bytes) => {
                    sample.probe_values.extend_from_slice(bytes);
                    sample.probe_ok.push(true);
                }
                None => {
                    sample.probe_values.resize(sample.probe_values.len() + len, 0);
                    sample.probe_ok.push(false);
                }
            }
        }

        sample.freeze_restores.clear();
        sample.freeze_restores.extend(self.freeze_restores.iter().map(|(_, n)| *n));
        sample.freeze_ok.clone_from(&self.freeze_ok);

        self.next_sample_generation += 1;
        self.sample_unpublished = true;
    }

    fn try_publish_sample(&mut self) {
        let Some(mut shared) = try_lock(&self.shared.sample) else {
            // The UI is reading; offer the same sample again next pass rather than copying again.
            return
        };
        core::mem::swap(&mut *shared, &mut self.sample);
        let generation = shared.generation;
        drop(shared);
        self.shared.sample_generation.store(generation, Ordering::Relaxed);
        self.sample_unpublished = false;
    }

    fn try_flush_events(&mut self, new_frame: bool) {
        // Traces can produce an event every frame; hand them over at most once per sample interval
        // unless something the user did is waiting for an answer.
        if !self.flush_events_now {
            if !new_frame {
                return
            }
            let now = Instant::now();
            if self.events_flush_at.is_some_and(|at| now < at) {
                return
            }
            self.events_flush_at = Some(now + self.request.interval);
        }

        let Some(mut ring) = try_lock(&self.shared.events) else {
            return
        };
        for event in self.events.drain(..) {
            if ring.events.len() >= EVENT_RING_CAPACITY {
                ring.events.pop_front();
                ring.dropped += 1;
            }
            ring.events.push_back(event);
        }
        ring.dropped += core::mem::take(&mut self.events_dropped);
        drop(ring);
        self.shared.events_generation.fetch_add(1, Ordering::Relaxed);
        self.flush_events_now = false;
    }

    fn send_snapshot(&mut self, core: &dyn EmulatorCore, job_id: u64, frame: u64, epoch: u64) {
        let Some(sender) = try_lock(&self.shared.snapshot_sender).and_then(|s| s.clone()) else {
            // Nobody to receive it (or the sender is being replaced); ask again later.
            let _ = self.shared.snapshot_requested.compare_exchange(0, job_id, Ordering::Relaxed, Ordering::Relaxed);
            return
        };

        let regions = core.memory_regions();
        let total: usize = regions.iter().map(|r| r.len as usize).sum();
        let mut bytes = try_lock(&self.shared.snapshot_pool).and_then(|mut pool| pool.pop()).unwrap_or_default();
        bytes.clear();
        bytes.reserve(total);

        let mut layout = Vec::with_capacity(regions.len());
        for (index, info) in regions.iter().enumerate() {
            let offset = bytes.len();
            let data = core.memory_region_data(index).unwrap_or(&[]);
            let available = data.len().min(info.len as usize);
            bytes.extend_from_slice(&data[..available]);
            bytes.resize(offset + info.len as usize, 0);
            layout.push(SnapshotRegion { info: *info, offset, available });
        }

        if let Err(e) = sender.send(FullSnapshot { job_id, frame, state_epoch: epoch, regions: layout, bytes }) {
            self.shared.recycle_snapshot_buffer(e.0.bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emulator::{Input, RunTime, ScreenData};
    use alloc::string::String;
    use supershuckie_replay_recorder::replay_file::{ReplayConsoleType, ReplayHeaderBlake3Hash};

    const REGIONS: [MemoryRegionInfo; 3] = [
        MemoryRegionInfo { name: "RAM", short_name: "RAM", base_address: 0x1000, len: 0x100, default_big_endian: false, writable: true },
        MemoryRegionInfo { name: "IO", short_name: "IO", base_address: 0x2000, len: 0x10, default_big_endian: false, writable: false },
        // Backed by fewer bytes than its length.
        MemoryRegionInfo { name: "SAVE", short_name: "SAVE", base_address: 0x3000, len: 0x40, default_big_endian: false, writable: true },
    ];

    struct FakeEmulator {
        ram: Vec<u8>,
        io: Vec<u8>,
        save: Vec<u8>
    }

    impl EmulatorCore for FakeEmulator {
        fn run(&mut self) -> RunTime { RunTime::ONE_FRAME }
        fn run_unlocked(&mut self) -> RunTime { RunTime::ONE_FRAME }
        fn read_ram(&self, address: u32, into: &mut [u8]) -> Result<(), &'static str> {
            crate::emulator::read_ram_from_regions(self, address, into)
        }
        fn write_ram(&mut self, address: u32, from: &[u8]) -> Result<(), &'static str> {
            let (index, offset) = locate_memory(&REGIONS, address, from.len()).ok_or("unmapped")?;
            let memory = match index { 0 => &mut self.ram, 1 => &mut self.io, _ => &mut self.save };
            memory.get_mut(offset..offset + from.len()).ok_or("short")?.copy_from_slice(from);
            Ok(())
        }
        fn memory_regions(&self) -> &[MemoryRegionInfo] { &REGIONS }
        fn memory_region_data(&self, index: usize) -> Option<&[u8]> {
            Some(match index { 0 => &self.ram, 1 => &self.io, 2 => &self.save, _ => return None })
        }
        fn set_speed(&mut self, _speed: f64) {}
        fn save_sram(&self) -> Vec<u8> { Vec::new() }
        fn create_save_state(&self) -> Vec<u8> { Vec::new() }
        fn load_save_state(&mut self, _state: &[u8]) -> Result<(), String> { Ok(()) }
        fn encode_input(&self, _input: Input, _into: &mut Vec<u8>) {}
        fn set_input_encoded(&mut self, _input: &[u8]) {}
        fn get_screens(&self) -> &[ScreenData] { &[] }
        fn swap_screen_data(&mut self, _screens: &mut [ScreenData]) {}
        fn hard_reset(&mut self) {}
        fn replay_console_type(&self) -> Option<ReplayConsoleType> { None }
        fn rom_checksum(&self) -> &ReplayHeaderBlake3Hash { unimplemented!() }
        fn bios_checksum(&self) -> &ReplayHeaderBlake3Hash { unimplemented!() }
        fn core_name(&self) -> &'static str { "fake" }
        fn frame_rate(&self) -> (u32, u32) { (60, 1) }
    }

    struct FakeCore {
        emulator: FakeEmulator,
        frame: u64,
        epoch: u64,
        playing_back: bool,
        writes: Vec<(u32, Vec<u8>)>
    }

    impl FakeCore {
        fn new() -> Self {
            Self {
                emulator: FakeEmulator { ram: alloc::vec![0; 0x100], io: alloc::vec![0; 0x10], save: alloc::vec![0; 0x20] },
                frame: 0,
                epoch: 0,
                playing_back: false,
                writes: Vec::new()
            }
        }
    }

    impl MonitoredCore for FakeCore {
        fn emulator(&self) -> &dyn EmulatorCore { &self.emulator }
        fn frame(&self) -> u64 { self.frame }
        fn state_epoch(&self) -> u64 { self.epoch }
        fn is_mid_frame(&self) -> bool { false }
        fn is_playing_back(&self) -> bool { self.playing_back }
        fn finish_current_frame(&mut self) {}
        fn write(&mut self, address: u32, data: &[u8]) -> bool {
            if self.playing_back {
                return false
            }
            self.writes.push((address, data.to_vec()));
            self.emulator.write_ram(address, data).is_ok()
        }
        fn write_if_changed(&mut self, address: u32, data: &[u8]) -> bool {
            let mut current = alloc::vec![0; data.len()];
            if self.emulator.read_ram(address, &mut current).is_err() || current == data {
                return false
            }
            self.write(address, data)
        }
    }

    fn drain(shared: &MemoryMonitorShared) -> Vec<MonitorEvent> {
        let mut events = Vec::new();
        shared.drain_events(&mut events);
        events
    }

    #[test]
    fn decode_values() {
        let le = ValueDecode::default();
        assert_eq!(le.decode(&[0x34, 0x12]), Some(0x1234));
        let be = ValueDecode { big_endian: true, ..Default::default() };
        assert_eq!(be.decode(&[0x12, 0x34]), Some(0x1234));
        let signed = ValueDecode { signed: true, ..Default::default() };
        assert_eq!(signed.decode(&[0xFE]), Some(-2));
        assert_eq!(signed.decode(&[0xFE, 0xFF]), Some(-2));
        let bcd = ValueDecode { bcd: true, big_endian: true, ..Default::default() };
        assert_eq!(bcd.decode(&[0x12, 0x34, 0x56]), Some(123456));
        assert_eq!(bcd.decode(&[0x1A]), None);
    }

    #[test]
    fn conditions_fire_on_transitions() {
        assert!(TraceCondition::Equals(5).fires(Some(4), Some(5), true));
        assert!(!TraceCondition::Equals(5).fires(Some(5), Some(5), false));
        assert!(TraceCondition::GreaterThan(10).fires(Some(10), Some(11), true));
        assert!(!TraceCondition::GreaterThan(10).fires(Some(11), Some(12), true));
        assert!(TraceCondition::DecreasedBy(3).fires(Some(10), Some(7), true));
        assert!(!TraceCondition::IncreasedBy(3).fires(Some(10), Some(7), true));
        assert!(TraceCondition::Changes.fires(None, None, true));
    }

    #[test]
    fn pointer_paths_resolve() {
        let mut core = FakeCore::new();
        core.emulator.ram[0x10..0x14].copy_from_slice(&0x1080u32.to_le_bytes());
        let path = AddressPath::with_derefs(0x1010, &[4]).unwrap();
        assert_eq!(path.resolve(&core.emulator), Some(0x1084));
        let broken = AddressPath::with_derefs(0x1020, &[0, 0]).unwrap();
        assert_eq!(broken.resolve(&core.emulator), None, "a null pointer is unmapped");
    }

    #[test]
    fn samples_windows_and_probes() {
        let shared = MemoryMonitorShared::new();
        let mut local = MemoryMonitorLocal::new(shared.clone());
        let mut core = FakeCore::new();
        core.emulator.ram[0] = 0xAB;
        core.emulator.save[0x1F] = 0xCD;

        shared.update_request(|r| {
            r.sampling = true;
            r.windows[0] = Some(ViewWindow { address: 0x1000, len: 4 });
            r.windows[2] = Some(ViewWindow { address: 0x3010, len: 0x30 });
            r.probes.push(Probe { path: AddressPath::direct(0x1000), len: 2 });
            r.probes.push(Probe { path: AddressPath::direct(0x5000), len: 1 });
        });
        local.service(&mut core, true);

        let mut sample = MonitorSample::default();
        let generation = shared.take_sample(0, &mut sample).expect("a sample");
        assert_eq!(&sample.windows[0].bytes, &[0xAB, 0, 0, 0]);
        assert_eq!(sample.windows[0].valid_len, 4);
        assert!(!sample.windows[1].active);
        // The save region is 0x40 long but only 0x20 bytes are backed.
        assert_eq!(sample.windows[2].valid_len, 0x10);
        assert_eq!(sample.windows[2].bytes[0xF], 0xCD);
        assert_eq!(sample.probe(0), Some(&[0xAB, 0][..]));
        assert_eq!(sample.probe(1), None);
        assert!(shared.take_sample(generation, &mut sample).is_none(), "nothing newer yet");

        // Nothing new: no new sample.
        local.service(&mut core, true);
        assert!(shared.take_sample(generation, &mut sample).is_none());
    }

    #[test]
    fn traces_report_changes_and_discontinuities() {
        let shared = MemoryMonitorShared::new();
        let mut local = MemoryMonitorLocal::new(shared.clone());
        let mut core = FakeCore::new();
        shared.update_request(|r| {
            r.traces.push(TraceSpec { id: 7, path: AddressPath::direct(0x1004), len: 1, decode: ValueDecode::default(), pause_when: Some(TraceCondition::Equals(9)) });
        });
        local.service(&mut core, true);

        core.frame += 1;
        core.emulator.ram[4] = 3;
        let outcome = local.service(&mut core, true);
        assert!(!outcome.pause);

        core.frame += 1;
        core.emulator.ram[4] = 9;
        let outcome = local.service(&mut core, true);
        assert!(outcome.pause, "became 9");

        // A state load changes the value without being reported as a change.
        core.frame += 1;
        core.epoch += 1;
        core.emulator.ram[4] = 1;
        local.service(&mut core, true);

        local.flush_events_now = true;
        local.service(&mut core, true);
        let events = drain(&shared);
        assert_eq!(events, alloc::vec![
            MonitorEvent::Changed { frame: 1, id: 7, old: 0, new: 3 },
            MonitorEvent::Changed { frame: 2, id: 7, old: 3, new: 9 },
            MonitorEvent::PausedByCondition { frame: 2, id: 7 },
            MonitorEvent::Discontinuity { frame: 3 },
        ]);
    }

    #[test]
    fn freezes_write_only_when_changed() {
        let shared = MemoryMonitorShared::new();
        let mut local = MemoryMonitorLocal::new(shared.clone());
        let mut core = FakeCore::new();
        shared.update_request(|r| {
            r.freezes.push(FreezeSpec::new(1, AddressPath::direct(0x1008), &[0x63, 0x00]).unwrap());
            r.freezes.push(FreezeSpec::new(2, AddressPath::direct(0x2000), &[0xFF]).unwrap());
        });
        local.service(&mut core, true);
        assert_eq!(core.writes, alloc::vec![(0x1008, alloc::vec![0x63, 0x00])], "applied once; the read-only region is not written");

        for _ in 0..5 {
            core.frame += 1;
            local.service(&mut core, true);
        }
        assert_eq!(core.writes.len(), 1, "no writes while the value holds");

        core.frame += 1;
        core.emulator.ram[8] = 0x10;
        local.service(&mut core, true);
        assert_eq!(core.writes.len(), 2, "restored once the game changed it");
        assert_eq!(core.emulator.ram[8], 0x63);

        // Suspended during playback.
        core.playing_back = true;
        core.frame += 1;
        core.emulator.ram[8] = 0x20;
        local.service(&mut core, true);
        assert_eq!(core.writes.len(), 2);

        shared.update_request(|r| r.sampling = true);
        core.playing_back = false;
        core.frame += 1;
        local.service(&mut core, true);
        let mut sample = MonitorSample::default();
        shared.take_sample(0, &mut sample).unwrap();
        assert_eq!(sample.freeze_restores, alloc::vec![3, 0]);
        assert_eq!(sample.freeze_ok, alloc::vec![true, false]);
    }

    #[test]
    fn edits_report_old_bytes_and_failures() {
        let shared = MemoryMonitorShared::new();
        let mut local = MemoryMonitorLocal::new(shared.clone());
        let mut core = FakeCore::new();
        core.emulator.ram[0x20] = 0x11;

        shared.push_edit(MemoryEdit { edit_id: 1, path: AddressPath::direct(0x1020), data: alloc::vec![0x22, 0x33] });
        shared.push_edit(MemoryEdit { edit_id: 2, path: AddressPath::direct(0x2000), data: alloc::vec![1] });
        shared.push_edit(MemoryEdit { edit_id: 3, path: AddressPath::direct(0x10FF), data: alloc::vec![1, 2] });
        local.service(&mut core, false);

        assert_eq!(&core.emulator.ram[0x20..0x22], &[0x22, 0x33]);
        let events = drain(&shared);
        assert_eq!(events, alloc::vec![
            MonitorEvent::Written { frame: 0, edit_id: 1, address: 0x1020, old: alloc::vec![0x11, 0x00], new: alloc::vec![0x22, 0x33] },
            MonitorEvent::WriteFailed { edit_id: 2, reason: WriteFailure::ReadOnly },
            MonitorEvent::WriteFailed { edit_id: 3, reason: WriteFailure::Unmapped },
        ]);

        core.playing_back = true;
        shared.push_edit(MemoryEdit { edit_id: 4, path: AddressPath::direct(0x1020), data: alloc::vec![0] });
        local.service(&mut core, false);
        assert_eq!(drain(&shared), alloc::vec![MonitorEvent::WriteFailed { edit_id: 4, reason: WriteFailure::Playback }]);
    }

    #[test]
    fn edits_under_a_trace_are_not_reported_as_changes() {
        let shared = MemoryMonitorShared::new();
        let mut local = MemoryMonitorLocal::new(shared.clone());
        let mut core = FakeCore::new();
        shared.update_request(|r| {
            r.traces.push(TraceSpec { id: 1, path: AddressPath::direct(0x1000), len: 1, decode: ValueDecode::default(), pause_when: None });
        });
        local.service(&mut core, true);
        shared.push_edit(MemoryEdit { edit_id: 1, path: AddressPath::direct(0x1000), data: alloc::vec![5] });
        local.service(&mut core, true);
        core.frame += 1;
        local.service(&mut core, true);
        let events = drain(&shared);
        assert!(events.iter().all(|e| !matches!(e, MonitorEvent::Changed { .. })), "{events:?}");
    }

    #[test]
    fn snapshots_cover_every_region() {
        let shared = MemoryMonitorShared::new();
        let mut local = MemoryMonitorLocal::new(shared.clone());
        let mut core = FakeCore::new();
        core.emulator.io[3] = 0x44;
        let (sender, receiver) = std::sync::mpsc::channel();
        shared.set_snapshot_sender(Some(sender));
        shared.request_snapshot(9);
        local.service(&mut core, true);
        let snapshot = receiver.try_recv().expect("a snapshot");
        assert_eq!(snapshot.job_id, 9);
        assert_eq!(snapshot.bytes.len(), 0x100 + 0x10 + 0x40);
        assert_eq!(snapshot.regions[1].offset, 0x100);
        assert_eq!(snapshot.bytes[0x103], 0x44);
        assert_eq!(snapshot.regions[2].available, 0x20);
    }

    #[test]
    fn request_limits_are_enforced() {
        let mut request = MonitorRequest::default();
        request.windows = [Some(ViewWindow { address: 0, len: 10_000 }); MAX_VIEW_WINDOWS];
        request.freezes = (0..300).map(|i| FreezeSpec::new(i, AddressPath::direct(i), &[0; 64]).unwrap()).collect();
        request.clamp();
        let total: u32 = request.windows.iter().flatten().map(|w| w.len).sum();
        assert_eq!(total as usize, MAX_VIEW_BYTES);
        assert_eq!(request.freezes.len(), MAX_FREEZE_BYTES / 64);
    }
}
