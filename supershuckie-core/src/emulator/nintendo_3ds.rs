use alloc::vec::Vec;
use alloc::string::String;
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::ffi::CString;
use azahar_rs::{Core, InputState, Settings};

/// Azahar settings a [`Nintendo3DS`] is created with (Old 3DS mode, JIT, fixed clock by default).
pub use azahar_rs::Settings as Nintendo3DSSettings;
use supershuckie_replay_recorder::blake3_hash;
use supershuckie_replay_recorder::replay_file::{ReplayConsoleType, ReplayHeaderBlake3Hash};
use crate::emulator::{EmulatorCore, Input, MemoryRegionInfo, RunTime, ScreenData, ScreenDataEncoding};
use crate::{MonotonicTimestampProvider, TimestampMicros};

/// A Nintendo 3DS emulator.
///
/// Uses [Azahar](https://github.com/azahar-emu/azahar) as the underlying core (see
/// `azahar-rs`). Only one can exist per process; the game is loaded from its file (a decrypted
/// `.cci`/`.cxi`/`.3dsx`), and its saves live in Azahar's virtual SD card under `user_dir`.
pub struct Nintendo3DS {
    screens: [ScreenData; 2],
    rom_checksum: ReplayHeaderBlake3Hash,
    core: Core,
    last_frame_microseconds: TimestampMicros,
    microseconds_per_frames: TimestampMicros,
    clock: Box<dyn MonotonicTimestampProvider>,
    skip_drawing: bool,
    /// How many frames in a row have been drawn before this one. The 3DS shows a frame one VBlank
    /// or more after the game renders it, so the picture on the screens after a drawn frame was
    /// rendered by earlier frames: only when those were drawn too is there anything worth
    /// showing (see [`EmulatorCore::draw_lead_frames`]).
    drawn_in_a_row: u64,
    /// Whether the picture in `screens` is older than the emulation (frames were skipped), so the
    /// next drawn frame must be copied even if nothing else changed.
    stale: bool,
    stopped: bool
}

impl Nintendo3DS {
    /// Load a game from its file. `rom` is the file's contents (for the replay header's checksum
    /// only; Azahar reads the file itself), `user_dir` Azahar's user directory for this game.
    pub fn new_from_path(rom_path: &str, rom: &[u8], user_dir: &str, clock: Box<dyn MonotonicTimestampProvider>, settings: &Settings) -> Result<Self, String> {
        let path = CString::new(rom_path).map_err(|_| "ROM path contains a NUL byte".to_owned())?;
        let dir = CString::new(user_dir).map_err(|_| "user directory contains a NUL byte".to_owned())?;
        let core = Core::new(&path, &dir, settings).map_err(|e| alloc::format!("{e}"))?;
        Ok(Self {
            rom_checksum: blake3_hash(rom),
            screens: [
                ScreenData { pixels: alloc::vec![0xFF000000u32; azahar_rs::TOP_WIDTH * azahar_rs::TOP_HEIGHT], width: azahar_rs::TOP_WIDTH, height: azahar_rs::TOP_HEIGHT, encoding: ScreenDataEncoding::A8R8G8B8 },
                ScreenData { pixels: alloc::vec![0xFF000000u32; azahar_rs::BOTTOM_WIDTH * azahar_rs::BOTTOM_HEIGHT], width: azahar_rs::BOTTOM_WIDTH, height: azahar_rs::BOTTOM_HEIGHT, encoding: ScreenDataEncoding::A8R8G8B8 },
            ],
            core,
            last_frame_microseconds: 0,
            microseconds_per_frames: DEFAULT_MICROSECONDS_PER_FRAME,
            clock,
            skip_drawing: false,
            drawn_in_a_row: u64::MAX,
            stale: false,
            stopped: false
        })
    }

    /// Whether the emulated program stopped (crashed or shut itself down); frames no longer
    /// advance. [`EmulatorCore::hard_reset`] recovers.
    pub fn stopped(&self) -> bool {
        self.stopped
    }

    /// What stopped it, or the last error.
    pub fn last_error(&self) -> String {
        self.core.last_error()
    }

    /// The process heap (kind 1) or linear heap (kind 2) as one contiguous slice: the VMA that
    /// starts the region. Pokémon games allocate their heap as one block, which is what the
    /// sync hash covers.
    fn region_slice(&self, kind: u32) -> Option<&[u8]> {
        let regions = self.core.regions();
        let region = regions.iter().find(|r| r.kind == kind && !r.data.is_null())?;
        // SAFETY: the pointer is the backing memory of a mapped VMA, valid until the core next
        // runs, loads a state or resets, none of which can happen while `&self` is borrowed.
        Some(unsafe { core::slice::from_raw_parts(region.data, region.length as usize) })
    }
}

// 268111856 Hz / 4481136 cycles per frame = 59.8261 Hz
const DEFAULT_MICROSECONDS_PER_FRAME: u64 = 16_715;

const N3DS_REGION_HEAP: usize = 0;
const N3DS_REGION_LINEAR: usize = 1;

