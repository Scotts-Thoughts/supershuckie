use crate::emulator::{EmulatorCore, Input, RunTime, ScreenData, ScreenDataEncoding, AUDIO_SAMPLE_RATE};
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, Ordering};
use safeboy::rgb_encoder::encode_a8r8g8b8;
use safeboy::{BorderMode, DirectAccessRegion, Gameboy, GameboyCallbacks, InputButton, RtcMode, RunnableInstanceFunctions, RunningGameboy, TurboMode, VBlankType};
pub use safeboy::Model;
use spin::Lazy;
use supershuckie_replay_recorder::blake3_hash;
use supershuckie_replay_recorder::replay_file::{ReplayConsoleType, ReplayHeaderBlake3Hash};

/// Game Boy and Game Boy Color emulator.
///
/// Uses [SameBoy](https://sameboy.github.io) as the underlying core.
///
/// # Audio
///
/// SameBoy only renders samples once a sample rate is set, and setting one is not free of side
/// effects: the APU is then run lazily in batches sized by the sample rate, and the joypad-bounce
/// emulation mixes the "not yet run" APU cycle count into its pseudo-random decision, so joypad
/// reads inside a bounce window come out differently depending on the sample rate (`joypad.c`,
/// `should_bounce`/`semi_random`). Every replay recorded so far was made with no sample rate, and
/// the emulated instance must stay that way for them to keep playing back frame-exact.
///
/// So the instance the frontend sees (`core`) never gets a sample rate. When audio is on, a
/// second, [`ShadowAudio`] instance of the same ROM runs one `GB_run` behind it in lockstep at
/// [`AUDIO_SAMPLE_RATE`] and only its samples are used; it is resynced from the emulated
/// instance's state the moment its instruction stream drifts (which the bounce quirk can cause).
pub struct GameBoyColor {
    core: Gameboy,
    turbo_mode: TurboMode,
    callback_data: Arc<GameBoyCallbackData>,

    rom_checksum: ReplayHeaderBlake3Hash,
    bios_checksum: ReplayHeaderBlake3Hash,

    /// What the shadow needs to be built: the same ROM, boot ROM and model.
    rom: Vec<u8>,
    bios: Vec<u8>,
    model: Model,

    /// The last button mask handed to the emulated instance, mirrored to the shadow.
    input_mask: u8,
    /// The last clock multiplier, mirrored to the shadow.
    speed: f64,

    /// Cycles the emulated instance has run since the shadow was last aligned with it.
    cycles: u64,

    shadow: Option<ShadowAudio>
}

struct GameBoyCallbackData {
    run_frames: AtomicU32,
    screen: UnsafeCell<ScreenData>
}

unsafe impl Send for GameBoyCallbackData {}
unsafe impl Sync for GameBoyCallbackData {}

/// See [`GameBoyColor`]'s audio notes.
struct ShadowAudio {
    gb: Gameboy,
    data: Arc<ShadowCallbackData>,
    /// Cycles run since the last alignment; compared with `GameBoyColor::cycles` after every step.
    cycles: u64,
    /// Emulated frames since the last full-state comparison.
    frames_since_check: u32,
    /// How many times the shadow had to be resynced from the emulated instance.
    resyncs: u64
}

struct ShadowCallbackData {
    /// Interleaved stereo samples rendered since the last `take_audio`.
    audio: UnsafeCell<Vec<i16>>
}

unsafe impl Send for ShadowCallbackData {}
unsafe impl Sync for ShadowCallbackData {}

struct ShadowCallbackHandler {
    data: Arc<ShadowCallbackData>
}

impl GameboyCallbacks for ShadowCallbackHandler {
    fn apu_sample(&mut self, _instance: &mut RunningGameboy, left: i16, right: i16) {
        // SAFETY: The shadow is only ever run while its owner is mutably borrowed.
        let audio = unsafe { &mut *self.data.audio.get() };
        audio.push(left);
        audio.push(right);
    }
}

impl ShadowAudio {
    /// Emulated frames between full-state comparisons, a backstop for a divergence that did
    /// not change the instruction stream's cycle counts (which is caught immediately).
    const CHECK_EVERY_FRAMES: u32 = 60;

