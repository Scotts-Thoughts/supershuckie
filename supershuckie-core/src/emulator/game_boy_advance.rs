use crate::emulator::link::{LinkError, LinkPort};
use crate::emulator::{locate_memory, read_ram_from_regions, EmulatorCore, Input, MemoryRegionInfo, RunTime, ScreenData, ScreenDataEncoding};
use alloc::{borrow::ToOwned, string::String, vec::Vec};
use std::prelude::rust_2015::Box;
use mgba_rs::{Core, LinkCoordinator, Region, ReplayQueued};
use supershuckie_replay_recorder::blake3_hash;
use supershuckie_replay_recorder::replay_file::{ReplayConsoleType, ReplayHeaderBlake3Hash};
use crate::{MonotonicTimestampProvider, TimestampMicros};

/// Emulator instance using mGBA.
pub struct GameBoyAdvance {
    core: Core,
    rom_checksum: ReplayHeaderBlake3Hash,
    bios_checksum: ReplayHeaderBlake3Hash,
    screen: ScreenData,
    last_frame_microseconds: TimestampMicros,
    microseconds_per_frames: TimestampMicros,
    clock: Box<dyn MonotonicTimestampProvider>,
    link: GbaLinkState
}

/// Where the link port stands (see [`LinkPort`]).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum LinkMode {
    Off,
    /// Plugged into a partner: mGBA's lockstep driver joins the two cores, which are stepped a
    /// slice at a time (`run_loop`) by whoever runs the pair.
    Live,
    /// Fed a recording of a live console's serial traffic (the replay driver answers the game).
    Replay
}

/// The link cable's state on a Game Boy Advance core.
struct GbaLinkState {
    mode: LinkMode,
    /// Whether this console is the pair's first: the lockstep's clock owner.
    first: bool,
    /// Emulated cycles since the cable went in (mGBA's clock wraps; this does not).
    link_time: u64,
    last_now: i32,
    /// Between two slices of a frame (live mode only): the frame is not complete.
    mid_frame: bool,
    /// The replayed cable came out during the frame about to run: take the replay driver out
    /// once the frame has.
    replay_detach_pending: bool
}

impl GbaLinkState {
    const OFF: Self = Self { mode: LinkMode::Off, first: false, link_time: 0, last_now: 0, mid_frame: false, replay_detach_pending: false };
}

// ~59.7 Hz
const DEFAULT_MICROSECONDS_PER_FRAME: TimestampMicros = 16742;

const GBA_REGION_EWRAM: usize = 0;
const GBA_REGION_IWRAM: usize = 1;
const GBA_REGION_PALETTE: usize = 2;
const GBA_REGION_VRAM: usize = 3;
const GBA_REGION_OAM: usize = 4;
const GBA_REGION_SAVE: usize = 5;

/// The Game Boy Advance's address space as seen by `read_ram`/`write_ram`: the real bus addresses.
/// EWRAM and IWRAM are the regions Poke-A-Byte has always used.
const GBA_MEMORY_REGIONS: [MemoryRegionInfo; 6] = [
    gba_region("EWRAM", "EWRAM", 0x0200_0000, 0x40000),
    gba_region("IWRAM", "IWRAM", 0x0300_0000, 0x8000),
    gba_region("Palette RAM", "PAL", 0x0500_0000, 0x400),
    gba_region("VRAM", "VRAM", 0x0600_0000, 0x18000),
    gba_region("OAM", "OAM", 0x0700_0000, 0x400),
    // Sized for the largest save chip (1 MiB flash); smaller or not-yet-detected saves are shorter.
    gba_region("Save data", "SAVE", 0x0E00_0000, 0x20000),
];

const fn gba_region(name: &'static str, short_name: &'static str, base_address: u32, len: u32) -> MemoryRegionInfo {
    MemoryRegionInfo { name, short_name, base_address, len, default_big_endian: false, writable: true }
}