/// Nominal regions of the emulated program's address space. `read_ram`/`write_ram` go through
/// the process's page table and accept any mapped address, so these only name the ranges a
/// memory tool can expect to find something in; their true extents are whatever the game mapped.
const N3DS_MEMORY_REGIONS: [MemoryRegionInfo; 2] = [
    MemoryRegionInfo { name: "Process heap", short_name: "HEAP", base_address: 0x0800_0000, len: 0x0800_0000, default_big_endian: false, writable: true },
    MemoryRegionInfo { name: "Linear heap", short_name: "LINEAR", base_address: 0x1400_0000, len: 0x0800_0000, default_big_endian: false, writable: true },
];

/// The replay encoding of a 3DS input: little-endian button bits (Azahar's `NativeButton`
/// order), the two sticks, and the touch point with a flag.
fn encode(input: &Input) -> [u8; 12] {
    let mut buttons = 0u32;
    buttons |= (input.a as u32) * InputState::A;
    buttons |= (input.b as u32) * InputState::B;
    buttons |= (input.x as u32) * InputState::X;
    buttons |= (input.y as u32) * InputState::Y;
    buttons |= (input.d_up as u32) * InputState::UP;
    buttons |= (input.d_down as u32) * InputState::DOWN;
    buttons |= (input.d_left as u32) * InputState::LEFT;
    buttons |= (input.d_right as u32) * InputState::RIGHT;
    buttons |= (input.l as u32) * InputState::L;
    buttons |= (input.r as u32) * InputState::R;
    buttons |= (input.start as u32) * InputState::START;
    buttons |= (input.select as u32) * InputState::SELECT;
    buttons |= (input.zl as u32) * InputState::ZL;
    buttons |= (input.zr as u32) * InputState::ZR;

    // No analog source yet in the frontend: the d-pad doubles as a fully deflected circle pad,
    // which is how most 3DS games expect to be walked.
    let circle = if input.circle != (0, 0) {
        input.circle
    } else {
        let x = (input.d_right as i8 - input.d_left as i8) * 127;
        let y = (input.d_up as i8 - input.d_down as i8) * 127;
        (x, y)
    };

    let mut out = [0u8; 12];
    out[..4].copy_from_slice(&buttons.to_le_bytes());
    out[4] = circle.0 as u8;
    out[5] = circle.1 as u8;
    out[6] = input.c_stick.0 as u8;
    out[7] = input.c_stick.1 as u8;
    // [8] = touching, [9] = y (0..=239 fits a byte), [10..12] = x (0..=319).
    if let Some((x, y)) = input.touch {
        out[8] = 1;
        out[9] = y.min(239) as u8;
        out[10..12].copy_from_slice(&x.min(319).to_le_bytes());
    }
    out
}

fn decode(bytes: &[u8]) -> InputState {
    let mut state = InputState::default();
    if bytes.len() < 12 {
        return state;
    }
    state.buttons = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    state.circle_x = bytes[4] as i8;
    state.circle_y = bytes[5] as i8;
    state.c_stick_x = bytes[6] as i8;
    state.c_stick_y = bytes[7] as i8;
    state.touch_pressed = bytes[8] != 0;
    state.touch_y = bytes[9] as u16;
    state.touch_x = u16::from_le_bytes([bytes[10], bytes[11]]);
    state
}

#[allow(unused_variables)]
impl EmulatorCore for Nintendo3DS {
    fn run(&mut self) -> RunTime {
        let expected_next = self.last_frame_microseconds + self.microseconds_per_frames;
        let now = self.clock.get_timestamp_microseconds();
        if now < expected_next {
            return RunTime::NONE
        }

        let rval = self.run_unlocked();

        let several_frames_ago = self.clock
            .get_timestamp_microseconds()
            .saturating_sub(self.microseconds_per_frames * 16);
        self.last_frame_microseconds = (self.last_frame_microseconds + self.microseconds_per_frames)
            .max(several_frames_ago);

        rval
    }

    fn run_unlocked(&mut self) -> RunTime {
        if self.stopped {
            return RunTime::NONE
        }
        let render = !self.skip_drawing;
        // What the screens show after this frame was rendered a frame or more earlier.
        let present = render && self.drawn_in_a_row >= self.draw_lead_frames();
        if !self.core.run_frame(!render) {
            self.stopped = true;
            return RunTime::NONE
        }
        self.drawn_in_a_row = if render { self.drawn_in_a_row.saturating_add(1) } else { 0 };
        if !present {
            self.stale = true;
            self.drawn_in_a_row = u64::MAX;
            return RunTime { frames: 1, presented: false }
        }
        self.screens[0].pixels.copy_from_slice(self.core.pixels(0));
        self.screens[1].pixels.copy_from_slice(self.core.pixels(1));
        self.stale = false;
        RunTime::ONE_FRAME
    }

    fn set_skip_drawing(&mut self, skip: bool) {
        self.skip_drawing = skip;
    }