    fn new(rom: &[u8], bios: &[u8], model: Model, speed: f64) -> Self {
        let mut gb = Gameboy::new(model);
        gb.set_rtc_mode(RtcMode::Accurate);
        gb.load_boot_rom(bios);
        gb.load_rom(rom);
        // Nobody looks at its screen.
        gb.set_rendering_enabled(false);
        gb.set_border_mode(BorderMode::Never);
        // Never sleep to pace itself; the emulated instance does the pacing.
        gb.set_turbo_mode(TurboMode::Enabled);
        gb.set_clock_multiplier(speed);
        gb.set_sample_rate(AUDIO_SAMPLE_RATE);

        let data = Arc::new(ShadowCallbackData { audio: UnsafeCell::new(Vec::new()) });
        gb.set_callbacks(Some(Box::new(ShadowCallbackHandler { data: data.clone() })));

        Self { gb, data, cycles: 0, frames_since_check: 0, resyncs: 0 }
    }

    fn audio(&mut self) -> &mut Vec<i16> {
        // SAFETY: `self` is mutably borrowed, so the shadow cannot be running.
        unsafe { &mut *self.data.audio.get() }
    }

    /// Make the shadow a copy of `main`. Both cycle counters restart from zero: the caller
    /// resets its own.
    fn sync_from(&mut self, main: &Gameboy, input_mask: u8) {
        let mut state = main.create_save_state();
        // The emulated instance runs its APU lazily in batches of up to 1024 cycles; the shadow,
        // with a sample rate, batches by ~44 and asserts on more than ~175 pending at once. Drop
        // the pending count: the shadow's APU then sits at most a quarter millisecond behind.
        zero_pending_apu_cycles(&mut state);
        let _ = self.gb.load_save_state(&state);
        self.gb.set_input_button_mask(input_mask);
        self.cycles = 0;
        self.frames_since_check = 0;
    }
}

impl GameBoyColor {
    /// Instantiate a `GameBoyColor` emulator from the given ROM.
    pub fn new_from_rom(
        rom: &[u8],
        bios: &[u8],
        sram: Option<&[u8]>,
        model: Model
    ) -> Self {
        let mut core = Gameboy::new(model);
        core.set_rtc_mode(RtcMode::Accurate);
        core.load_boot_rom(bios);
        core.load_rom(rom);

        if let Some(sram) = sram {
            core.load_sram(sram);
        };

        core.set_rgb_encoder(encode_a8r8g8b8);
        core.set_rendering_enabled(true);
        core.set_border_mode(BorderMode::Never);

        let dimensions = core.get_pixel_buffer();
        let screen_data = ScreenData {
            pixels: dimensions.pixels.to_owned(),
            width: dimensions.width as usize,
            height: dimensions.height as usize,
            encoding: ScreenDataEncoding::A8R8G8B8
        };

        let callback_data = Arc::new(GameBoyCallbackData {
            run_frames: AtomicU32::new(0),
            screen: UnsafeCell::new(screen_data)
        });

        core.set_callbacks(Some(Box::new(CallbackHandler { callback_data: callback_data.clone() })));

        let mut r = Self {
            turbo_mode: TurboMode::Disabled,
            callback_data,
            core,
            rom_checksum: blake3_hash(rom),
            bios_checksum: blake3_hash(bios),
            rom: rom.to_vec(),
            bios: bios.to_vec(),
            model,
            input_mask: 0,
            speed: 1.0,
            cycles: 0,
            shadow: None
        };
        r.hard_reset();
        r
    }

    /// How many times the audio shadow has been resynced from the emulated instance (see the
    /// type's audio notes); zero while audio is off. For diagnostics.
    pub fn audio_resyncs(&self) -> u64 {
        self.shadow.as_ref().map(|s| s.resyncs).unwrap_or(0)
    }

    /// Step the emulated instance once and keep the shadow, if any, in lockstep with it.
    fn step(&mut self) -> RunTime {
        let cycles = self.core.run() as u64;
        self.cycles += cycles;
        let frames = self.callback_data.run_frames.swap(0, Ordering::Relaxed) as u64;

        if let Some(shadow) = self.shadow.as_mut() {
            // Same instruction stream, same step sizes: one GB_run each keeps them exactly
            // aligned, so a cycle count that differs means the shadow took another path (the
            // joypad-bounce quirk); a state check every so often is the backstop for a
            // divergence that did not change the step sizes.
            shadow.cycles += shadow.gb.run() as u64;
            shadow.frames_since_check += frames as u32;

            let mut diverged = shadow.cycles != self.cycles;
            if !diverged && shadow.frames_since_check >= ShadowAudio::CHECK_EVERY_FRAMES {
                shadow.frames_since_check = 0;
                diverged = !same_observable_state(&self.core, &shadow.gb);
            }
            if diverged {
                shadow.resyncs += 1;
                shadow.sync_from(&self.core, self.input_mask);
                self.cycles = 0;
            }
        }

        RunTime { frames, presented: frames > 0 }
    }

