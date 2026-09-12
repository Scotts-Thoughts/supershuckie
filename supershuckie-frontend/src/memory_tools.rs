//! The RAM tools' state on the UI side: which memory the viewers, watches and search want, the
//! latest sample the core thread published for them, and attaching the memory monitor to
//! whichever core is running.

use crate::util::UTF8CString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::collections::{BTreeMap, VecDeque};
use std::time::Instant;
use supershuckie_core::memory_monitor::{AddressPath, FullSnapshot, MemoryMonitorShared, MonitorEvent, MonitorRequest, MonitorSample, Probe, TraceCondition, TraceSpec, ValueDecode, ViewWindow, MAX_PROBES, MAX_TRACES, MAX_VIEW_BYTES, MAX_VIEW_WINDOWS};
use supershuckie_core::ThreadedSuperShuckieCore;
use supershuckie_memory_tools::search::{Comparison, MemorySnapshot, ScanControl, Search, SearchError, SearchRow, SearchSettings, SnapshotRegion};
use supershuckie_memory_tools::watch::{Watch, WatchCondition, WatchFile, WATCH_FILE_VERSION};
use supershuckie_memory_tools::{format_value, CharTable, RegionInfo, ValueType};
use supershuckie_replay_recorder::replay_file::{ReplayConsoleType, ReplayHeaderBlake3Hash};

/// Viewer windows that can be open at once.
pub const MAX_VIEWERS: usize = MAX_VIEW_WINDOWS;

/// Which game the tools' state belongs to.
#[derive(Copy, Clone, PartialEq, Debug)]
pub struct GameIdentity {
    pub console_type: ReplayConsoleType,
    pub rom_checksum: ReplayHeaderBlake3Hash
}

/// A viewer window's bytes from the latest sample.
pub struct ViewerSample<'a> {
    /// Increases with every new sample.
    pub generation: u64,
    /// Emulated frame the bytes were read after.
    pub frame: u64,
    /// Address of the first byte.
    pub address: u32,
    /// The bytes; only the first `valid_len` are mapped.
    pub bytes: &'a [u8],
    pub valid_len: u32
}

/// What the search worker is doing and what it found.
#[derive(Clone, Debug, Default)]
pub struct SearchStatus {
    /// A search has been started (and not reset or invalidated).
    pub active: bool,
    /// A scan is waiting for its snapshot or running.
    pub busy: bool,
    /// 0-1000 while busy.
    pub progress: u32,
    pub result_count: u64,
    /// Scans in the current state.
    pub steps: u32,
    pub can_undo: bool,
    pub can_redo: bool,
    /// Frame of the last scan.
    pub frame: u64,
    /// Memory was replaced wholesale (state load, seek) since the scan before the last one.
    pub state_changed: bool,
    /// The last operation's problem, if any.
    pub message: Option<String>,
    /// Changes whenever the results change.
    pub generation: u64,
    /// The active search's settings.
    pub settings: Option<SearchSettings>
}

enum SearchJob {
    Scan {
        job_id: u64,
        settings: Option<SearchSettings>,
        comparison: Comparison,
        game: GameIdentity
    },
    Undo,
    Redo,
    Reset { message: Option<String> }
}

/// State shared by the UI side and the search worker thread.
struct SearchShared {
    search: Mutex<Option<(Search, GameIdentity)>>,
    status: Mutex<SearchStatus>,
    control: ScanControl,
    busy: AtomicBool,
    generation: AtomicU64
}

impl SearchShared {
    fn update_status(&self, message: Option<String>, state_changed: bool) {
        let search = self.search.lock().unwrap_or_else(|e| e.into_inner());
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        *status = match search.as_ref() {
            Some((search, _)) => SearchStatus {
                active: true,
                busy: false,
                progress: 1000,
                result_count: search.count(),
                steps: search.steps(),
                can_undo: search.can_undo(),
                can_redo: search.can_redo(),
                frame: search.last_scan().0,
                state_changed,
                message,
                generation,
                settings: Some(search.settings().clone())
            },
            None => SearchStatus { message, generation, ..Default::default() }
        };
    }
}

fn convert_snapshot(snapshot: FullSnapshot) -> MemorySnapshot {
    MemorySnapshot {
        frame: snapshot.frame,
        epoch: snapshot.state_epoch,
        layout: snapshot.regions.iter().map(|r| SnapshotRegion {
            region: RegionInfo {
                name: r.info.name.to_owned(),
                short_name: r.info.short_name.to_owned(),
                base: r.info.base_address,
                len: r.info.len,
                big_endian: r.info.default_big_endian,
                writable: r.info.writable
            },
            offset: r.offset,
            available: r.available
        }).collect(),
        bytes: snapshot.bytes
    }
}

