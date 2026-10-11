#![no_std]
extern crate alloc;

use alloc::vec::Vec;

#[repr(C)]
struct MelonDSCoreHolderRaw(());

unsafe extern "C" {
    fn melonds_rs_core_new(
        rom: *const u8,
        rom_size: usize,
        sram: *const u8,
        sram_size: usize,
        jit: bool,
        error_out: *mut u32
    ) -> *mut MelonDSCoreHolderRaw;
    fn melonds_rs_core_free(core: *mut MelonDSCoreHolderRaw);
    fn melonds_rs_core_run_frame(core: *mut MelonDSCoreHolderRaw);
    fn melonds_rs_core_set_skip_drawing(core: *mut MelonDSCoreHolderRaw, skip: bool);
    fn melonds_rs_core_reset(core: *mut MelonDSCoreHolderRaw);
    fn melonds_rs_core_get_pixels(core: *const MelonDSCoreHolderRaw, screen: usize) -> *const u32;
    fn melonds_rs_core_set_input(core: *mut MelonDSCoreHolderRaw, input: u32);
    fn melonds_rs_core_get_sram(core: *const MelonDSCoreHolderRaw, size: &mut usize) -> *const u8;
    fn melonds_rs_core_create_save_state(core: *mut MelonDSCoreHolderRaw, data: *mut u8, data_size: usize) -> usize;
    fn melonds_rs_core_load_save_state(core: *mut MelonDSCoreHolderRaw, data: *const u8, data_size: usize) -> bool;
    fn melonds_rs_core_load_save_state_discarding_geometry(core: *mut MelonDSCoreHolderRaw, data: *const u8, data_size: usize) -> bool;
    fn melonds_rs_core_shows_discarded_geometry(core: *const MelonDSCoreHolderRaw) -> bool;
    fn melonds_rs_core_get_ram(core: *mut MelonDSCoreHolderRaw) -> *mut [u8; 0x400000];
    fn melonds_rs_core_get_shared_wram(core: *mut MelonDSCoreHolderRaw) -> *mut [u8; 0x8000];
    fn melonds_rs_core_get_arm7_wram(core: *mut MelonDSCoreHolderRaw) -> *mut [u8; 0x10000];
    fn melonds_rs_core_invalidate_jit(core: *mut MelonDSCoreHolderRaw, region: u32, offset: u32, length: usize);
    fn melonds_rs_core_read_audio(core: *mut MelonDSCoreHolderRaw, out: *mut i16, max_frames: usize) -> usize;
    fn melonds_rs_core_drain_audio(core: *mut MelonDSCoreHolderRaw);
    fn melonds_rs_core_set_date(
        core: *mut MelonDSCoreHolderRaw,
        year: u16,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8
    );
    fn melonds_rs_core_adopt_replay_keyframe(core: *mut MelonDSCoreHolderRaw) -> bool;
    fn melonds_rs_core_check_replay_keyframe(core: *mut MelonDSCoreHolderRaw, data: *const u8, size: usize) -> u32;
    fn melonds_rs_core_debug_unsaved_state(core: *mut MelonDSCoreHolderRaw, out: *mut u32, max: usize) -> usize;
    fn melonds_rs_core_debug_firmware(core: *mut MelonDSCoreHolderRaw, length: *mut usize) -> *const u8;
    fn melonds_rs_core_debug_write_firmware(core: *mut MelonDSCoreHolderRaw, offset: usize, data: *const u8, length: usize);
}

/// Names of the values [`Core::debug_unsaved_state`] returns, in order.
pub const DEBUG_UNSAVED_STATE_NAMES: [&str; 24] = [
    "ARM9.IRQ", "ARM7.IRQ", "ARM9.Halted", "ARM7.Halted",
    "IME9", "IE9", "IF9", "IME7", "IE7", "IF7",
    "DIVCNT", "DIV_NUMER.lo", "DIV_NUMER.hi", "DIV_DENOM.lo", "DIV_DENOM.hi",
    "DIV_RESULT.lo", "DIV_RESULT.hi", "DIVREM.lo", "DIVREM.hi",
    "SQRTCNT", "SQRT_RESULT", "SQRT_PARAM.lo", "SQRT_PARAM.hi",
    "KEYINPUT",
];

