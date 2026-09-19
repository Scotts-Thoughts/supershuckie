#![no_std]
extern crate alloc;

use alloc::vec::Vec;

#[repr(C)]
struct MGBACoreRaw(());

#[repr(C)]
struct MGBALinkCoordinatorRaw(());

unsafe extern "C" {
    fn mgba_rs_core_new(
        rom: *const u8,
        rom_size: usize,
        sram: *const u8,
        sram_size: usize,
        bios: *const u8,
        bios_size: usize,
        error_out: *mut u32
    ) -> *mut MGBACoreRaw;
    fn mgba_rs_core_free(core: *mut MGBACoreRaw);
    fn mgba_rs_core_run_frame(core: *mut MGBACoreRaw);
    fn mgba_rs_core_reset(core: *mut MGBACoreRaw);
    fn mgba_rs_core_get_pixels(core: *const MGBACoreRaw) -> *const u32;
    fn mgba_rs_core_set_input(core: *mut MGBACoreRaw, input: u16);
    fn mgba_rs_core_get_sram(core: *const MGBACoreRaw, size: &mut usize) -> *const u8;
    fn mgba_rs_core_free_sram_clone(sram: *mut u8);
    fn mgba_rs_core_create_save_state(core: *const MGBACoreRaw, data: *mut u8, data_size: usize) -> usize;
    fn mgba_rs_core_load_save_state(core: *mut MGBACoreRaw, data: *const u8, data_size: usize) -> bool;
    fn mgba_rs_core_get_ewram(core: *mut MGBACoreRaw) -> *mut [u8; 0x40000];
    fn mgba_rs_core_get_iwram(core: *mut MGBACoreRaw) -> *mut [u8; 0x8000];
    fn mgba_rs_core_set_audio_enabled(core: *mut MGBACoreRaw, enabled: bool);
    fn mgba_rs_core_get_region(core: *mut MGBACoreRaw, region: u32, size: &mut usize) -> *mut u8;
    fn mgba_rs_core_patch_write(core: *mut MGBACoreRaw, address: u32, data: *const u8, length: usize);
    fn mgba_rs_core_read_audio(core: *mut MGBACoreRaw, out: *mut i16, max_frames: usize) -> usize;

    fn mgba_rs_link_coordinator_new() -> *mut MGBALinkCoordinatorRaw;
    fn mgba_rs_link_coordinator_free(coordinator: *mut MGBALinkCoordinatorRaw);
    fn mgba_rs_core_link_attach(core: *mut MGBACoreRaw, coordinator: *mut MGBALinkCoordinatorRaw, first: bool) -> bool;
    fn mgba_rs_core_link_detach(core: *mut MGBACoreRaw);
    fn mgba_rs_core_link_is_attached(core: *const MGBACoreRaw) -> bool;
    fn mgba_rs_core_link_is_asleep(core: *const MGBACoreRaw) -> bool;
    fn mgba_rs_core_link_player_id(core: *const MGBACoreRaw) -> i32;
    fn mgba_rs_core_run_loop(core: *mut MGBACoreRaw) -> u32;
    fn mgba_rs_core_timing_now(core: *const MGBACoreRaw) -> i32;
    fn mgba_rs_core_link_frame_started(core: *mut MGBACoreRaw);
    fn mgba_rs_core_link_take_log(core: *mut MGBACoreRaw, out: *mut u8, capacity: usize) -> usize;
    fn mgba_rs_core_replay_detach(core: *mut MGBACoreRaw);
    fn mgba_rs_core_replay_is_attached(core: *const MGBACoreRaw) -> bool;
    fn mgba_rs_core_replay_misses(core: *const MGBACoreRaw) -> u64;
    fn mgba_rs_core_replay_queue(core: *mut MGBACoreRaw, data: *const u8, size: usize) -> u32;
}

/// mGBA's lockstep coordinator: what joins the cores of a link cable. One per cable; every
/// core plugged in holds a clone (see [`Core::link_attach`]) and the last one to let go frees
/// it. The cores it joins must be stepped from one thread.
pub struct LinkCoordinator {
    inner: alloc::sync::Arc<CoordinatorHandle>
}

struct CoordinatorHandle(*mut MGBALinkCoordinatorRaw);

unsafe impl Send for CoordinatorHandle {}
unsafe impl Sync for CoordinatorHandle {}

impl Drop for CoordinatorHandle {
    fn drop(&mut self) {
        unsafe { mgba_rs_link_coordinator_free(self.0) }
    }
}