fn search_worker(jobs: Receiver<SearchJob>, snapshots: Receiver<FullSnapshot>, shared: Arc<SearchShared>, monitor: Arc<MemoryMonitorShared>) {
    let recycle = |released: Vec<MemorySnapshot>| {
        for snapshot in released {
            monitor.recycle_snapshot_buffer(snapshot.bytes);
        }
    };

    while let Ok(job) = jobs.recv() {
        match job {
            SearchJob::Scan { job_id, settings, comparison, game } => {
                // Wait for this scan's snapshot (taken at the core thread's next frame boundary).
                let snapshot = loop {
                    match snapshots.recv_timeout(Duration::from_millis(50)) {
                        Ok(s) if s.job_id == job_id => break Some(s),
                        Ok(stale) => monitor.recycle_snapshot_buffer(stale.bytes),
                        Err(RecvTimeoutError::Timeout) if shared.control.cancel.load(Ordering::Relaxed) => break None,
                        Err(RecvTimeoutError::Timeout) => continue,
                        Err(RecvTimeoutError::Disconnected) => return
                    }
                };
                let Some(snapshot) = snapshot else {
                    shared.busy.store(false, Ordering::Relaxed);
                    shared.update_status(Some("Scan cancelled".to_owned()), false);
                    continue
                };
                let snapshot = convert_snapshot(snapshot);

                let mut search_slot = shared.search.lock().unwrap_or_else(|e| e.into_inner());
                let existing = search_slot.take();
                drop(search_slot);

                let mut message = None;
                let mut state_changed = false;
                let result = match (settings, existing) {
                    (Some(settings), existing) => {
                        if let Some((old, _)) = existing {
                            recycle(old.into_snapshots());
                        }
                        let bytes_for_error = snapshot.bytes.len();
                        match Search::new(settings, &comparison, snapshot, &shared.control) {
                            Ok(search) => Some((search, game)),
                            Err(e) => {
                                let _ = bytes_for_error;
                                message = Some(match e { SearchError::Cancelled => "Scan cancelled".to_owned(), e => e.to_string() });
                                None
                            }
                        }
                    }
                    (None, Some((mut search, search_game))) if search_game == game => {
                        let epoch_before = search.last_scan().1;
                        state_changed = snapshot.epoch != epoch_before;
                        match search.refine(&comparison, snapshot, &shared.control) {
                            Ok(released) => recycle(released),
                            Err((e, snapshot)) => {
                                recycle(vec![snapshot]);
                                message = Some(match e { SearchError::Cancelled => "Scan cancelled".to_owned(), e => e.to_string() });
                            }
                        }
                        Some((search, search_game))
                    }
                    (None, Some((search, _))) => {
                        recycle(search.into_snapshots());
                        recycle(vec![snapshot]);
                        message = Some("The game changed; start a new search".to_owned());
                        None
                    }
                    (None, None) => {
                        recycle(vec![snapshot]);
                        message = Some("No search to refine; start a new search".to_owned());
                        None
                    }
                };

                *shared.search.lock().unwrap_or_else(|e| e.into_inner()) = result;
                shared.busy.store(false, Ordering::Relaxed);
                shared.update_status(message, state_changed);
            }
            SearchJob::Undo | SearchJob::Redo => {
                let mut slot = shared.search.lock().unwrap_or_else(|e| e.into_inner());
                if let Some((search, _)) = slot.as_mut() {
                    if matches!(job, SearchJob::Undo) { search.undo(); } else { search.redo(); }
                }
                drop(slot);
                shared.update_status(None, false);
            }
            SearchJob::Reset { message } => {
                let taken = shared.search.lock().unwrap_or_else(|e| e.into_inner()).take();
                if let Some((search, _)) = taken {
                    recycle(search.into_snapshots());
                }
                shared.update_status(message, false);
            }
        }
    }
}

/// Who a probe in the monitor request belongs to.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ProbeOwner {
    SearchRow(usize),
    Watch(u32)
}

/// Change log lines kept.
pub const CHANGE_LOG_CAPACITY: usize = 10_000;

/// What a change log line is about.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LogKind {
    /// A traced watch changed.
    Changed,
    /// Memory was replaced wholesale (state load, reset, seek).
    Discontinuity,
    /// A watch's condition paused emulation.
    Paused,
    /// Memory was edited by the user.
    Edited,
    /// An edit was refused.
    EditFailed
}

/// A change log line.
#[derive(Clone, Debug)]
pub struct LogEntry {
    pub frame: u64,
    pub watch_id: u32,
    pub kind: LogKind,
    pub text: String
}

/// What the tools know about a watch between samples.
#[derive(Clone, Debug, Default)]
struct WatchRuntime {
    value: Option<Vec<u8>>,
    previous: Option<Vec<u8>>,
    last_change_frame: Option<u64>,
    resolved: Option<u32>
}