impl GameBoyAdvance {
    /// Instantiate from a ROM.
    pub fn new_from_rom(rom: &[u8], sram: Option<&[u8]>, bios: &[u8], clock: Box<dyn MonotonicTimestampProvider>) -> Result<Self, String> {
        Ok(Self {
            rom_checksum: blake3_hash(rom),
            screen: ScreenData {
                pixels: alloc::vec![0u32; 240*160],
                width: 240,
                height: 160,
                encoding: ScreenDataEncoding::A8R8G8B8
            },
            last_frame_microseconds: 0,
            bios_checksum: blake3_hash(bios),
            microseconds_per_frames: DEFAULT_MICROSECONDS_PER_FRAME,
            core: Core::new(rom, sram.unwrap_or(&[]), bios).map_err(|e| alloc::format!("mGBA rejected the ROM: {e}"))?,
            clock,
            link: GbaLinkState::OFF
        })
    }

    /// Copy mGBA's frame into the screen buffer.
    fn publish_frame(&mut self) {
        // TODO: implement A8B8G8R8 so we don't have to do this swizzling
        for (a, &pixel) in self.screen.pixels.iter_mut().zip(self.core.get_pixels().iter()) {
            let r = (pixel >> 16) & 0xFF;
            let g = pixel & 0xFF00;
            let b = (pixel << 16) & 0xFF0000;

            *a = 0xFF000000 | r | g | b;
        }
    }

    /// A frame just completed: keep the pacing clock where [`EmulatorCore::run`] would.
    fn note_frame_paced(&mut self) {
        // if our clock is way too far behind, limit it a bit
        let several_frames_ago = self.clock
            .get_timestamp_microseconds()
            .saturating_sub(self.microseconds_per_frames * 16);

        self.last_frame_microseconds = (self.last_frame_microseconds + self.microseconds_per_frames)
            .max(several_frames_ago);
    }

    /// Advance the link clock by what mGBA's clock did since the last look.
    fn advance_link_time(&mut self) {
        let now = self.core.timing_now();
        let delta = now.wrapping_sub(self.link.last_now);
        self.link.last_now = now;
        if delta > 0 {
            self.link.link_time += delta as u64;
        }
    }

    /// Both cores of the pair on one coordinator (the first step of either side does this;
    /// whichever core already has one shares it).
    fn ensure_attached(&mut self, partner: &mut GameBoyAdvance) -> Result<(), LinkError> {
        if self.core.is_link_attached() && partner.core.is_link_attached() {
            return Ok(())
        }
        let coordinator = match (self.core.link_coordinator(), partner.core.link_coordinator()) {
            (Some(c), _) => c.clone(),
            (None, Some(c)) => c.clone(),
            (None, None) => LinkCoordinator::new()
        };
        if !self.core.is_link_attached() && !self.core.link_attach(&coordinator, self.link.first) {
            return Err(LinkError::Emulator(String::from("mGBA refused the link cable (a replay driver is in?)")))
        }
        if !partner.core.is_link_attached() && !partner.core.link_attach(&coordinator, partner.link.first) {
            self.core.link_detach();
            return Err(LinkError::Emulator(String::from("mGBA refused the other console's link cable")))
        }
        self.core.link_frame_started();
        partner.core.link_frame_started();
        self.link.last_now = self.core.timing_now();
        partner.link.last_now = partner.core.timing_now();
        Ok(())
    }
}

impl LinkPort for GameBoyAdvance {
    fn step_linked(&mut self, partner: &mut dyn EmulatorCore, paced: bool) -> Result<RunTime, LinkError> {
        let partner = partner.as_any_mut().downcast_mut::<GameBoyAdvance>().ok_or(LinkError::IncompatiblePartner)?;
        if self.link.mode != LinkMode::Live || partner.link.mode != LinkMode::Live {
            return Err(LinkError::NotConnected)
        }
        self.ensure_attached(partner)?;
        if self.core.is_link_asleep() {
            // The coordinator parked this console; the partner has to run first.
            return Ok(RunTime::NONE)
        }
        // A paced console starts a new frame only when it is due.
        if !self.link.mid_frame && paced {
            let expected_next = self.last_frame_microseconds + self.microseconds_per_frames;
            if self.clock.get_timestamp_microseconds() < expected_next {
                return Ok(RunTime::NONE)
            }
        }
        let frames = self.core.run_loop();
        self.advance_link_time();
        if frames == 0 {
            self.link.mid_frame = true;
            return Ok(RunTime::NONE)
        }
        self.link.mid_frame = false;
        self.publish_frame();
        if paced {
            self.note_frame_paced();
        }
        else {
            self.last_frame_microseconds = self.clock.get_timestamp_microseconds();
        }
        // The next frame's serial log counts from here.
        self.core.link_frame_started();
        Ok(RunTime::ONE_FRAME)
    }

