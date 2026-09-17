#![no_std]
extern crate alloc;

use alloc::vec::Vec;

#[repr(C)]
struct MGBACoreRaw(());

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
}

pub struct Core {
    inner: *mut MGBACoreRaw
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
        Ok(Self { inner })
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

impl Drop for Core {
    fn drop(&mut self) {
        unsafe { mgba_rs_core_free(self.inner) }
    }
}