impl Default for LinkCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl LinkCoordinator {
    /// A coordinator with nobody plugged in.
    pub fn new() -> Self {
        Self { inner: alloc::sync::Arc::new(CoordinatorHandle(unsafe { mgba_rs_link_coordinator_new() })) }
    }

    /// Whether `other` is the same coordinator.
    pub fn same_as(&self, other: &LinkCoordinator) -> bool {
        alloc::sync::Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Clone for LinkCoordinator {
    fn clone(&self) -> Self {
        Self { inner: alloc::sync::Arc::clone(&self.inner) }
    }
}

/// What [`Core::replay_queue`] did with a frame's serial log.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ReplayQueued {
    /// Applied.
    Applied,
    /// Applied, and the cable came out during that frame: call [`Core::replay_detach`] once the
    /// frame has run.
    AppliedThenDetach
}

pub struct Core {
    inner: *mut MGBACoreRaw,
    /// The cable's coordinator while one is attached: kept alive as long as this core is on it.
    coordinator: Option<LinkCoordinator>
}

/// Memory regions reachable through [`Core::get_region`].
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Region {
    PaletteRAM = 0,
    VRAM = 1,
    OAM = 2,
    /// The cartridge's save memory (SRAM, flash or EEPROM), all of it; empty until mGBA knows
    /// the save type.
    SaveData = 3
}

unsafe impl Send for Core {}

/// Why [`Core::new`] failed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CoreError {
    /// mGBA failed to allocate/create its core object.
    CreateFailed,
    /// The created core failed to initialise.
    InitFailed,
    /// The ROM was rejected (including an empty one).
    BadRom,
    /// mGBA did not expose EWRAM/IWRAM as memory blocks.
    MissingMemoryBlocks,
    /// The BIOS image was rejected.
    BadBios,
    /// An error code the Rust binding does not recognise.
    Unknown(u32)
}

impl From<u32> for CoreError {
    fn from(code: u32) -> Self {
        match code {
            1 => CoreError::CreateFailed,
            2 => CoreError::InitFailed,
            3 => CoreError::BadRom,
            4 => CoreError::MissingMemoryBlocks,
            5 => CoreError::BadBios,
            other => CoreError::Unknown(other)
        }
    }
}

impl core::fmt::Display for CoreError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            CoreError::CreateFailed => f.write_str("failed to create the mGBA core"),
            CoreError::InitFailed => f.write_str("failed to initialize the mGBA core"),
            CoreError::BadRom => f.write_str("the ROM was rejected"),
            CoreError::MissingMemoryBlocks => f.write_str("mGBA did not expose EWRAM/IWRAM"),
            CoreError::BadBios => f.write_str("the BIOS image was rejected"),
            CoreError::Unknown(code) => write!(f, "unknown mGBA core error ({code})")
        }
    }
}

impl Core {
    pub fn new(rom: &[u8], sram: &[u8], bios: &[u8]) -> Result<Self, CoreError> {
        let mut error_code: u32 = 0;
        let inner = unsafe {
            mgba_rs_core_new(
                rom.as_ptr(), rom.len(),
                sram.as_ptr(), sram.len(),
                bios.as_ptr(), bios.len(),
                &mut error_code
            )
        };
        if inner.is_null() {
            return Err(CoreError::from(error_code))
        }
        Ok(Self { inner, coordinator: None })
    }

    #[inline]
    pub fn reset(&mut self) {
        unsafe { mgba_rs_core_reset(self.inner) }
    }

    #[inline]
    pub fn run_frame(&mut self) {
        unsafe { mgba_rs_core_run_frame(self.inner) }
    }

    #[inline]
    pub fn get_pixels(&self) -> &[u32; 240 * 160] {
        let pixels = unsafe { mgba_rs_core_get_pixels(self.inner) };
        unsafe { &*(pixels as *const _) }
    }

    #[inline]
    pub fn set_input(&mut self, input: u16) {
        unsafe { mgba_rs_core_set_input(self.inner, input) }
    }

    /// A copy of the cartridge's save memory, or an empty `Vec` if mGBA does not know the save
    /// type yet (the game was closed before it touched its save). mGBA hands back a fresh
    /// `malloc`'d clone on every call; this copies it into a `Vec` and frees the clone itself.
    #[inline]
    pub fn get_sram(&self) -> Vec<u8> {
        let mut size = 0;
        let ptr = unsafe { mgba_rs_core_get_sram(self.inner, &mut size) };
        if ptr.is_null() || size == 0 {
            return Vec::new()
        }
        let mut v = Vec::with_capacity(size);
        unsafe {
            core::ptr::copy_nonoverlapping(ptr, v.as_mut_ptr(), size);
            v.set_len(size);
            mgba_rs_core_free_sram_clone(ptr as *mut u8);
        }
        v
    }

