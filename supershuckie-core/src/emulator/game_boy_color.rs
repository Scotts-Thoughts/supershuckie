use crate::emulator::link::{GbSerialEvent, GbSerialEvents, LinkError, LinkPort};
use crate::emulator::{locate_memory, read_ram_from_regions, EmulatorCore, Input, MemoryRegionInfo, RunTime, ScreenData, ScreenDataEncoding, AUDIO_SAMPLE_RATE};
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::collections::VecDeque;
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
///
/// # Link cable
///
/// The console's serial port is a [`LinkPort`] (see [`crate::emulator::link`]). Live, the
/// partner's SameBoy instance is reachable from this instance's serial callbacks for the duration
/// of one [`LinkPort::step_linked`]: a bit this console clocks out is shifted into the partner
/// (and the partner's outgoing bit read) right there, the way SameBoy's own frontend links two
/// windows. Everything received is logged per frame as [`GbSerialEvents`]; in replay mode the same
/// events are fed back at the same points of the instruction stream. The audio shadow gets the
/// same bits its emulated instance got, at the same step boundaries, so it never diverges over a
/// transfer.
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

    shadow: Option<ShadowAudio>,

    /// Memory regions (see [`GameBoyColor::memory_regions`]) and where each one's bytes live.
    regions: Vec<MemoryRegionInfo>,
    region_sources: Vec<RegionSource>,

    /// Whether the last [`Self::step`] stopped inside a frame (see
    /// [`EmulatorCore::is_mid_frame`]).
    mid_frame: bool
}

/// Where a [`MemoryRegionInfo`] of a [`GameBoyColor`] is backed: `len` bytes of a SameBoy direct
/// access region, starting at `offset`.
#[derive(Copy, Clone)]
struct RegionSource {
    region: DirectAccessRegion,
    offset: usize
}

struct GameBoyCallbackData {
    run_frames: AtomicU32,
    screen: UnsafeCell<ScreenData>,
    /// The link port's state; only ever touched from the thread stepping the core (by
    /// [`GameBoyColor`] itself and by the serial callbacks it triggers, including the partner's).
    link: UnsafeCell<GbLinkState>
}

unsafe impl Send for GameBoyCallbackData {}
unsafe impl Sync for GameBoyCallbackData {}

/// What the port is doing (see [`LinkPort`]).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum LinkMode {
    /// No cable: the serial callbacks are not installed.
    Off,
    /// Plugged into a partner instance, reached through [`GbLinkState::partner`] during a step.
    Live,
    /// Fed what a live instance once received.
    Replay
}

/// The state of the serial port as a link cable end.
struct GbLinkState {
    mode: LinkMode,

    /// For the duration of one [`LinkPort::step_linked`]: the partner's pinned instance and its
    /// callback data (to log the bits shifted into it). Null pointers otherwise.
    partner: *mut RunningGameboy,
    partner_data: *const GameBoyCallbackData,

    /// The bit announced by the last `serial_transfer_bit_start`, sent at the next bit end.
    bit_to_send: bool,

    /// Emulated 8 MHz cycles since [`LinkPort::connect`].
    link_time: u64,

    /// Emulated 8 MHz cycles since the last step that completed a frame: the position slave bits
    /// are logged at (they arrive between this console's steps, from the partner's).
    link_cycles: u64,

    /// Events received since the log was last taken (see [`LinkPort::take_serial_in`]).
    log: Vec<GbSerialEvent>,

    /// Replay mode: master bits still to hand to the callbacks, in order.
    replay_master: VecDeque<bool>,

    /// Replay mode: slave bits still to shift in, as `(link_cycles, bit)` in cycle order.
    replay_slave: VecDeque<(u64, bool)>,

    /// Replay mode: whether the recording says a cable was plugged in (an empty master queue is
    /// then a miss, not "no cable").
    replay_connected: bool,

    /// See [`LinkPort::serial_replay_misses`].
    misses: u64,

    /// Bits the master callbacks received during the current step: the shadow's instance gets
    /// them queued before its own step.
    step_master_bits: Vec<bool>,