pub struct Core {
    inner: *mut MelonDSCoreHolderRaw
}

/// What [`Core::check_replay_keyframe`] found.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ReplayKeyframeCheck {
    /// The keyframe carries the firmware's settings and the console's differed; it now has the
    /// keyframe's.
    pub firmware_synced: bool,
    /// The game used the firmware chip differently from the recording since they last matched
    /// (a boot found a WFC ID where the recording's wrote one, or the other way round): only
    /// loading the keyframe makes up for that.
    pub firmware_access_differs: bool
}

/// Memory whose compiled JIT blocks [`Core::invalidate_jit`] can drop.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum JitRegion {
    MainRAM = 0,
    SharedWRAM = 1,
    ARM7WRAM = 2
}

unsafe impl Send for Core {}

/// Why [`Core::new`] failed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CoreError {
    /// The ROM was rejected by melonDS's parser.
    BadRom,
    /// Core construction, ROM parsing or save loading threw a C++ exception.
    Exception,
    /// An error code the Rust binding does not recognise.
    Unknown(u32)
}

impl From<u32> for CoreError {
    fn from(code: u32) -> Self {
        match code {
            1 => CoreError::BadRom,
            2 => CoreError::Exception,
            other => CoreError::Unknown(other)
        }
    }
}

impl core::fmt::Display for CoreError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            CoreError::BadRom => f.write_str("the ROM was rejected"),
            CoreError::Exception => f.write_str("melonDS threw while creating the core"),
            CoreError::Unknown(code) => write!(f, "unknown melonDS core error ({code})")
        }
    }
}

impl Core {
    pub fn new(rom: &[u8], sram: &[u8], jit: bool) -> Result<Self, CoreError> {
        let mut error_code: u32 = 0;
        let inner = unsafe {
            melonds_rs_core_new(rom.as_ptr(), rom.len(), sram.as_ptr(), sram.len(), jit, &mut error_code)
        };
        if inner.is_null() {
            return Err(CoreError::from(error_code))
        }
        Ok(Self { inner })
    }
    #[inline]
    pub fn reset(&mut self) {
        unsafe { melonds_rs_core_reset(self.inner) }
    }
    #[inline]
    pub fn run_frame(&mut self) {
        unsafe { melonds_rs_core_run_frame(self.inner) }
    }
    /// Presentation hint for the frames that follow: when `skip` is set the 2D renderer does not
    /// composite them, so `get_pixels` keeps returning the last drawn frame. Emulation, timing
    /// and save states are unaffected.
    #[inline]
    pub fn set_skip_drawing(&mut self, skip: bool) {
        unsafe { melonds_rs_core_set_skip_drawing(self.inner, skip) }
    }
    #[inline]
    pub fn get_pixels(&self, screen: usize) -> Option<&[u32; 256 * 192]> {
        let line = unsafe { melonds_rs_core_get_pixels(self.inner, screen) };
        if line.is_null() {
            return None
        }
        Some(unsafe { &*(line as *const _) })
    }
    #[inline]
    pub fn set_input(&mut self, input: u32) {
        unsafe { melonds_rs_core_set_input(self.inner, input) }
    }
    /// The cartridge's live save memory (not a copy), or an empty slice if this cart has none
    /// (`GetNDSSave` can return null with a zero length in that case, which is not safe to hand to
    /// `slice::from_raw_parts` directly).
    #[inline]
    pub fn get_sram(&self) -> &[u8] {
        let mut size = 0;
        let ptr = unsafe { melonds_rs_core_get_sram(self.inner, &mut size) };
        if ptr.is_null() || size == 0 {
            return &[]
        }
        unsafe { core::slice::from_raw_parts(ptr, size) }
    }
    /// Upper bound of a melonDS save state; the writer fails (returns 0) if it does not fit.
    const SAVE_STATE_CAPACITY: usize = 32 * 1024 * 1024;

