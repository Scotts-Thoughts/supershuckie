//! Rust binding to the Azahar (Nintendo 3DS) emulator core.
//!
//! Azahar's `Core::System` is a process-wide singleton, so at most one [`Core`] exists at a
//! time; [`Core::new`] fails with [`CoreError::AlreadyRunning`] otherwise. The core owns a hidden
//! window with an OpenGL 4.3 context (the software rasterizer is 100x too slow), so it must be
//! created, run and dropped on one thread.
//!
//! Save states are Azahar's raw serialisation (no header, no compression); `deterministic
//! async operations`, a fixed clock and the JIT are pinned, which is what makes two runs of the
//! same inputs reproduce each other (see `replay-3ds-spec.md`).

#![no_std]
extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::ffi::{c_char, CStr};

#[repr(C)]
struct AzaharCoreRaw(());

/// What [`Core::new`] needs to know.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Settings {
    /// Emulate a New 3DS (256 MB FCRAM) instead of an Old 3DS (128 MB). Old 3DS halves every
    /// state buffer; every gen 6/7 Pokémon game runs on it.
    pub new_3ds: bool,
    /// dynarmic JIT (the only back end replays may use; the interpreter diverges from it).
    pub jit: bool,
    /// Console region, `-1` = pick from the game.
    pub region: i32,
    /// The emulated clock at power-on, seconds since 2000-01-01 (fixed, never the host clock).
    pub init_time: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self { new_3ds: false, jit: true, region: -1, init_time: 946_681_277 }
    }
}

/// One frame's input, in Azahar's own terms.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputState {
    /// Bit `n` = `NativeButton` `n`: A, B, X, Y, Up, Down, Left, Right, L, R, Start, Select,
    /// Debug, Gpio14, ZL, ZR, Home, Power.
    pub buttons: u32,
    /// Circle pad, -127..=127 each (positive = right / up).
    pub circle_x: i8,
    pub circle_y: i8,
    /// C-stick (New 3DS), same scale.
    pub c_stick_x: i8,
    pub c_stick_y: i8,
    /// Touch on the 320x240 bottom screen.
    pub touch_pressed: bool,
    pub touch_x: u16,
    pub touch_y: u16,
}

impl InputState {
    pub const A: u32 = 1 << 0;
    pub const B: u32 = 1 << 1;
    pub const X: u32 = 1 << 2;
    pub const Y: u32 = 1 << 3;
    pub const UP: u32 = 1 << 4;
    pub const DOWN: u32 = 1 << 5;
    pub const LEFT: u32 = 1 << 6;
    pub const RIGHT: u32 = 1 << 7;
    pub const L: u32 = 1 << 8;
    pub const R: u32 = 1 << 9;
    pub const START: u32 = 1 << 10;
    pub const SELECT: u32 = 1 << 11;
    pub const ZL: u32 = 1 << 14;
    pub const ZR: u32 = 1 << 15;
}

/// A region of the emulated program's address space.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Region {
    pub virtual_address: u32,
    pub length: u32,
    /// 1 = process heap, 2 = linear heap.
    pub kind: u32,
    /// Host pointer to the backing memory; valid until the next `run_frame`, state load or reset.
    pub data: *const u8,
}

unsafe extern "C" {
    fn azahar_rs_core_new(rom_path: *const c_char, user_dir: *const c_char, settings: *const Settings, error_out: *mut u32) -> *mut AzaharCoreRaw;
    fn azahar_rs_core_free(core: *mut AzaharCoreRaw);
    fn azahar_rs_core_last_error(core: *const AzaharCoreRaw) -> *const c_char;
    fn azahar_rs_core_run_frame(core: *mut AzaharCoreRaw, skip_drawing: bool) -> bool;
    fn azahar_rs_core_get_pixels(core: *const AzaharCoreRaw, screen: u32) -> *const u32;
    fn azahar_rs_core_set_input(core: *mut AzaharCoreRaw, input: *const InputState);
    fn azahar_rs_core_state_pending(core: *const AzaharCoreRaw) -> bool;
    fn azahar_rs_core_save_state_raw(core: *mut AzaharCoreRaw) -> usize;
    fn azahar_rs_core_state_data(core: *const AzaharCoreRaw) -> *const u8;
    fn azahar_rs_core_save_state_raw_into(core: *mut AzaharCoreRaw, buffer: *mut u8, capacity: usize, valid_len: usize) -> usize;
    fn azahar_rs_core_load_state_raw(core: *mut AzaharCoreRaw, data: *const u8, len: usize) -> bool;
    fn azahar_rs_core_read_memory(core: *const AzaharCoreRaw, address: u32, out: *mut u8, len: usize) -> bool;
    fn azahar_rs_core_write_memory(core: *mut AzaharCoreRaw, address: u32, data: *const u8, len: usize) -> bool;
    fn azahar_rs_core_region(core: *const AzaharCoreRaw, index: u32, out: *mut Region) -> bool;
    fn azahar_rs_core_reset(core: *mut AzaharCoreRaw) -> bool;
}