    /// Serialises the core's state. Takes `&self`: mGBA's serializer does touch some internal
    /// bookkeeping (e.g. flushing the current flash/EEPROM command) while writing the state out,
    /// but that is treated as an implementation detail of what is logically a read, not a
    /// mutation callers need to synchronise against the way [`load_save_state`](Self::load_save_state) is.
    #[inline]
    pub fn create_save_state(&self) -> Option<Vec<u8>> {
        let mut v = Vec::new();
        self.create_save_state_into(&mut v).then_some(v)
    }

    /// Write a save state into `into`, reusing its allocation (see the melonDS binding for why
    /// callers taking periodic states should recycle buffers).
    pub fn create_save_state_into(&self, into: &mut Vec<u8>) -> bool {
        into.clear();
        into.reserve(32 * 1024 * 1024);

        let capacity = into.capacity();
        let n = unsafe { mgba_rs_core_create_save_state(self.inner, into.as_mut_ptr(), capacity) };
        // The shim already guarantees n <= capacity when n != 0; re-checked here so a bug on the
        // C++ side can never make us set_len past what was actually allocated/written.
        if n == 0 || n > capacity {
            return false
        }
        unsafe { into.set_len(n); }
        true
    }

    /// Loads a save state, overwriting all emulated RAM and the running core's state. Treated as a
    /// mutation (not merely a read of `state`) because it invalidates any borrow of the core's
    /// memory (e.g. from [`get_ewram`](Self::get_ewram)) taken before the call.
    #[inline]
    pub fn load_save_state(&mut self, state: &[u8]) -> bool {
        unsafe { mgba_rs_core_load_save_state(self.inner, state.as_ptr(), state.len()) }
    }

    #[inline]
    pub fn get_ewram(&self) -> &[u8] {
        unsafe { &*mgba_rs_core_get_ewram(self.inner) }.as_slice()
    }

    #[inline]
    pub fn get_ewram_mut(&mut self) -> &mut [u8] {
        unsafe { &mut *mgba_rs_core_get_ewram(self.inner) }.as_mut_slice()
    }

    #[inline]
    pub fn get_iwram(&self) -> &[u8] {
        unsafe { &*mgba_rs_core_get_iwram(self.inner) }.as_slice()
    }

    #[inline]
    pub fn get_iwram_mut(&mut self) -> &mut [u8] {
        unsafe { &mut *mgba_rs_core_get_iwram(self.inner) }.as_mut_slice()
    }

    /// A memory region other than EWRAM/IWRAM, or an empty slice if the core has none right now.
    #[inline]
    pub fn get_region(&self, region: Region) -> &[u8] {
        let mut size = 0;
        let ptr = unsafe { mgba_rs_core_get_region(self.inner, region as u32, &mut size) };
        if ptr.is_null() || size == 0 {
            return &[]
        }
        unsafe { core::slice::from_raw_parts(ptr, size) }
    }

    /// Mutable [`get_region`](Self::get_region).
    #[inline]
    pub fn get_region_mut(&mut self, region: Region) -> &mut [u8] {
        let mut size = 0;
        let ptr = unsafe { mgba_rs_core_get_region(self.inner, region as u32, &mut size) };
        if ptr.is_null() || size == 0 {
            return &mut []
        }
        unsafe { core::slice::from_raw_parts_mut(ptr, size) }
    }

    /// Write `data` at the bus `address` through mGBA's patch path, which keeps the renderer's
    /// palette, VRAM and OAM caches up to date (a plain memory write would not).
    #[inline]
    pub fn patch_write(&mut self, address: u32, data: &[u8]) {
        unsafe { mgba_rs_core_patch_write(self.inner, address, data.as_ptr(), data.len()) }
    }

    /// Whether the core's mix is resampled for `read_audio`. Never affects emulation.
    #[inline]
    pub fn set_audio_enabled(&mut self, enabled: bool) {
        unsafe { mgba_rs_core_set_audio_enabled(self.inner, enabled) }
    }