    #[inline]
    pub fn create_save_state(&self) -> Option<Vec<u8>> {
        let mut v = Vec::new();
        self.create_save_state_into(&mut v).then_some(v)
    }

    /// Write a save state into `into`, reusing its allocation.
    ///
    /// A fresh 32 MiB allocation costs several milliseconds of page faults on every call; a
    /// buffer that has been written once is filled in well under a millisecond, so callers that
    /// take states periodically should keep and reuse the buffers they get back.
    pub fn create_save_state_into(&self, into: &mut Vec<u8>) -> bool {
        into.clear();
        into.reserve(Self::SAVE_STATE_CAPACITY);

        match unsafe { melonds_rs_core_create_save_state(self.inner, into.as_mut_ptr(), into.capacity()) } {
            0 => false,
            n => {
                unsafe { into.set_len(n); }
                true
            }
        }
    }

    /// Loads a save state, overwriting all emulated RAM and the running core's state. Takes
    /// `&mut self` because it invalidates any borrow of the core's memory (e.g. from
    /// [`get_main_ram`](Self::get_main_ram)) taken before the call.
    #[inline]
    pub fn load_save_state(&mut self, state: &[u8]) -> bool {
        unsafe { melonds_rs_core_load_save_state(self.inner, state.as_ptr(), state.len()) }
    }

    /// Like [`load_save_state`](Self::load_save_state), for a state whose 3D polygon and vertex
    /// RAM may be a stale copy from another state (a masked replay keyframe): nothing is drawn
    /// from the polygons it restores, only from those the game submits afterwards. Whether a
    /// frame still shows the gap is [`shows_discarded_geometry`](Self::shows_discarded_geometry).
    #[inline]
    pub fn load_save_state_discarding_geometry(&mut self, state: &[u8]) -> bool {
        unsafe { melonds_rs_core_load_save_state_discarding_geometry(self.inner, state.as_ptr(), state.len()) }
    }

    /// Whether the frame last run showed a 3D picture missing geometry that
    /// [`load_save_state_discarding_geometry`](Self::load_save_state_discarding_geometry)
    /// discarded: the game had not yet flushed and shown a frame's worth of polygons of its own.
    #[inline]
    pub fn shows_discarded_geometry(&self) -> bool {
        unsafe { melonds_rs_core_shows_discarded_geometry(self.inner) }
    }

    #[inline]
    pub fn get_main_ram(&self) -> &[u8] {
        unsafe { &*melonds_rs_core_get_ram(self.inner) }.as_slice()
    }

    #[inline]
    pub fn get_main_ram_mut(&mut self) -> &mut [u8] {
        unsafe { &mut *melonds_rs_core_get_ram(self.inner) }.as_mut_slice()
    }

    /// The 32 KiB of work RAM shared by both CPUs (which CPU sees which part depends on WRAMCNT).
    #[inline]
    pub fn get_shared_wram(&self) -> &[u8] {
        unsafe { &*melonds_rs_core_get_shared_wram(self.inner) }.as_slice()
    }

    #[inline]
    pub fn get_shared_wram_mut(&mut self) -> &mut [u8] {
        unsafe { &mut *melonds_rs_core_get_shared_wram(self.inner) }.as_mut_slice()
    }

    /// The ARM7's private 64 KiB of work RAM.
    #[inline]
    pub fn get_arm7_wram(&self) -> &[u8] {
        unsafe { &*melonds_rs_core_get_arm7_wram(self.inner) }.as_slice()
    }

    #[inline]
    pub fn get_arm7_wram_mut(&mut self) -> &mut [u8] {
        unsafe { &mut *melonds_rs_core_get_arm7_wram(self.inner) }.as_mut_slice()
    }