    /// Bring the shadow back to the emulated instance's state after something other than a
    /// step changed it (a state load, a reset).
    fn resync_shadow(&mut self) {
        if let Some(shadow) = self.shadow.as_mut() {
            shadow.sync_from(&self.core, self.input_mask);
            self.cycles = 0;
        }
    }
}

/// Zero `GB_apu_t::apu_cycles` in a SameBoy save state: an 8-byte header, then sections each
/// prefixed by a 4-byte size, `apu` being the sixth; `apu_cycles` is the `u16` at its offset 2.
fn zero_pending_apu_cycles(state: &mut [u8]) {
    let mut offset = 8usize;
    for section in 0..6 {
        let Some(size) = state.get(offset..offset + 4) else { return };
        let size = u32::from_le_bytes(size.try_into().unwrap()) as usize;
        offset += 4;
        if section == 5 {
            if let Some(bytes) = state.get_mut(offset + 2..offset + 4) {
                bytes.fill(0);
            }
            return;
        }
        offset += size;
    }
}

/// Whether the game-visible memory of two instances at the same cycle matches.
fn same_observable_state(a: &Gameboy, b: &Gameboy) -> bool {
    for region in [DirectAccessRegion::RAM, DirectAccessRegion::HRAM, DirectAccessRegion::OAM, DirectAccessRegion::VRAM] {
        if a.direct_access(region).data != b.direct_access(region).data {
            return false;
        }
    }
    let ra = a.get_registers();
    let rb = b.get_registers();
    ra.pc == rb.pc && ra.sp == rb.sp && ra.af == rb.af && ra.bc == rb.bc && ra.de == rb.de && ra.hl == rb.hl
}

struct CallbackHandler {
    callback_data: Arc<GameBoyCallbackData>
}

impl GameboyCallbacks for CallbackHandler {
    fn vblank(&mut self, instance: &mut RunningGameboy, _vblank_type: VBlankType) {
        // SAFETY: Nothing else can currently access this Arc since GameBoyColor is currently
        //         mutably borrowed.
        let screen = unsafe { &mut *self.callback_data.screen.get() };

        screen.pixels.copy_from_slice(instance.get_pixel_buffer_pixels());
        self.callback_data.run_frames.fetch_add(1, Ordering::Relaxed);
    }
}

/// Returns the region and offset.
fn pokeabyte_protocol_region_from_address(address: u32) -> Option<(DirectAccessRegion, usize)> {
    match address {
        // VRAM
        0x8000..=0x9FFF => Some((DirectAccessRegion::VRAM, address as usize - 0x8000)),

        // WRAM bank #0
        0xC000..=0xDFFF => Some((DirectAccessRegion::RAM, address as usize - 0xC000)),

        // WRAM bank #1 (not the actual address)
        0x10000..=0x11FFF => Some((DirectAccessRegion::RAM, address as usize - 0x10000 + 0x2000)),

        // HRAM
        0xFF80..=0xFFFE => Some((DirectAccessRegion::HRAM, address as usize - 0xFF80)),

        _ => None
    }
}

impl EmulatorCore for GameBoyColor {
    fn run(&mut self) -> RunTime {
        self.step()
    }

    fn run_unlocked(&mut self) -> RunTime {
        self.core.set_turbo_mode(TurboMode::Enabled);
        let timing = self.run();
        self.core.set_turbo_mode(self.turbo_mode);
        timing
    }

    fn read_ram(&self, address: u32, into: &mut [u8]) -> Result<(), &'static str> {
        let Some((region, offset)) = pokeabyte_protocol_region_from_address(address) else {
            return Err("invalid or unknown address");
        };
        let Some(offset_end) = offset.checked_add(into.len()) else {
            return Err("invalid length");
        };