    /// Bits shifted into the emulated instance as a slave since the shadow last stepped (live
    /// mode: by the partner, between this console's steps); applied to the shadow before its
    /// next step.
    shadow_pending_slave: Vec<bool>
}

impl GbLinkState {
    fn new() -> Self {
        Self {
            mode: LinkMode::Off,
            partner: core::ptr::null_mut(),
            partner_data: core::ptr::null(),
            bit_to_send: false,
            link_time: 0,
            link_cycles: 0,
            log: Vec::new(),
            replay_master: VecDeque::new(),
            replay_slave: VecDeque::new(),
            replay_connected: false,
            misses: 0,
            step_master_bits: Vec::new(),
            shadow_pending_slave: Vec::new()
        }
    }
}

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
    audio: UnsafeCell<Vec<i16>>,
    /// Master bits the emulated instance received this step, for the shadow's own transfer.
    master_bits: UnsafeCell<VecDeque<bool>>,
    /// Whether the shadow asked for a master bit the queue did not have (a divergence).
    missed: UnsafeCell<bool>
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

    fn serial_transfer_bit_end(&mut self, _instance: &mut RunningGameboy) -> bool {
        // SAFETY: as above.
        let bits = unsafe { &mut *self.data.master_bits.get() };
        match bits.pop_front() {
            Some(bit) => bit,
            None => {
                // SAFETY: as above.
                unsafe { *self.data.missed.get() = true };
                true
            }
        }
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

        let data = Arc::new(ShadowCallbackData {
            audio: UnsafeCell::new(Vec::new()),
            master_bits: UnsafeCell::new(VecDeque::new()),
            missed: UnsafeCell::new(false)
        });
        gb.set_callbacks(Some(Box::new(ShadowCallbackHandler { data: data.clone() })));

        Self { gb, data, cycles: 0, frames_since_check: 0, resyncs: 0 }
    }

    fn audio(&mut self) -> &mut Vec<i16> {
        // SAFETY: `self` is mutably borrowed, so the shadow cannot be running.
        unsafe { &mut *self.data.audio.get() }
    }

    fn master_bits(&mut self) -> &mut VecDeque<bool> {
        // SAFETY: as above.
        unsafe { &mut *self.data.master_bits.get() }
    }

    fn take_missed(&mut self) -> bool {
        // SAFETY: as above.
        core::mem::take(unsafe { &mut *self.data.missed.get() })
    }

    /// Make the shadow a copy of `main`, with the same link cable situation. Both cycle
    /// counters restart from zero: the caller resets its own.
    fn sync_from(&mut self, main: &Gameboy, input_mask: u8, link_mode: LinkMode) {
        let mut state = main.create_save_state();
        // The emulated instance runs its APU lazily in batches of up to 1024 cycles; the shadow,
        // with a sample rate, batches by ~44 and asserts on more than ~175 pending at once. Drop
        // the pending count: the shadow's APU then sits at most a quarter millisecond behind.
        zero_pending_apu_cycles(&mut state);
        let _ = self.gb.load_save_state(&state);
        self.gb.set_input_button_mask(input_mask);
        // The callbacks are not part of a state; the shadow talks to nobody but reads the bits
        // its instance got, so it needs the callbacks exactly when the emulated instance has them.
        if link_mode == LinkMode::Off {
            self.gb.disconnect_serial();
        }
        else {
            self.gb.connect_serial();
        }
        self.master_bits().clear();
        self.take_missed();
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
            screen: UnsafeCell::new(screen_data),
            link: UnsafeCell::new(GbLinkState::new())
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
            shadow: None,
            regions: Vec::new(),
            region_sources: Vec::new(),
            mid_frame: false
        };
        r.hard_reset();
        (r.regions, r.region_sources) = build_memory_regions(&r.core);
        r
    }

    /// How many times the audio shadow has been resynced from the emulated instance (see the
    /// type's audio notes); zero while audio is off. For diagnostics.
    pub fn audio_resyncs(&self) -> u64 {
        self.shadow.as_ref().map(|s| s.resyncs).unwrap_or(0)
    }

    fn link(&mut self) -> &mut GbLinkState {
        // SAFETY: `self` is mutably borrowed, so neither instance is running and no callback
        // (this instance's or a partner's) can be touching the state.
        unsafe { &mut *self.callback_data.link.get() }
    }

    fn link_ref(&self) -> &GbLinkState {
        // SAFETY: as above; nothing runs without a mutable borrow.
        unsafe { &*self.callback_data.link.get() }
    }

    /// Step the emulated instance once and keep the shadow, if any, in lockstep with it.
    fn step(&mut self) -> RunTime {
        let link_mode = self.link().mode;
        debug_assert!(
            link_mode != LinkMode::Live || !self.link().partner.is_null(),
            "a live link port must be stepped through LinkPort::step_linked"
        );

        // Slave bits the partner shifted into the emulated instance since the last step (live
        // mode); the shadow gets them before its own step, like the emulated instance did.
        let shadow_slave = core::mem::take(&mut self.link().shadow_pending_slave);
        self.link().step_master_bits.clear();

        let cycles = self.core.run() as u64;
        self.cycles += cycles;
        let frames = self.callback_data.run_frames.swap(0, Ordering::Relaxed) as u64;
        self.mid_frame = frames == 0;
        debug_assert!(frames <= 1, "one GB_run completed {frames} frames");

        // Link time bookkeeping, then the slave bits a recording says arrived at this boundary.
        let mut applied_now: Vec<bool> = Vec::new();
        if link_mode != LinkMode::Off {
            let link = self.link();
            link.link_time += cycles;
            link.link_cycles += cycles;
            if link_mode == LinkMode::Replay {
                let at = link.link_cycles;
                while let Some(&(cycles_at, bit)) = link.replay_slave.front() {
                    if cycles_at > at && frames == 0 {
                        break
                    }
                    // Past its moment (or the frame is over and it never came due): applied late
                    // rather than dropped, so the byte count the game sees stays right.
                    if cycles_at != at {
                        link.misses += 1;
                    }
                    link.replay_slave.pop_front();
                    applied_now.push(bit);
                }
                for bit in &applied_now {
                    self.core.serial_set_data_bit(*bit);
                }
            }
            if frames > 0 {
                self.link().link_cycles = 0;
            }
        }

        if let Some(shadow) = self.shadow.as_mut() {
            // Same instruction stream, same step sizes: one GB_run each keeps them exactly
            // aligned, so a cycle count that differs means the shadow took another path (the
            // joypad-bounce quirk); a state check every so often is the backstop for a
            // divergence that did not change the step sizes.
            for bit in shadow_slave {
                shadow.gb.serial_set_data_bit(bit);
            }
            // SAFETY: `self` is mutably borrowed (see `link`).
            let step_master_bits = unsafe { &(*self.callback_data.link.get()).step_master_bits };
            shadow.master_bits().extend(step_master_bits.iter().copied());
            shadow.cycles += shadow.gb.run() as u64;
            shadow.frames_since_check += frames as u32;
            for bit in &applied_now {
                shadow.gb.serial_set_data_bit(*bit);
            }

            let mut diverged = shadow.cycles != self.cycles || shadow.take_missed() || !shadow.master_bits().is_empty();
            if !diverged && shadow.frames_since_check >= ShadowAudio::CHECK_EVERY_FRAMES {
                shadow.frames_since_check = 0;
                diverged = !same_observable_state(&self.core, &shadow.gb);
            }
            if diverged {
                shadow.resyncs += 1;
                shadow.sync_from(&self.core, self.input_mask, link_mode);
                self.cycles = 0;
            }
        }

        RunTime { frames, presented: frames > 0 }
    }

    /// Bring the shadow back to the emulated instance's state after something other than a
    /// step changed it (a state load, a reset).
    fn resync_shadow(&mut self) {
        let link_mode = self.link().mode;
        if let Some(shadow) = self.shadow.as_mut() {
            shadow.sync_from(&self.core, self.input_mask, link_mode);
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

    fn serial_transfer_bit_start(&mut self, _instance: &mut RunningGameboy, bit: bool) {
        // SAFETY: as in `vblank`.
        let link = unsafe { &mut *self.callback_data.link.get() };
        link.bit_to_send = bit;
    }

    fn serial_transfer_bit_end(&mut self, _instance: &mut RunningGameboy) -> bool {
        // SAFETY: as in `vblank`.
        let link = unsafe { &mut *self.callback_data.link.get() };
        match link.mode {
            LinkMode::Live => {
                if link.partner.is_null() {
                    // Not inside `step_linked`; there is nobody at the other end right now.
                    return true
                }
                // SAFETY: `step_linked` set these for exactly this step: the partner instance is
                // alive, not running and not otherwise borrowed (its `GameBoyColor` is behind the
                // `&mut` that `step_linked` holds and does not use until the pointers are cleared),
                // and only this thread touches either instance.
                let partner = unsafe { &mut *link.partner };
                let partner_link = unsafe { &mut *(*link.partner_data).link.get() };
                let got = partner.serial_get_data_bit();
                partner.serial_set_data_bit(link.bit_to_send);
                partner_link.log.push(GbSerialEvent::SlaveBit { cycles: partner_link.link_cycles, bit: link.bit_to_send });
                partner_link.shadow_pending_slave.push(link.bit_to_send);
                link.log.push(GbSerialEvent::MasterBit(got));
                link.step_master_bits.push(got);
                got
            }
            LinkMode::Replay => match link.replay_master.pop_front() {
                Some(bit) => {
                    link.step_master_bits.push(bit);
                    bit
                }
                None => {
                    if link.replay_connected {
                        link.misses += 1;
                    }
                    // What SameBoy returns with no cable.
                    true
                }
            },
            LinkMode::Off => true
        }
    }
}

impl LinkPort for GameBoyColor {
    fn step_linked(&mut self, partner: &mut dyn EmulatorCore, paced: bool) -> Result<RunTime, LinkError> {
        let partner = partner.as_any_mut().downcast_mut::<GameBoyColor>().ok_or(LinkError::IncompatiblePartner)?;
        if self.link().mode != LinkMode::Live || partner.link().mode != LinkMode::Live {
            return Err(LinkError::NotConnected)
        }
        let partner_ptr = partner.core.running_instance_ptr();
        let partner_data = Arc::as_ptr(&partner.callback_data);
        {
            let link = self.link();
            link.partner = partner_ptr;
            link.partner_data = partner_data;
        }
        let time = if paced {
            self.step()
        }
        else {
            self.core.set_turbo_mode(TurboMode::Enabled);
            let time = self.step();
            self.core.set_turbo_mode(self.turbo_mode);
            time
        };
        let link = self.link();
        link.partner = core::ptr::null_mut();
        link.partner_data = core::ptr::null();
        Ok(time)
    }

    fn link_time(&self) -> u64 {
        self.link_ref().link_time
    }

    fn connect(&mut self, _first: bool) -> Result<(), LinkError> {
        if self.link().mode == LinkMode::Live {
            return Err(LinkError::NotConnected)
        }
        {
            let link = self.link();
            link.mode = LinkMode::Live;
            link.link_time = 0;
            link.link_cycles = 0;
            link.replay_master.clear();
            link.replay_slave.clear();
            link.replay_connected = false;
            link.step_master_bits.clear();
            link.shadow_pending_slave.clear();
            link.log.push(GbSerialEvent::Connected(true));
        }
        self.core.connect_serial();
        if let Some(shadow) = self.shadow.as_mut() {
            shadow.gb.connect_serial();
            shadow.master_bits().clear();
        }
        Ok(())
    }

    fn disconnect(&mut self) {
        let mode = self.link().mode;
        if mode == LinkMode::Off {
            return
        }
        {
            let link = self.link();
            if mode == LinkMode::Live {
                link.log.push(GbSerialEvent::Connected(false));
            }
            link.mode = LinkMode::Off;
            link.partner = core::ptr::null_mut();
            link.partner_data = core::ptr::null();
            link.replay_master.clear();
            link.replay_slave.clear();
            link.replay_connected = false;
            link.shadow_pending_slave.clear();
        }
        self.core.disconnect_serial();
        if let Some(shadow) = self.shadow.as_mut() {
            shadow.gb.disconnect_serial();
            shadow.master_bits().clear();
        }
    }

    fn is_live(&self) -> bool {
        self.link_ref().mode == LinkMode::Live
    }

    fn take_serial_in(&mut self, into: &mut Vec<u8>) {
        let log = core::mem::take(&mut self.link().log);
        GbSerialEvents::encode(&log, into);
    }

    fn queue_serial_in(&mut self, data: &[u8]) -> Result<(), LinkError> {
        let events = GbSerialEvents::decode(data)?;
        if self.link().mode == LinkMode::Live {
            // A live port hears the real partner; a recording of what somebody else once heard
            // has no business here.
            return Err(LinkError::NotConnected)
        }
        if self.link().mode == LinkMode::Off {
            self.link().mode = LinkMode::Replay;
            self.link().link_time = 0;
            self.link().link_cycles = 0;
            self.core.connect_serial();
            if let Some(shadow) = self.shadow.as_mut() {
                shadow.gb.connect_serial();
                shadow.master_bits().clear();
            }
        }
        let mut due_now = Vec::new();
        {
            let link = self.link();
            for event in events {
                match event {
                    GbSerialEvent::MasterBit(bit) => link.replay_master.push_back(bit),
                    GbSerialEvent::SlaveBit { cycles, bit } => {
                        // A bit at the frame boundary itself is due before the frame's first step.
                        if cycles == 0 && link.link_cycles == 0 && link.replay_slave.is_empty() {
                            due_now.push(bit);
                        }
                        else {
                            link.replay_slave.push_back((cycles, bit));
                        }
                    }
                    GbSerialEvent::Connected(connected) => link.replay_connected = connected
                }
            }
        }
        for bit in &due_now {
            self.core.serial_set_data_bit(*bit);
        }
        if !due_now.is_empty() {
            // The shadow gets them before its next step, like a live slave bit.
            self.link().shadow_pending_slave.extend(due_now);
        }
        Ok(())
    }

    fn serial_replay_misses(&self) -> u64 {
        self.link_ref().misses
    }
}

/// The Game Boy address space as seen by `read_ram`/`write_ram`.
///
/// VRAM, WRAM and HRAM keep the addresses Poke-A-Byte has always used: `0xC000-0xDFFF` is the
/// first 8 KiB of WRAM (banks 0 and 1, whatever bank the game has switched in) and `0x10000` onwards
/// continues through the rest of it (banks 2-7 on a Game Boy Color). OAM and the I/O registers are
/// at their real addresses. Cartridge RAM gets a synthetic address past everything else, because
/// its real `0xA000-0xBFFF` window only shows whichever bank is switched in.
fn build_memory_regions(core: &Gameboy) -> (Vec<MemoryRegionInfo>, Vec<RegionSource>) {
    let mut regions = Vec::new();
    let mut sources = Vec::new();

    let mut add = |name, short_name, base_address: u32, region: DirectAccessRegion, offset: usize, max_len: usize, writable| {
        let available = core.direct_access(region).data.len().saturating_sub(offset);
        let len = available.min(max_len);
        if len == 0 {
            return
        }
        regions.push(MemoryRegionInfo { name, short_name, base_address, len: len as u32, default_big_endian: true, writable });
        sources.push(RegionSource { region, offset });
    };

    add("VRAM", "VRAM", 0x8000, DirectAccessRegion::VRAM, 0, 0x2000, true);
    add("WRAM (banks 0-1)", "WRAM", 0xC000, DirectAccessRegion::RAM, 0, 0x2000, true);
    add("WRAM (banks 2-7)", "WRAMX", 0x10000, DirectAccessRegion::RAM, 0x2000, 0x10000 - 0x2000, true);
    add("OAM", "OAM", 0xFE00, DirectAccessRegion::OAM, 0, 0xA0, true);
    // Raw register writes would bypass the hardware side effects of writing them.
    add("I/O registers", "IO", 0xFF00, DirectAccessRegion::IO, 0, 0x80, false);
    add("HRAM", "HRAM", 0xFF80, DirectAccessRegion::HRAM, 0, 0x7F, true);
    add("Cartridge RAM", "CART", 0x20000, DirectAccessRegion::CartRAM, 0, 0x100000, true);

    (regions, sources)
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
        read_ram_from_regions(self, address, into)
    }

    fn write_ram(&mut self, address: u32, from: &[u8]) -> Result<(), &'static str> {
        let Some((index, offset)) = locate_memory(&self.regions, address, from.len()) else {
            return Err("invalid or unknown address");
        };
        if !self.regions[index].writable {
            return Err("region is read-only");
        }
        let source = self.region_sources[index];
        let start = source.offset + offset;

        let region = self.core.direct_access_mut(source.region);
        let Some(data) = region.data.get_mut(start..start + from.len()) else {
            return Err("address+length overflows");
        };
        data.copy_from_slice(from);

        // Keep the audio shadow in lockstep (see the type's audio notes); otherwise it would drift
        // and need a full resync.
        if let Some(shadow) = self.shadow.as_mut()
            && let Some(data) = shadow.gb.direct_access_mut(source.region).data.get_mut(start..start + from.len())
        {
            data.copy_from_slice(from);
        }
        Ok(())
    }

    fn memory_regions(&self) -> &[MemoryRegionInfo] {
        &self.regions
    }

    fn memory_region_data(&self, index: usize) -> Option<&[u8]> {
        let info = self.regions.get(index)?;
        let source = self.region_sources[index];
        self.core.direct_access(source.region).data.get(source.offset..source.offset + info.len as usize)
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
            let link_mode = self.link().mode;
            let mut shadow = ShadowAudio::new(&self.rom, &self.bios, self.model, self.speed);
            shadow.sync_from(&self.core, self.input_mask, link_mode);
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
        self.mid_frame = false;
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
        let mask = input.first().copied().unwrap_or(0);
        self.input_mask = mask;
        self.core.set_input_button_mask(mask);
        if let Some(shadow) = self.shadow.as_mut() {
            shadow.gb.set_input_button_mask(mask);
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
        // What a reset leaves in RAM, HRAM, OAM and the wave RAM comes from SameBoy's random
        // number generator; seeded the same way every time, every reset starts from the same
        // garbage, here, in a replay of this session and on the other end of a link cable.
        safeboy::seed_random(RESET_RANDOM_SEED);
        // The cable stays plugged in across a reset: SameBoy keeps the callbacks (they live in the
        // unsaved section) and the port keeps its mode; only the pending replay bits are moot.
        self.core.reset();

        // skip the intro
        if self.core.is_hle_sgb() {
            let mut state = self.core.create_save_state();
            if let Some(b) = state.get_mut(0x1AB66..0x1AB68) {
                b[0] = 201;
                b[1] = 0;
                let _ = self.core.load_save_state(&state);
            }
        }

        self.resync_shadow();
        self.mid_frame = false;
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

    #[inline]
    fn is_mid_frame(&self) -> bool {
        self.mid_frame
    }

    fn link_port(&mut self) -> Option<&mut dyn LinkPort> {
        Some(self)
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}

/// The seed every reset starts SameBoy's random number generator from (see `hard_reset`).
const RESET_RANDOM_SEED: u64 = 0x5375_7065_7253_6875;

static GB_VERSION_WITH_HACKS: Lazy<String> = Lazy::new(|| {
    alloc::format!("{} with SGB intro skipped", safeboy::GB_VERSION)
});
