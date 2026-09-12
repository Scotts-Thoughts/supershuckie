//! The RAM tools' state on the UI side: which memory the viewers, watches and search want, the
//! latest sample the core thread published for them, and attaching the memory monitor to
//! whichever core is running.

use crate::util::UTF8CString;
use std::path::PathBuf;
use std::sync::Arc;
use supershuckie_core::memory_monitor::{MemoryMonitorShared, MonitorRequest, MonitorSample, ViewWindow, MAX_VIEW_BYTES, MAX_VIEW_WINDOWS};
use supershuckie_core::ThreadedSuperShuckieCore;
use supershuckie_memory_tools::{CharTable, RegionInfo};
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

    playback: bool
}

impl MemoryTools {
    /// Default samples per second while the game runs.
    pub const DEFAULT_REFRESH_HZ: u8 = 30;

    pub fn new(tables_dir: PathBuf) -> Self {
        let mut tools = Self {
            shared: MemoryMonitorShared::new(),
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
            playback: false
        };
        tools.reload_tables();
        tools
    }

    /// Whether the monitor needs to be attached to the core at all.
    fn needed(&self, request: &MonitorRequest) -> bool {
        !request.is_idle()
    }

    /// A new core took over: learn its memory layout and hand it the monitor.
    ///
    /// Returns whether the game changed (a different ROM or console), in which case state that
    /// belongs to the old game must be dropped.
    pub fn core_switched(&mut self, core: &ThreadedSuperShuckieCore) -> bool {
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

        // The old core took its half of the monitor with it.
        self.attached = false;
        self.push_request(core);
        game_changed
    }

    /// Call regularly (every frontend tick): picks up new samples.
    pub fn tick(&mut self, core: &ThreadedSuperShuckieCore, playing_back: bool) {
        if playing_back != self.playback {
            self.playback = playing_back;
            self.push_request(core);
        }
        if self.attached && let Some(generation) = self.shared.take_sample(self.sample_generation, &mut self.sample) {
            self.sample_generation = generation;
        }
    }

    /// Rebuild the request from everything the tools want and attach or detach the monitor.
    fn push_request(&mut self, core: &ThreadedSuperShuckieCore) {
        let interval = std::time::Duration::from_micros(1_000_000 / self.refresh_hz.max(1) as u64);
        let viewers = self.viewers;
        let playback = self.playback;
        let request = self.shared.update_request(|request| {
            request.sampling = viewers.iter().any(Option::is_some);
            request.interval = interval;
            request.windows = viewers;
            request.freezes_suspended = playback;
            request.clone()
        });

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