/// A watch's latest value, for display.
#[derive(Clone, Debug)]
pub struct WatchValue {
    pub id: u32,
    /// The value's bytes, if they could be read.
    pub value: Option<Vec<u8>>,
    pub text: String,
    pub previous_text: String,
    /// The address the value was read at (after pointers).
    pub resolved_address: Option<u32>,
    /// Frames since the value last changed, if it has been seen changing.
    pub frames_since_change: Option<u64>
}

/// Value bytes of a traced watch packed into an event (see `MonitorEvent::Changed`).
fn unpack_event_value(packed: u64, len: usize) -> Option<Vec<u8>> {
    (len <= 8).then(|| packed.to_le_bytes()[..len].to_vec())
}

fn trace_condition(condition: WatchCondition) -> TraceCondition {
    match condition {
        WatchCondition::Changes => TraceCondition::Changes,
        WatchCondition::Equals(v) => TraceCondition::Equals(v),
        WatchCondition::NotEquals(v) => TraceCondition::NotEquals(v),
        WatchCondition::GreaterThan(v) => TraceCondition::GreaterThan(v),
        WatchCondition::LessThan(v) => TraceCondition::LessThan(v),
        WatchCondition::IncreasedBy(v) => TraceCondition::IncreasedBy(v),
        WatchCondition::DecreasedBy(v) => TraceCondition::DecreasedBy(v)
    }
}

fn watch_path(watch: &Watch) -> AddressPath {
    AddressPath::with_derefs(watch.address.base, &watch.address.offsets).unwrap_or(AddressPath::direct(watch.address.base))
}

pub struct MemoryTools {
    shared: Arc<MemoryMonitorShared>,
    attached: bool,

    regions: Vec<RegionInfo>,
    region_names: Vec<(UTF8CString, UTF8CString)>,
    regions_generation: u64,
    game: Option<GameIdentity>,

    refresh_hz: u8,
    viewers: [Option<ViewWindow>; MAX_VIEWERS],

    sample: MonitorSample,
    sample_generation: u64,

    tables_dir: PathBuf,
    tables: Vec<CharTable>,
    table_names: Vec<UTF8CString>,
    table_errors: Vec<String>,

    playback: bool,

    search: Arc<SearchShared>,
    search_jobs: Sender<SearchJob>,
    next_search_job: u64,
    /// The search is waiting for a snapshot or scanning (as last seen by the UI side).
    search_busy: bool,
    /// Emulation was paused for the scan and is resumed when it ends.
    paused_for_search: bool,
    /// Rows of the search results on screen, and their probes.
    search_visible: (u64, u32),
    search_visible_rows: Vec<SearchRow>,
    search_rows_generation: u64,

    probe_owners: Vec<ProbeOwner>,
    /// Request generation from which the current probes' values are in samples.
    probes_request_generation: u64,

    watches: Vec<Watch>,
    watch_runtime: BTreeMap<u32, WatchRuntime>,
    watch_file: Option<PathBuf>,
    watches_dirty_at: Option<Instant>,
    watch_generation: u64,
    watch_problems: Vec<String>,
    visible_watches: Vec<u32>,

    events: Vec<MonitorEvent>,
    log: VecDeque<LogEntry>,
    log_dropped: u64
}

impl MemoryTools {
    /// Default samples per second while the game runs.
    pub const DEFAULT_REFRESH_HZ: u8 = 30;

    pub fn new(tables_dir: PathBuf) -> Self {
        let shared = MemoryMonitorShared::new();
        let search = Arc::new(SearchShared {
            search: Mutex::new(None),
            status: Mutex::new(SearchStatus::default()),
            control: ScanControl::default(),
            busy: AtomicBool::new(false),
            generation: AtomicU64::new(0)
        });
        let (search_jobs, jobs_receiver) = channel();
        let (snapshot_sender, snapshot_receiver) = channel();
        shared.set_snapshot_sender(Some(snapshot_sender));
        {
            let search = search.clone();
            let shared = shared.clone();
            let _ = std::thread::Builder::new()
                .name("RamSearchWorker".to_owned())
                .spawn(move || search_worker(jobs_receiver, snapshot_receiver, search, shared));
        }

        let mut tools = Self {
            shared,
            attached: false,
            regions: Vec::new(),
            region_names: Vec::new(),
            regions_generation: 1,
            game: None,
            refresh_hz: Self::DEFAULT_REFRESH_HZ,
            viewers: [None; MAX_VIEWERS],
            sample: MonitorSample::default(),
            sample_generation: 0,
            tables_dir,
            tables: Vec::new(),
            table_names: Vec::new(),
            table_errors: Vec::new(),
            playback: false,
            search,
            search_jobs,
            next_search_job: 1,
            search_busy: false,
            paused_for_search: false,
            search_visible: (0, 0),
            search_visible_rows: Vec::new(),
            search_rows_generation: 0,
            probe_owners: Vec::new(),
            probes_request_generation: 0,
            watches: Vec::new(),
            watch_runtime: BTreeMap::new(),
            watch_file: None,
            watches_dirty_at: None,
            watch_generation: 1,
            watch_problems: Vec::new(),
            visible_watches: Vec::new(),
            events: Vec::new(),
            log: VecDeque::new(),
            log_dropped: 0
        };
        tools.reload_tables();
        tools
    }