/// Why [`Core::new`] failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoreError {
    /// Azahar refused the file (not a decrypted `.cci`/`.cxi`/`.3dsx`, or missing system files).
    Load(String),
    /// Another core exists in this process (Azahar's `Core::System` is a singleton).
    AlreadyRunning,
    /// No OpenGL 4.3 context could be created.
    NoOpenGl,
    /// This platform has no context-creation code yet.
    Unsupported,
    Unknown(u32),
}

impl core::fmt::Display for CoreError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            CoreError::Load(details) => write!(f, "Azahar could not load the game: {details}"),
            CoreError::AlreadyRunning => f.write_str("another 3DS core is already running in this process"),
            CoreError::NoOpenGl => f.write_str("an OpenGL 4.3 context could not be created"),
            CoreError::Unsupported => f.write_str("the 3DS core is not available on this platform yet"),
            CoreError::Unknown(code) => write!(f, "unknown Azahar core error ({code})"),
        }
    }
}

pub struct Core {
    inner: *mut AzaharCoreRaw,
}

// The core is pinned to one thread by its OpenGL context; the trait object it lives in is moved
// to the core thread once and never shared, which is what `Send` promises here.
unsafe impl Send for Core {}

/// Top screen size.
pub const TOP_WIDTH: usize = 400;
pub const TOP_HEIGHT: usize = 240;
/// Bottom screen size.
pub const BOTTOM_WIDTH: usize = 320;
pub const BOTTOM_HEIGHT: usize = 240;

/// Frame rate as a rational: ARM11 clock / cycles per frame = 59.8261 Hz.
pub const FRAME_RATE: (u32, u32) = (268_111_856, 4_481_136);

impl Core {
    /// Load `rom_path` (a decrypted `.cci`/`.cxi`/`.3dsx`). `user_dir` is Azahar's user directory
    /// for this game: its virtual SD card (where the game's save lives), NAND and config.
    pub fn new(rom_path: &CStr, user_dir: &CStr, settings: &Settings) -> Result<Self, CoreError> {
        let mut code = 0u32;
        let inner = unsafe { azahar_rs_core_new(rom_path.as_ptr(), user_dir.as_ptr(), settings, &mut code) };
        if inner.is_null() {
            return Err(match code {
                1 => CoreError::Load(last_error_static()),
                3 => CoreError::AlreadyRunning,
                4 => CoreError::NoOpenGl,
                5 => CoreError::Unsupported,
                other => CoreError::Unknown(other),
            });
        }
        Ok(Self { inner })
    }

    /// Run one emulated frame (to the next VBlank). With `skip_drawing` the picture is not
    /// updated (draw batches are consumed, nothing is rasterised: 2x faster, memory identical).
    /// `false` means the emulated program stopped (an exception or a shutdown request); see
    /// [`Self::last_error`].
    #[inline]
    pub fn run_frame(&mut self, skip_drawing: bool) -> bool {
        unsafe { azahar_rs_core_run_frame(self.inner, skip_drawing) }
    }

    /// `0xAARRGGBB` pixels of the top (0, 400x240) or bottom (1, 320x240) screen as of the last
    /// drawn frame.
    #[inline]
    pub fn pixels(&self, screen: usize) -> &[u32] {
        let (ptr, len) = match screen {
            0 => (unsafe { azahar_rs_core_get_pixels(self.inner, 0) }, TOP_WIDTH * TOP_HEIGHT),
            1 => (unsafe { azahar_rs_core_get_pixels(self.inner, 1) }, BOTTOM_WIDTH * BOTTOM_HEIGHT),
            _ => return &[],
        };
        if ptr.is_null() {
            return &[];
        }
        unsafe { core::slice::from_raw_parts(ptr, len) }
    }