    fn link_time(&self) -> u64 {
        self.link.link_time
    }

    fn is_asleep(&self) -> bool {
        self.link.mode == LinkMode::Live && self.core.is_link_asleep()
    }

    fn connect(&mut self, first: bool) -> Result<(), LinkError> {
        if self.link.mode == LinkMode::Live {
            return Err(LinkError::NotConnected)
        }
        if self.link.mode == LinkMode::Replay {
            self.core.replay_detach();
        }
        self.link = GbaLinkState { mode: LinkMode::Live, first, link_time: 0, last_now: self.core.timing_now(), mid_frame: false, replay_detach_pending: false };
        // The coordinator comes with the first step, once the partner is known.
        Ok(())
    }

    fn disconnect(&mut self) {
        match self.link.mode {
            LinkMode::Off => return,
            LinkMode::Live => self.core.link_detach(),
            LinkMode::Replay => self.core.replay_detach()
        }
        // A frame cut short by the unplug completes on the next ordinary run.
        self.link = GbaLinkState::OFF;
    }

    fn is_live(&self) -> bool {
        self.link.mode == LinkMode::Live
    }

    fn take_serial_in(&mut self, into: &mut Vec<u8>) {
        self.core.link_take_log(into);
    }

    fn queue_serial_in(&mut self, data: &[u8]) -> Result<(), LinkError> {
        if self.link.mode == LinkMode::Live {
            return Err(LinkError::NotConnected)
        }
        match self.core.replay_queue(data) {
            Some(ReplayQueued::Applied) => {}
            Some(ReplayQueued::AppliedThenDetach) => self.link.replay_detach_pending = true,
            None => return Err(LinkError::BadSerialData(String::from("the Game Boy Advance serial log does not parse")))
        }
        self.link.mode = LinkMode::Replay;
        Ok(())
    }

    fn serial_replay_misses(&self) -> u64 {
        self.core.replay_misses()
    }
}

impl EmulatorCore for GameBoyAdvance {
    fn run(&mut self) -> RunTime {
        let expected_next = self.last_frame_microseconds + self.microseconds_per_frames;
        let now = self.clock.get_timestamp_microseconds();
        if now < expected_next {
            return RunTime::NONE
        }

        let rval = self.run_unlocked();
        self.note_frame_paced();
        rval
    }

    fn run_unlocked(&mut self) -> RunTime {
        // A whole frame (the rest of one, after a linked run's slices ended mid-frame).
        self.core.run_frame();
        self.link.mid_frame = false;
        if self.link.replay_detach_pending {
            self.link.replay_detach_pending = false;
            self.core.replay_detach();
            self.link.mode = LinkMode::Off;
        }
        self.publish_frame();
        RunTime::ONE_FRAME
    }

    fn is_mid_frame(&self) -> bool {
        self.link.mid_frame
    }

    fn link_port(&mut self) -> Option<&mut dyn LinkPort> {
        Some(self)
    }

    fn microseconds_until_next_frame(&mut self) -> Option<u64> {
        let expected_next = self.last_frame_microseconds + self.microseconds_per_frames;
        Some(expected_next.saturating_sub(self.clock.get_timestamp_microseconds()))
    }

    fn frame_period_microseconds(&self) -> Option<u64> {
        Some(self.microseconds_per_frames)
    }