    /// Whether the monitor needs to be attached to the core at all.
    fn needed(&self, request: &MonitorRequest) -> bool {
        !request.is_idle() || self.search_busy
    }

    /// A new core took over: learn its memory layout and hand it the monitor.
    ///
    /// Returns whether the game changed (a different ROM or console), in which case state that
    /// belongs to the old game must be dropped.
    pub fn core_switched(&mut self, core: &ThreadedSuperShuckieCore, watch_file: Option<PathBuf>) -> bool {
        let regions: Vec<RegionInfo> = core.memory_regions().iter().map(|r| RegionInfo {
            name: r.name.to_owned(),
            short_name: r.short_name.to_owned(),
            base: r.base_address,
            len: r.len,
            big_endian: r.default_big_endian,
            writable: r.writable
        }).collect();
        let game = core.console_type().map(|console_type| GameIdentity { console_type, rom_checksum: *core.rom_checksum() });
        let game_changed = game != self.game;
        self.game = game;

        if regions != self.regions || game_changed {
            self.region_names = regions.iter().map(|r| (UTF8CString::from_str(&r.name), UTF8CString::from_str(&r.short_name))).collect();
            self.regions = regions;
            self.regions_generation += 1;
        }

        if game_changed && self.search.status.lock().unwrap_or_else(|e| e.into_inner()).active {
            let _ = self.search_jobs.send(SearchJob::Reset { message: Some("The game changed; start a new search".to_owned()) });
            self.search_visible_rows.clear();
        }
        if game_changed {
            self.save_watches();
            self.load_watches(if self.game.is_some() { watch_file } else { None });
        }

        // The old core took its half of the monitor with it.
        self.attached = false;
        self.push_request(core);
        game_changed
    }

    /// Call regularly (every frontend tick): picks up new samples.
    pub fn tick(&mut self, core: &ThreadedSuperShuckieCore, playing_back: bool) {
        let mut push = false;
        if playing_back != self.playback {
            self.playback = playing_back;
            push = true;
        }
        if self.attached && let Some(generation) = self.shared.take_sample(self.sample_generation, &mut self.sample) {
            self.sample_generation = generation;
            self.absorb_sample();
        }
        if self.attached {
            self.absorb_events();
        }
        if self.watches_dirty_at.is_some_and(|at| at.elapsed() >= Duration::from_secs(1)) {
            self.save_watches();
        }

        // A scan finished: resume emulation if it was paused for it, re-read the rows on screen,
        // and detach the monitor if nothing else needs it.
        if self.search_busy && !self.search.busy.load(Ordering::Relaxed) {
            self.search_busy = false;
            if self.paused_for_search {
                self.paused_for_search = false;
                core.start();
            }
            push = true;
        }
        let generation = self.search.generation.load(Ordering::Relaxed);
        if generation != self.search_rows_generation {
            self.search_rows_generation = generation;
            self.refresh_search_rows();
            push = true;
        }

        if push {
            self.push_request(core);
        }
    }

    /// Rebuild the request from everything the tools want and attach or detach the monitor.
    fn push_request(&mut self, core: &ThreadedSuperShuckieCore) {
        let interval = std::time::Duration::from_micros(1_000_000 / self.refresh_hz.max(1) as u64);
        let viewers = self.viewers;
        let playback = self.playback;

        let mut probes = Vec::new();
        self.probe_owners.clear();
        for id in &self.visible_watches {
            if probes.len() >= MAX_PROBES {
                break
            }
            if let Some(watch) = self.watches.iter().find(|w| w.id == *id) {
                probes.push(Probe { path: watch_path(watch), len: watch.format.size });
                self.probe_owners.push(ProbeOwner::Watch(watch.id));
            }
        }
        for (index, row) in self.search_visible_rows.iter().enumerate() {
            if probes.len() >= MAX_PROBES {
                break
            }
            probes.push(Probe { path: AddressPath::direct(row.address), len: row.previous.len() as u8 });
            self.probe_owners.push(ProbeOwner::SearchRow(index));
        }

        let traces: Vec<TraceSpec> = self.watches.iter().filter(|w| w.is_traced()).take(MAX_TRACES).map(|w| TraceSpec {
            id: w.id,
            path: watch_path(w),
            len: w.format.size,
            decode: ValueDecode { big_endian: w.format.big_endian, signed: w.format.ty.is_signed(), bcd: w.format.ty == ValueType::Bcd },
            pause_when: w.pause_when.map(trace_condition)
        }).collect();

        let request = self.shared.update_request(|request| {
            request.sampling = viewers.iter().any(Option::is_some) || !probes.is_empty();
            request.interval = interval;
            request.windows = viewers;
            request.probes = probes;
            request.traces = traces;
            request.freezes_suspended = playback;
            request.clone()
        });
        self.probes_request_generation = self.shared.request_generation();

        let needed = self.game.is_some() && self.needed(&request);
        if needed != self.attached {
            core.set_memory_monitor(needed.then(|| self.shared.clone()));
            self.attached = needed;
        }
        else if needed {
            core.wake();
        }
    }