    /// Pop the stereo frames (interleaved `i16` pairs at 48 kHz) mixed since the last call into
    /// `out`, returning how many frames were written. Nothing while audio is disabled.
    #[inline]
    pub fn read_audio(&mut self, out: &mut [i16]) -> usize {
        unsafe { mgba_rs_core_read_audio(self.inner, out.as_mut_ptr(), out.len() / 2) }
    }
}

impl Core {
    /// Plug this core into `coordinator` as the cable's first (`first`) or second player. mGBA's
    /// lockstep driver replaces the SIO driver; from here on the core is stepped with
    /// [`run_loop`](Self::run_loop) and only while [`is_link_asleep`](Self::is_link_asleep) is
    /// false, cooperatively with the other core on the same coordinator. Fails while a cable or
    /// a replay is already in.
    pub fn link_attach(&mut self, coordinator: &LinkCoordinator, first: bool) -> bool {
        let ok = unsafe { mgba_rs_core_link_attach(self.inner, coordinator.inner.0, first) };
        if ok {
            self.coordinator = Some(coordinator.clone());
        }
        ok
    }

    /// Pull the cable (nothing without one).
    pub fn link_detach(&mut self) {
        unsafe { mgba_rs_core_link_detach(self.inner) }
        self.coordinator = None;
    }

    /// The coordinator this core is plugged into, while it is.
    #[inline]
    pub fn link_coordinator(&self) -> Option<&LinkCoordinator> {
        self.coordinator.as_ref()
    }

    #[inline]
    pub fn is_link_attached(&self) -> bool {
        unsafe { mgba_rs_core_link_is_attached(self.inner) }
    }

    /// Whether the lockstep coordinator has put this core to sleep (it waits for the other
    /// core; stepping it now would break the lockstep).
    #[inline]
    pub fn is_link_asleep(&self) -> bool {
        unsafe { mgba_rs_core_link_is_asleep(self.inner) }
    }

    /// The player number the coordinator gave this core (0 = the clock owner), or -1.
    #[inline]
    pub fn link_player_id(&self) -> i32 {
        unsafe { mgba_rs_core_link_player_id(self.inner) }
    }

    /// One slice of emulation (until mGBA's next timing event): how many frames completed in
    /// it (0 or 1). The pixel buffer holds the new frame when 1.
    #[inline]
    pub fn run_loop(&mut self) -> u32 {
        unsafe { mgba_rs_core_run_loop(self.inner) }
    }

    /// The core's cycle clock: wraps every couple of minutes, so take differences.
    #[inline]
    pub fn timing_now(&self) -> i32 {
        unsafe { mgba_rs_core_timing_now(self.inner) }
    }

    /// A new frame begins now (while a cable is in): the serial log's times are relative to it.
    #[inline]
    pub fn link_frame_started(&mut self) {
        unsafe { mgba_rs_core_link_frame_started(self.inner) }
    }

    /// The serial log gathered since the last call (the `SerialIn` payload of a Game Boy
    /// Advance frame), appended to `into`.
    pub fn link_take_log(&mut self, into: &mut Vec<u8>) {
        let mut capacity = 256usize;
        loop {
            let start = into.len();
            into.resize(start + capacity, 0);
            let needed = unsafe { mgba_rs_core_link_take_log(self.inner, into.as_mut_ptr().add(start), capacity) };
            if needed <= capacity {
                into.truncate(start + needed);
                return
            }
            into.truncate(start);
            capacity = needed;
        }
    }

    /// Feed one frame's recorded serial log, at the frame boundary before that frame runs; the
    /// replay driver goes in with the first. `None` when the log does not parse.
    pub fn replay_queue(&mut self, log: &[u8]) -> Option<ReplayQueued> {
        match unsafe { mgba_rs_core_replay_queue(self.inner, log.as_ptr(), log.len()) } {
            1 => Some(ReplayQueued::Applied),
            2 => Some(ReplayQueued::AppliedThenDetach),
            _ => None
        }
    }

    /// Take the replay driver out (nothing without one).
    #[inline]
    pub fn replay_detach(&mut self) {
        unsafe { mgba_rs_core_replay_detach(self.inner) }
    }

    #[inline]
    pub fn is_replay_attached(&self) -> bool {
        unsafe { mgba_rs_core_replay_is_attached(self.inner) }
    }

    /// How often the game asked the replay driver for something the log did not have.
    #[inline]
    pub fn replay_misses(&self) -> u64 {
        unsafe { mgba_rs_core_replay_misses(self.inner) }
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        unsafe { mgba_rs_core_free(self.inner) }
    }
}