        let region = self.core.direct_access(region);
        let Some(data) = region.data.get(offset..offset_end) else {
            return Err("address+length overflows");
        };
        into.copy_from_slice(data);
        Ok(())
    }

    fn write_ram(&mut self, address: u32, from: &[u8]) -> Result<(), &'static str> {
        let Some((region, offset)) = pokeabyte_protocol_region_from_address(address) else {
            return Err("invalid or unknown address");
        };
        let Some(offset_end) = offset.checked_add(from.len()) else {
            return Err("invalid length");
        };
        let region = self.core.direct_access_mut(region);
        let Some(data) = region.data.get_mut(offset..offset_end) else {
            return Err("address+length overflows");
        };
        data.copy_from_slice(from);
        Ok(())
    }

    #[inline]
    fn set_speed(&mut self, speed: f64) {
        self.speed = speed;
        self.core.set_clock_multiplier(speed);
        if let Some(shadow) = self.shadow.as_mut() {
            // SameBoy re-derives its sample timing from the clock rate, so the shadow keeps
            // producing AUDIO_SAMPLE_RATE samples per real second, pitched with the speed.
            shadow.gb.set_clock_multiplier(speed);
        }
    }

    fn set_audio_enabled(&mut self, enabled: bool) {
        if enabled == self.shadow.is_some() {
            return
        }
        if enabled {
            let mut shadow = ShadowAudio::new(&self.rom, &self.bios, self.model, self.speed);
            shadow.sync_from(&self.core, self.input_mask);
            self.cycles = 0;
            self.shadow = Some(shadow);
        }
        else {
            self.shadow = None;
        }
    }

    fn take_audio(&mut self, into: &mut Vec<i16>) {
        if let Some(shadow) = self.shadow.as_mut() {
            into.append(shadow.audio());
        }
    }

    fn save_sram(&self) -> Vec<u8> {
        self.core.save_sram()
    }

    fn create_save_state(&self) -> Vec<u8> {
        self.core.create_save_state()
    }

    fn load_save_state(&mut self, state: &[u8]) -> Result<(), String> {
        let r = self.core.load_save_state(state).map_err(|e| alloc::format!("{e:?}"));
        self.resync_shadow();
        r
    }

    fn encode_input(&self, input: Input, into: &mut Vec<u8>) {
        let mask = (input.a as u8) << InputButton::A
            | (input.b as u8) << InputButton::B
            | (input.start as u8) << InputButton::Start
            | (input.select as u8) << InputButton::Select
            | (input.d_up as u8) << InputButton::Up
            | (input.d_down as u8) << InputButton::Down
            | (input.d_left as u8) << InputButton::Left
            | (input.d_right as u8) << InputButton::Right;
        into.push(mask);
    }

    #[inline]
    fn set_input_encoded(&mut self, input: &[u8]) {
        debug_assert!(input.len() == 1, "set_input_encoded with wrong number of bytes {}", input.len());
        self.input_mask = input[0];
        self.core.set_input_button_mask(input[0]);
        if let Some(shadow) = self.shadow.as_mut() {
            shadow.gb.set_input_button_mask(input[0]);
        }
    }

    #[inline]
    fn get_screens(&self) -> &[ScreenData] {
        // SAFETY: This is going to return a reference with the same lifetime as `self`, thus once
        //         we have to mutably borrow again, the borrow will end.
        let screen_data = unsafe { &*self.callback_data.screen.get() };
        core::slice::from_ref(screen_data)
    }

    #[inline]
    fn swap_screen_data(&mut self, screens: &mut [ScreenData]) {
        assert_eq!(screens.len(), 1, "Invalid screen count");
        let first_screen = &mut screens[0];

        // SAFETY: This won't leave this function.
        let screen_data = unsafe { &mut *self.callback_data.screen.get() };

        assert_eq!(first_screen.pixels.len(), screen_data.pixels.len());
        core::mem::swap(&mut first_screen.pixels, &mut screen_data.pixels);
    }

    #[inline]
    fn hard_reset(&mut self) {
        self.core.reset();

        // skip the intro
        if self.core.is_hle_sgb() {
            let mut state = self.core.create_save_state();
            state[0x1AB66] = 201;
            state[0x1AB67] = 0;
            let _ = self.core.load_save_state(&state);
        }

        self.resync_shadow();
    }

    fn replay_console_type(&self) -> Option<ReplayConsoleType> {
        match self.core.is_cgb() {
            true => Some(ReplayConsoleType::GameBoyColor),
            false => match self.core.is_sgb() {
                false => Some(ReplayConsoleType::GameBoy),
                true => Some(ReplayConsoleType::SuperGameBoy2)
            }
        }
    }

    #[inline]
    fn rom_checksum(&self) -> &ReplayHeaderBlake3Hash {
        &self.rom_checksum
    }

    #[inline]
    fn bios_checksum(&self) -> &ReplayHeaderBlake3Hash {
        &self.bios_checksum
    }

    #[inline]
    fn core_name(&self) -> &'static str {
        if self.core.is_hle_sgb() {
            GB_VERSION_WITH_HACKS.as_str()
        }
        else {
            safeboy::GB_VERSION
        }
    }

    #[inline]
    fn frame_rate(&self) -> (u32, u32) {
        // GB/GBC: 4194304 Hz CPU clock / 70224 dots per frame ~= 59.7275 Hz.
        (4194304, 70224)
    }
}

static GB_VERSION_WITH_HACKS: Lazy<String> = Lazy::new(|| {
    alloc::format!("{} with SGB intro skipped", safeboy::GB_VERSION)
});