    /// The running game's memory regions.
    #[inline]
    pub fn regions(&self) -> &[RegionInfo] {
        &self.regions
    }

    /// `(name, short name)` of each region as C strings, valid until the regions change.
    #[inline]
    pub fn region_names(&self) -> &[(UTF8CString, UTF8CString)] {
        &self.region_names
    }

    /// Changes whenever the region list does.
    #[inline]
    pub fn regions_generation(&self) -> u64 {
        self.regions_generation
    }

    /// The game the tools are looking at, if any.
    #[inline]
    pub fn game(&self) -> Option<GameIdentity> {
        self.game
    }

    /// Samples per second while the game runs.
    #[inline]
    pub fn refresh_hz(&self) -> u8 {
        self.refresh_hz
    }

    pub fn set_refresh_hz(&mut self, core: &ThreadedSuperShuckieCore, hz: u8) {
        let hz = hz.clamp(1, 120);
        if hz != self.refresh_hz {
            self.refresh_hz = hz;
            self.push_request(core);
        }
    }

    /// Show `window` in viewer slot `viewer` (`None` when the viewer is closed or hidden).
    pub fn set_viewer_window(&mut self, core: &ThreadedSuperShuckieCore, viewer: usize, window: Option<(u32, u32)>) {
        let Some(slot) = self.viewers.get_mut(viewer) else {
            return
        };
        let window = window.map(|(address, len)| ViewWindow { address, len: len.min(MAX_VIEW_BYTES as u32) });
        if *slot != window {
            *slot = window;
            self.push_request(core);
        }
    }