    fn draw_lead_frames(&self) -> u64 {
        // Every frame: measured on Pokémon X, with one frame in three or four skipped the bottom
        // screen (redrawn by the game only on some frames) strobed between its picture and the
        // cleared buffer, whatever the number of drawn frames before each presented one.
        // SUPERSHUCKIE_3DS_DRAW_LEAD overrides it for measurement.
        #[cfg(feature = "std")]
        {
            static LEAD: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
            return *LEAD.get_or_init(|| std::env::var("SUPERSHUCKIE_3DS_DRAW_LEAD").ok().and_then(|v| v.parse().ok()).unwrap_or(u64::MAX));
        }
        #[allow(unreachable_code)]
        u64::MAX
    }

    fn microseconds_until_next_frame(&mut self) -> Option<u64> {
        let expected_next = self.last_frame_microseconds + self.microseconds_per_frames;
        Some(expected_next.saturating_sub(self.clock.get_timestamp_microseconds()))
    }

    fn frame_period_microseconds(&self) -> Option<u64> {
        Some(self.microseconds_per_frames)
    }

    fn read_ram(&self, address: u32, into: &mut [u8]) -> Result<(), &'static str> {
        self.core.read_memory(address, into).then_some(()).ok_or("address is not mapped")
    }

    fn write_ram(&mut self, address: u32, from: &[u8]) -> Result<(), &'static str> {
        self.core.write_memory(address, from).then_some(()).ok_or("address is not mapped")
    }

    fn memory_regions(&self) -> &[MemoryRegionInfo] {
        &N3DS_MEMORY_REGIONS
    }

    fn memory_region_data(&self, index: usize) -> Option<&[u8]> {
        match index {
            N3DS_REGION_HEAP => self.region_slice(1),
            N3DS_REGION_LINEAR => self.region_slice(2),
            _ => None
        }
    }

    fn set_speed(&mut self, speed: f64) {
        self.microseconds_per_frames = (DEFAULT_MICROSECONDS_PER_FRAME as f64 / speed).clamp(0.0, u32::MAX as f64) as u64;
    }

    fn save_sram(&self) -> Vec<u8> {
        // Saves are files on Azahar's virtual SD card, written by the game itself.
        Vec::new()
    }

    fn create_save_state(&self) -> Vec<u8> {
        let mut v = Vec::new();
        self.create_save_state_into(&mut v);
        v
    }

    fn create_save_state_into(&self, into: &mut Vec<u8>) {
        if !self.core.save_state_into(into) {
            into.clear();
        }
    }

    fn save_state_possible(&self) -> bool {
        !self.core.state_pending()
    }

    fn load_save_state(&mut self, state: &[u8]) -> Result<(), String> {
        if self.core.load_state(state) {
            self.stopped = false;
            self.stale = true;
            self.drawn_in_a_row = u64::MAX;
            Ok(())
        } else {
            Err(alloc::format!("failed to load 3DS save state: {}", self.core.last_error()))
        }
    }

    fn encode_input(&self, input: Input, into: &mut Vec<u8>) {
        into.clear();
        into.extend_from_slice(&encode(&input));
    }

    fn set_input_encoded(&mut self, input: &[u8]) {
        self.core.set_input(&decode(input));
    }

    fn get_screens(&self) -> &[ScreenData] {
        &self.screens
    }

    fn swap_screen_data(&mut self, screens: &mut [ScreenData]) {
        assert_eq!(screens.len(), self.screens.len(), "expected two screens to be swapped");
        for (a, b) in self.screens.iter_mut().zip(screens.iter_mut()) {
            core::mem::swap(&mut a.pixels, &mut b.pixels)
        }
    }

    fn hard_reset(&mut self) {
        self.stopped = !self.core.reset();
        self.stale = true;
    }

    fn replay_console_type(&self) -> Option<ReplayConsoleType> {
        Some(ReplayConsoleType::Nintendo3DS)
    }

    fn rom_checksum(&self) -> &ReplayHeaderBlake3Hash {
        &self.rom_checksum
    }

    fn bios_checksum(&self) -> &ReplayHeaderBlake3Hash {
        &const { unsafe { core::mem::zeroed() } }
    }

    fn core_name(&self) -> &'static str {
        "Azahar [SUPERSHUCKIE-EXPERIMENTAL-0]"
    }

    #[inline]
    fn frame_rate(&self) -> (u32, u32) {
        azahar_rs::FRAME_RATE
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_round_trips_through_the_replay_encoding() {
        let mut input = Input::new();
        input.a = true;
        input.zr = true;
        input.d_left = true;
        input.touch = Some((319, 239));
        let state = decode(&encode(&input));
        assert_eq!(state.buttons, InputState::A | InputState::ZR | InputState::LEFT);
        assert_eq!((state.circle_x, state.circle_y), (-127, 0));
        assert!(state.touch_pressed);
        assert_eq!((state.touch_x, state.touch_y), (319, 239));

        input.circle = (40, -3);
        let state = decode(&encode(&input));
        assert_eq!((state.circle_x, state.circle_y), (40, -3));
    }
}