    /// After writing `length` bytes at `offset` of `region` directly, drop any JIT blocks compiled
    /// from them (a no-op with the JIT off).
    #[inline]
    pub fn invalidate_jit(&mut self, region: JitRegion, offset: u32, length: usize) {
        unsafe { melonds_rs_core_invalidate_jit(self.inner, region as u32, offset, length) }
    }

    /// Pop the stereo frames (interleaved `i16` pairs at 48 kHz) mixed since the last call into
    /// `out`, returning how many frames were written. Reading never affects emulation.
    #[inline]
    pub fn read_audio(&mut self, out: &mut [i16]) -> usize {
        unsafe { melonds_rs_core_read_audio(self.inner, out.as_mut_ptr(), out.len() / 2) }
    }

    /// Forget the audio mixed so far.
    #[inline]
    pub fn drain_audio(&mut self) {
        unsafe { melonds_rs_core_drain_audio(self.inner) }
    }

    #[inline]
    pub fn set_date(
        &mut self,
        year: u16,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8
    ) {
        unsafe { melonds_rs_core_set_date(self.inner, year, month, day, hour, minute, second) }
    }

    /// Replay playback, right after loading one of the replay's keyframes: when the keyframe
    /// predates save states carrying the firmware and was taken well after the game booted, give
    /// the firmware a Nintendo WFC ID if it has none, as the recording console's had by then
    /// (games write one a moment into every boot that finds none, so without it the replay's next
    /// console reset goes differently). Returns whether the firmware changed.
    #[inline]
    pub fn adopt_replay_keyframe(&mut self) -> bool {
        unsafe { melonds_rs_core_adopt_replay_keyframe(self.inner) }
    }

    /// Replay playback reached `state`, one of the replay's keyframes, by playing rather than by
    /// loading it: copies the keyframe's firmware settings into the console when it carries them
    /// and they differ, and compares how the game has used the firmware chip with the recording.
    #[inline]
    pub fn check_replay_keyframe(&mut self, state: &[u8]) -> ReplayKeyframeCheck {
        let bits = unsafe { melonds_rs_core_check_replay_keyframe(self.inner, state.as_ptr(), state.len()) };
        ReplayKeyframeCheck {
            firmware_synced: bits & 1 != 0,
            firmware_access_differs: bits & 2 != 0
        }
    }

    /// Diagnostics: CPU-visible state that melonDS save states do not carry (the CPUs' IRQ
    /// lines, the divider and square root units, the key input), named by
    /// [`DEBUG_UNSAVED_STATE_NAMES`]. IME/IE/IF are included to check the IRQ lines against.
    pub fn debug_unsaved_state(&self) -> Vec<u32> {
        let mut out = alloc::vec![0u32; DEBUG_UNSAVED_STATE_NAMES.len()];
        let n = unsafe { melonds_rs_core_debug_unsaved_state(self.inner, out.as_mut_ptr(), out.len()) };
        out.truncate(n);
        out
    }

    /// Diagnostics: the firmware flash image. Games can write to it; save states do not carry it.
    pub fn debug_firmware(&self) -> &[u8] {
        let mut length = 0usize;
        let ptr = unsafe { melonds_rs_core_debug_firmware(self.inner, &mut length) };
        if ptr.is_null() {
            return &[];
        }
        unsafe { core::slice::from_raw_parts(ptr, length) }
    }

    /// Diagnostics: overwrite bytes of the firmware flash image (out-of-range bytes are dropped).
    pub fn debug_write_firmware(&mut self, offset: usize, data: &[u8]) {
        unsafe { melonds_rs_core_debug_write_firmware(self.inner, offset, data.as_ptr(), data.len()) }
    }

}

impl Drop for Core {
    fn drop(&mut self) {
        unsafe { melonds_rs_core_free(self.inner) }
    }
}