    /// Viewer slot `viewer`'s bytes from the latest sample, if it is newer than `last_generation`.
    pub fn read_viewer(&self, viewer: usize, last_generation: u64) -> Option<ViewerSample<'_>> {
        if self.sample_generation <= last_generation {
            return None
        }
        let window = self.sample.windows.get(viewer)?;
        if !window.active {
            return None
        }
        Some(ViewerSample {
            generation: self.sample_generation,
            frame: self.sample.frame,
            address: window.address,
            bytes: &window.bytes,
            valid_len: window.valid_len
        })
    }

    /// The latest sample.
    #[inline]
    pub fn sample(&self) -> &MonitorSample {
        &self.sample
    }

    /// The search worker's status.
    pub fn search_status(&self) -> SearchStatus {
        let mut status = self.search.status.lock().unwrap_or_else(|e| e.into_inner()).clone();
        status.busy = self.search.busy.load(Ordering::Relaxed);
        if status.busy {
            status.progress = self.search.control.progress.load(Ordering::Relaxed);
        }
        status
    }

    /// Start a new search (`settings` given) or refine the active one with `comparison`, at the
    /// next frame boundary. `pause` pauses emulation until the scan is done.
    pub fn search_scan(&mut self, core: &ThreadedSuperShuckieCore, settings: Option<SearchSettings>, comparison: Comparison, pause: bool) -> Result<(), String> {
        let Some(game) = self.game else {
            return Err("No game is loaded".to_owned())
        };
        if self.search.busy.load(Ordering::Relaxed) {
            return Err("A scan is already running".to_owned())
        }
        if settings.is_none() && !self.search_status().active {
            return Err("There is no search to refine; start a new search".to_owned())
        }

        let job_id = self.next_search_job;
        self.next_search_job += 1;
        self.search.control.cancel.store(false, Ordering::Relaxed);
        self.search.control.progress.store(0, Ordering::Relaxed);
        self.search.busy.store(true, Ordering::Relaxed);
        self.search_busy = true;
        self.search_jobs.send(SearchJob::Scan { job_id, settings, comparison, game }).map_err(|_| "The search worker stopped".to_owned())?;

        if pause && !core.is_paused() {
            core.pause();
            self.paused_for_search = true;
        }
        self.push_request(core);
        self.shared.request_snapshot(job_id);
        core.wake();
        Ok(())
    }

    /// Stop a scan that has not finished.
    pub fn search_cancel(&self) {
        self.search.control.cancel.store(true, Ordering::Relaxed);
    }

    pub fn search_undo(&self) {
        if !self.search.busy.load(Ordering::Relaxed) {
            let _ = self.search_jobs.send(SearchJob::Undo);
        }
    }

    pub fn search_redo(&self) {
        if !self.search.busy.load(Ordering::Relaxed) {
            let _ = self.search_jobs.send(SearchJob::Redo);
        }
    }

    /// Forget the search.
    pub fn search_reset(&mut self, core: &ThreadedSuperShuckieCore) {
        self.search_cancel();
        let _ = self.search_jobs.send(SearchJob::Reset { message: None });
        self.search_visible_rows.clear();
        self.push_request(core);
    }

    /// Up to `count` results starting with the `offset`th. Empty while a scan is running.
    pub fn search_results(&self, offset: u64, count: usize) -> Vec<SearchRow> {
        // The worker only holds this briefly; during a scan the search is taken out of it.
        let guard = self.search.search.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().map(|(search, _)| search.results(offset, count)).unwrap_or_default()
    }

    /// Set which result rows are on screen, so their current values are sampled.
    pub fn search_set_visible_rows(&mut self, core: &ThreadedSuperShuckieCore, offset: u64, count: u32) {
        let count = count.min(MAX_PROBES as u32);
        if (offset, count) == self.search_visible {
            return
        }
        self.search_visible = (offset, count);
        self.refresh_search_rows();
        self.push_request(core);
    }

    fn refresh_search_rows(&mut self) {
        let (offset, count) = self.search_visible;
        self.search_visible_rows = if count == 0 { Vec::new() } else { self.search_results(offset, count as usize) };
    }

    /// The first visible result row and the current value of each visible row (`None` where it
    /// could not be read or is not sampled yet).
    pub fn search_visible_values(&self) -> (u64, Vec<Option<&[u8]>>) {
        let mut values = vec![None; self.search_visible_rows.len()];
        if self.sample.request_generation >= self.probes_request_generation {
            for (probe, owner) in self.probe_owners.iter().enumerate() {
                if let ProbeOwner::SearchRow(row) = owner && let Some(slot) = values.get_mut(*row) {
                    *slot = self.sample.probe(probe);
                }
            }
        }
        (self.search_visible.0, values)
    }

    /// Changes whenever the sample (and so current values) changes.
    #[inline]
    pub fn sample_generation(&self) -> u64 {
        self.sample_generation
    }

    // -----------------------------------------------------------------------------------------
    // Watches

    fn table_for(&self, watch: &Watch) -> &CharTable {
        self.tables.iter().find(|t| t.name() == watch.table).unwrap_or(&self.tables[0])
    }

    fn format_watch_value(&self, watch: &Watch, bytes: &[u8]) -> String {
        format_value(&watch.format, watch.display, bytes, self.table_for(watch))
    }

    /// Update watch values from the latest sample.
    fn absorb_sample(&mut self) {
        if self.sample.request_generation < self.probes_request_generation {
            return
        }
        for (probe, owner) in self.probe_owners.iter().enumerate() {
            let ProbeOwner::Watch(id) = owner else { continue };
            let traced = self.watches.iter().any(|w| w.id == *id && w.is_traced());
            let value = self.sample.probe(probe).map(|b| b.to_vec());
            let runtime = self.watch_runtime.entry(*id).or_default();
            runtime.resolved = self.sample.probe_ok.get(probe).copied().unwrap_or(false).then(|| self.sample.probe_addresses[probe]);
            if value.is_some() && runtime.value != value {
                if runtime.value.is_some() {
                    runtime.previous = runtime.value.take();
                    if !traced {
                        runtime.last_change_frame = Some(self.sample.frame);
                    }
                }
                runtime.value = value;
            }
        }
    }

    fn push_log(&mut self, entry: LogEntry) {
        if self.log.len() >= CHANGE_LOG_CAPACITY {
            self.log.pop_front();
            self.log_dropped += 1;
        }
        self.log.push_back(entry);
    }

    /// Turn the core thread's events into log lines.
    fn absorb_events(&mut self) {
        let dropped = self.shared.drain_events(&mut self.events);
        if dropped > 0 {
            self.log_dropped += dropped;
        }
        let events = core::mem::take(&mut self.events);
        for event in &events {
            match event {
                MonitorEvent::Changed { frame, id, old, new } => {
                    let Some(watch) = self.watches.iter().find(|w| w.id == *id) else { continue };
                    let len = watch.format.len();
                    let describe = |packed: u64| unpack_event_value(packed, len).map(|b| self.format_watch_value(watch, &b)).unwrap_or_else(|| "(changed)".to_owned());
                    let text = format!("{}: {} → {}", watch.label, describe(*old), describe(*new));
                    let runtime = self.watch_runtime.entry(*id).or_default();
                    runtime.last_change_frame = Some(*frame);
                    self.push_log(LogEntry { frame: *frame, watch_id: *id, kind: LogKind::Changed, text });
                }
                MonitorEvent::Discontinuity { frame } => {
                    self.push_log(LogEntry { frame: *frame, watch_id: 0, kind: LogKind::Discontinuity, text: "memory replaced (state loaded, reset or seeked)".to_owned() });
                }
                MonitorEvent::PausedByCondition { frame, id } => {
                    let label = self.watches.iter().find(|w| w.id == *id).map(|w| w.label.clone()).unwrap_or_default();
                    self.push_log(LogEntry { frame: *frame, watch_id: *id, kind: LogKind::Paused, text: format!("paused: {label}") });
                }
                MonitorEvent::Written { .. } | MonitorEvent::WriteFailed { .. } => {
                    self.absorb_write_event(event);
                }
            }
        }
        self.events = events;
        self.events.clear();
    }

    /// Edits are handled with editing (see below); nothing to log for them otherwise.
    fn absorb_write_event(&mut self, _event: &MonitorEvent) {}

    /// Load the watch list at `path` (none: clear it).
    fn load_watches(&mut self, path: Option<PathBuf>) {
        self.watches.clear();
        self.watch_runtime.clear();
        self.watch_problems.clear();
        self.watches_dirty_at = None;
        self.watch_file = path;
        self.watch_generation += 1;
        let Some(path) = self.watch_file.as_ref() else {
            return
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return
        };
        match WatchFile::parse(&text) {
            Ok((file, problems)) => {
                self.watch_problems = problems;
                if let Some(game) = self.game && !file.rom_checksum.is_empty() && file.rom_checksum != blake3_hex(&game.rom_checksum) {
                    self.watch_problems.push("these watches were made for a different version of the ROM".to_owned());
                }
                self.watches = file.watches;
            }
            Err(e) => self.watch_problems.push(format!("{}: {e}", path.display()))
        }
    }

    /// Write the watch list now (if it has a file).
    pub fn save_watches(&mut self) {
        self.watches_dirty_at = None;
        let Some(path) = self.watch_file.as_ref() else {
            return
        };
        let file = self.watch_file_contents();
        let temp = path.with_extension("json.tmp");
        if std::fs::write(&temp, file.to_json()).is_ok() {
            let _ = std::fs::rename(&temp, path);
        }
    }

    fn watch_file_contents(&self) -> WatchFile {
        WatchFile {
            version: WATCH_FILE_VERSION,
            console: self.game.map(|g| format!("{:?}", g.console_type)).unwrap_or_default(),
            rom_checksum: self.game.map(|g| blake3_hex(&g.rom_checksum)).unwrap_or_default(),
            watches: self.watches.clone()
        }
    }

    fn watches_changed(&mut self, core: &ThreadedSuperShuckieCore) {
        self.watch_generation += 1;
        self.watches_dirty_at.get_or_insert_with(Instant::now);
        self.push_request(core);
    }

    /// The watch list.
    #[inline]
    pub fn watches(&self) -> &[Watch] {
        &self.watches
    }

    /// Changes whenever the watch list does.
    #[inline]
    pub fn watch_generation(&self) -> u64 {
        self.watch_generation
    }

    /// Problems found loading or importing watches, cleared when taken.
    pub fn take_watch_problems(&mut self) -> Vec<String> {
        core::mem::take(&mut self.watch_problems)
    }

    /// Add `watch` (id 0 or unknown) or replace the watch with its id. Returns its id.
    pub fn upsert_watch(&mut self, core: &ThreadedSuperShuckieCore, mut watch: Watch) -> Result<u32, String> {
        if self.game.is_none() {
            return Err("No game is loaded".to_owned())
        }
        watch.validate()?;
        let existing = self.watches.iter().position(|w| w.id == watch.id && watch.id != 0);
        let traced = self.watches.iter().enumerate().filter(|(i, w)| Some(*i) != existing && w.is_traced()).count();
        if watch.is_traced() && traced >= MAX_TRACES {
            return Err(format!("At most {MAX_TRACES} watches can log changes or pause emulation at once"))
        }
        match existing {
            Some(index) => {
                let old = &self.watches[index];
                if old.address != watch.address || old.format != watch.format {
                    self.watch_runtime.remove(&watch.id);
                }
                self.watches[index] = watch.clone();
            }
            None => {
                watch.id = self.watches.iter().map(|w| w.id).max().unwrap_or(0) + 1;
                self.watches.push(watch.clone());
            }
        }
        self.watches_changed(core);
        Ok(watch.id)
    }

    pub fn remove_watch(&mut self, core: &ThreadedSuperShuckieCore, id: u32) {
        let before = self.watches.len();
        self.watches.retain(|w| w.id != id);
        if self.watches.len() != before {
            self.watch_runtime.remove(&id);
            self.visible_watches.retain(|v| *v != id);
            self.watches_changed(core);
        }
    }

    /// Which watches are on screen, so their values are sampled.
    pub fn set_visible_watches(&mut self, core: &ThreadedSuperShuckieCore, ids: &[u32]) {
        if ids != self.visible_watches.as_slice() {
            self.visible_watches = ids.to_vec();
            self.push_request(core);
        }
    }

    /// The latest values of the watches on screen.
    pub fn watch_values(&self) -> Vec<WatchValue> {
        self.visible_watches.iter().filter_map(|id| {
            let watch = self.watches.iter().find(|w| w.id == *id)?;
            let runtime = self.watch_runtime.get(id);
            let value = runtime.and_then(|r| r.value.clone());
            Some(WatchValue {
                id: *id,
                text: value.as_ref().map(|v| self.format_watch_value(watch, v)).unwrap_or_else(|| "—".to_owned()),
                previous_text: runtime.and_then(|r| r.previous.as_ref()).map(|v| self.format_watch_value(watch, v)).unwrap_or_default(),
                value,
                resolved_address: runtime.and_then(|r| r.resolved),
                frames_since_change: runtime.and_then(|r| r.last_change_frame).map(|f| self.sample.frame.saturating_sub(f))
            })
        }).collect()
    }

    /// Take up to `max` change log lines (oldest first) and how many were dropped meanwhile.
    pub fn drain_log(&mut self, max: usize) -> (Vec<LogEntry>, u64) {
        let count = max.min(self.log.len());
        (self.log.drain(..count).collect(), core::mem::take(&mut self.log_dropped))
    }

    /// Replace the watch list with the one in `path` (freezes load inactive).
    pub fn import_watches(&mut self, core: &ThreadedSuperShuckieCore, path: &std::path::Path, replace: bool) -> Result<usize, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("Can't read {}: {e}", path.display()))?;
        let (file, problems) = WatchFile::parse(&text)?;
        if replace {
            self.watches.clear();
            self.watch_runtime.clear();
        }
        let mut next_id = self.watches.iter().map(|w| w.id).max().unwrap_or(0) + 1;
        let mut added = 0;
        for mut watch in file.watches {
            watch.id = next_id;
            next_id += 1;
            if watch.is_traced() && self.watches.iter().filter(|w| w.is_traced()).count() >= MAX_TRACES {
                watch.trace = false;
                watch.pause_when = None;
            }
            self.watches.push(watch);
            added += 1;
        }
        self.watch_problems = problems;
        self.watches_changed(core);
        Ok(added)
    }

    pub fn export_watches(&self, path: &std::path::Path) -> Result<(), String> {
        std::fs::write(path, self.watch_file_contents().to_json()).map_err(|e| format!("Can't write {}: {e}", path.display()))
    }

    /// Where `.tbl` character tables are loaded from.
    #[inline]
    pub fn tables_dir(&self) -> &PathBuf {
        &self.tables_dir
    }

    /// Load the character tables again: ASCII, then every `.tbl` file in the tables directory.
    pub fn reload_tables(&mut self) {
        self.tables = vec![CharTable::ascii()];
        self.table_errors.clear();
        let _ = std::fs::create_dir_all(&self.tables_dir);
        let mut files: Vec<PathBuf> = std::fs::read_dir(&self.tables_dir)
            .map(|dir| dir.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("tbl"))).collect())
            .unwrap_or_default();
        files.sort();
        for path in files {
            let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("table").to_owned();
            let parsed = std::fs::read(&path)
                .map_err(|e| e.to_string())
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .and_then(|text| CharTable::parse_tbl(&name, &text));
            match parsed {
                Ok(table) => self.tables.push(table),
                Err(e) => self.table_errors.push(format!("{}: {e}", path.display()))
            }
        }
        self.table_names = self.tables.iter().map(|t| UTF8CString::from_str(t.name())).collect();
    }

    /// Loaded character tables (index 0 is ASCII).
    #[inline]
    pub fn tables(&self) -> &[CharTable] {
        &self.tables
    }

    /// Table names as C strings.
    #[inline]
    pub fn table_names(&self) -> &[UTF8CString] {
        &self.table_names
    }

    /// Problems found by the last [`reload_tables`](Self::reload_tables).
    #[inline]
    pub fn table_errors(&self) -> &[String] {
        &self.table_errors
    }

    /// Table `index`, or ASCII if there is no such table.
    #[inline]
    pub fn table(&self, index: usize) -> &CharTable {
        self.tables.get(index).unwrap_or(&self.tables[0])
    }
}

fn blake3_hex(hash: &ReplayHeaderBlake3Hash) -> String {
    hash.iter().map(|b| format!("{b:02x}")).collect()
}