    #[inline]
    pub fn set_input(&mut self, input: &InputState) {
        unsafe { azahar_rs_core_set_input(self.inner, input) }
    }

    /// Whether an HLE operation is mid-flight, in which case a save state cannot be taken this
    /// frame (try again after the next frame).
    #[inline]
    pub fn state_pending(&self) -> bool {
        unsafe { azahar_rs_core_state_pending(self.inner) }
    }

    /// Write the raw state into `into` (170 MB Old 3DS / 308 MB New 3DS). `false` when
    /// [`Self::state_pending`]. Takes `&self`: serialising changes nothing the emulation can
    /// observe (the binding's own scratch buffer is what gets written).
    pub fn save_state_into(&self, into: &mut Vec<u8>) -> bool {
        // `into` keeps its contents until the new state is complete: when it holds a state this
        // core produced recently (a recycled keyframe buffer), only the memory pages written
        // since are copied into it, which is what keeps a keyframe under a frame.
        // A state's size drifts by a few bytes between saves; a reused buffer fits at once.
        if into.capacity() < 1 << 20 {
            into.reserve(180 << 20);
        }
        loop {
            let n = unsafe { azahar_rs_core_save_state_raw_into(self.inner, into.as_mut_ptr(), into.capacity(), into.len()) };
            if n == 0 {
                into.clear();
                return false;
            }
            if n <= into.capacity() {
                // SAFETY: the binding wrote or kept exactly `n` initialised bytes.
                unsafe { into.set_len(n) };
                return true;
            }
            into.reserve(n + (1 << 20) - into.len());
        }
    }

    /// [`Self::save_state_into`] into the binding's own buffer (an extra copy; kept for tools).
    pub fn save_state(&self) -> Option<Vec<u8>> {
        let n = unsafe { azahar_rs_core_save_state_raw(self.inner) };
        if n == 0 {
            return None;
        }
        let data = unsafe { azahar_rs_core_state_data(self.inner) };
        Some(unsafe { core::slice::from_raw_parts(data, n) }.to_vec())
    }

    #[inline]
    pub fn load_state(&mut self, state: &[u8]) -> bool {
        unsafe { azahar_rs_core_load_state_raw(self.inner, state.as_ptr(), state.len()) }
    }

    /// Read from the emulated program's virtual address space; `false` if any byte is unmapped.
    #[inline]
    pub fn read_memory(&self, address: u32, out: &mut [u8]) -> bool {
        unsafe { azahar_rs_core_read_memory(self.inner, address, out.as_mut_ptr(), out.len()) }
    }

    /// Write to the emulated program's virtual address space (JIT blocks over it are dropped).
    #[inline]
    pub fn write_memory(&mut self, address: u32, data: &[u8]) -> bool {
        unsafe { azahar_rs_core_write_memory(self.inner, address, data.as_ptr(), data.len()) }
    }

    /// The program's heap and linear-heap regions, in address order.
    pub fn regions(&self) -> Vec<Region> {
        let mut out = Vec::new();
        let mut index = 0u32;
        loop {
            let mut region = Region::default();
            if !unsafe { azahar_rs_core_region(self.inner, index, &mut region) } {
                break;
            }
            out.push(region);
            index += 1;
        }
        out
    }

    /// Power-cycle: the game is reloaded from its file with the same settings.
    #[inline]
    pub fn reset(&mut self) -> bool {
        unsafe { azahar_rs_core_reset(self.inner) }
    }

    pub fn last_error(&self) -> String {
        cstr_to_string(unsafe { azahar_rs_core_last_error(self.inner) })
    }
}

fn last_error_static() -> String {
    cstr_to_string(unsafe { azahar_rs_core_last_error(core::ptr::null()) })
}

fn cstr_to_string(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    String::from(unsafe { CStr::from_ptr(ptr) }.to_str().unwrap_or(""))
}

impl Drop for Core {
    fn drop(&mut self) {
        unsafe { azahar_rs_core_free(self.inner) }
    }
}