    fn read_ram(&self, address: u32, into: &mut [u8]) -> Result<(), &'static str> {
        read_ram_from_regions(self, address, into)
    }

    fn write_ram(&mut self, address: u32, from: &[u8]) -> Result<(), &'static str> {
        let (index, offset) = locate_memory(&GBA_MEMORY_REGIONS, address, from.len()).ok_or("unknown address or range")?;
        match index {
            GBA_REGION_EWRAM => self.core.get_ewram_mut()[offset..offset + from.len()].copy_from_slice(from),
            GBA_REGION_IWRAM => self.core.get_iwram_mut()[offset..offset + from.len()].copy_from_slice(from),
            // The renderer caches these; only mGBA's patch path keeps the caches in sync.
            GBA_REGION_PALETTE | GBA_REGION_VRAM | GBA_REGION_OAM => self.core.patch_write(address, from),
            GBA_REGION_SAVE => {
                let save = self.core.get_region_mut(Region::SaveData);
                let Some(bytes) = save.get_mut(offset..offset + from.len()) else {
                    return Err("invalid range (went outside of the save data)")
                };
                bytes.copy_from_slice(from);
            }
            _ => unreachable!("GBA_MEMORY_REGIONS index {index}")
        }
        Ok(())
    }

    fn memory_regions(&self) -> &[MemoryRegionInfo] {
        &GBA_MEMORY_REGIONS
    }

    fn memory_region_data(&self, index: usize) -> Option<&[u8]> {
        Some(match index {
            GBA_REGION_EWRAM => self.core.get_ewram(),
            GBA_REGION_IWRAM => self.core.get_iwram(),
            GBA_REGION_PALETTE => self.core.get_region(Region::PaletteRAM),
            GBA_REGION_VRAM => self.core.get_region(Region::VRAM),
            GBA_REGION_OAM => self.core.get_region(Region::OAM),
            GBA_REGION_SAVE => self.core.get_region(Region::SaveData),
            _ => return None
        })
    }

    #[inline]
    fn set_audio_enabled(&mut self, enabled: bool) {
        self.core.set_audio_enabled(enabled);
    }

    fn take_audio(&mut self, into: &mut Vec<i16>) {
        // One frame is ~804 frames at 48 kHz; normally a single iteration.
        let mut chunk = [0i16; 2048 * 2];
        loop {
            let frames = self.core.read_audio(&mut chunk);
            if frames == 0 {
                break
            }
            into.extend_from_slice(&chunk[..frames * 2]);
        }
    }

    fn set_speed(&mut self, speed: f64) {
        self.microseconds_per_frames = (DEFAULT_MICROSECONDS_PER_FRAME as f64 / speed).clamp(0.0, u32::MAX as f64) as u64;
    }

    #[inline]
    fn save_sram(&self) -> Vec<u8> {
        self.core.get_sram()
    }

    #[inline]
    fn create_save_state(&self) -> Vec<u8> {
        self.core.create_save_state().expect("failed to create GBA save state")
    }

    fn create_save_state_into(&self, into: &mut Vec<u8>) {
        assert!(self.core.create_save_state_into(into), "failed to create GBA save state");
    }

    #[inline]
    fn load_save_state(&mut self, state: &[u8]) -> Result<(), String> {
        if self.core.load_save_state(state) {
            Ok(())
        }
        else {
            Err("loading save state to mGBA failed".to_owned())
        }
    }

    fn encode_input(&self, input: Input, into: &mut Vec<u8>) {
        let mut value = 0u16;

        value |= (input.a as u16) << 0;
        value |= (input.b as u16) << 1;
        value |= (input.d_left as u16) << 5;
        value |= (input.d_right as u16) << 4;
        value |= (input.d_up as u16) << 6;
        value |= (input.d_down as u16) << 7;
        value |= (input.l as u16) << 9;
        value |= (input.r as u16) << 8;
        value |= (input.select as u16) << 2;
        value |= (input.start as u16) << 3;

        into.clear();
        into.extend_from_slice(value.to_le_bytes().as_slice());
    }

    #[inline]
    fn set_input_encoded(&mut self, input: &[u8]) {
        let [a, b] = input else {
            return self.set_input_encoded(&[0,0])
        };
        self.core.set_input(u16::from_le_bytes([*a, *b]))
    }

    #[inline]
    fn get_screens(&self) -> &[ScreenData] {
        core::slice::from_ref(&self.screen)
    }

    #[inline]
    fn swap_screen_data(&mut self, screens: &mut [ScreenData]) {
        assert_eq!(screens.len(), 1, "expected one screen to be swapped");
        core::mem::swap(&mut screens[0].pixels, &mut self.screen.pixels)
    }

    #[inline]
    fn hard_reset(&mut self) {
        self.core.reset()
    }

    #[inline]
    fn replay_console_type(&self) -> Option<ReplayConsoleType> {
        Some(ReplayConsoleType::GameBoyAdvance)
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
        "mGBA 0.10.5-76d93515629031047ca2b7bcb6237089adcb36d7 with BIOS skipped"
    }

    #[inline]
    fn frame_rate(&self) -> (u32, u32) {
        // GBA: 16777216 Hz / 280896 dots per frame, which reduces to 4194304/70224 ~= 59.7275 Hz
        // (same rational as GB/GBC).
        (4194304, 70224)
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}
