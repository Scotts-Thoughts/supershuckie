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
        jit: bool
    ) -> *mut MelonDSCoreHolderRaw;
    fn melonds_rs_core_free(core: *mut MelonDSCoreHolderRaw);
    fn melonds_rs_core_run_frame(core: *mut MelonDSCoreHolderRaw);
    fn melonds_rs_core_set_skip_drawing(core: *mut MelonDSCoreHolderRaw, skip: bool);
    fn melonds_rs_core_reset(core: *mut MelonDSCoreHolderRaw);
    fn melonds_rs_core_get_pixels(core: *const MelonDSCoreHolderRaw, screen: usize) -> *const u32;
    fn melonds_rs_core_set_input(core: *mut MelonDSCoreHolderRaw, input: u32);
    fn melonds_rs_core_get_sram(core: *const MelonDSCoreHolderRaw, size: &mut usize) -> *const u8;
    fn melonds_rs_core_create_save_state(core: *const MelonDSCoreHolderRaw, data: *mut u8, data_size: usize) -> usize;
    fn melonds_rs_core_load_save_state(core: *mut MelonDSCoreHolderRaw, data: *const u8, data_size: usize) -> bool;
    fn melonds_rs_core_get_ram(core: *mut MelonDSCoreHolderRaw) -> *mut [u8; 0x400000];
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
}

pub struct Core {
    inner: *mut MelonDSCoreHolderRaw
}

unsafe impl Send for Core {}
unsafe impl Sync for Core {}

impl Core {
    pub fn new(rom: &[u8], sram: &[u8], jit: bool) -> Option<Self> {
        let inner = unsafe { melonds_rs_core_new(rom.as_ptr(), rom.len(), sram.as_ptr(), sram.len(), jit) };
        if inner.is_null() {
            return None
        }
        Some(Self { inner })
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
    #[inline]
    pub fn get_sram(&self) -> &[u8] {
        let mut size = 0;
        let ptr = unsafe { melonds_rs_core_get_sram(self.inner, &mut size) };
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

    #[inline]
    pub fn load_save_state(&self, state: &[u8]) -> bool {
        unsafe { melonds_rs_core_load_save_state(self.inner, state.as_ptr(), state.len()) }
    }

    #[inline]
    pub fn get_main_ram(&self) -> &[u8] {
        unsafe { &*melonds_rs_core_get_ram(self.inner) }.as_slice()
    }

    #[inline]
    pub fn get_main_ram_mut(&mut self) -> &mut [u8] {
        unsafe { &mut *melonds_rs_core_get_ram(self.inner) }.as_mut_slice()
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

}

impl Drop for Core {
    fn drop(&mut self) {
        unsafe { melonds_rs_core_free(self.inner) }
    }
}
