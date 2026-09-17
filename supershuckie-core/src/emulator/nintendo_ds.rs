use alloc::vec::Vec;
use crate::emulator::{locate_memory, read_ram_from_regions, EmulatorCore, Input, MemoryRegionInfo, RunTime, ScreenData, ScreenDataEncoding};
use alloc::string::String;
use alloc::borrow::ToOwned;
use melonds_rs::{Core, JitRegion};
use supershuckie_replay_recorder::blake3_hash;
use supershuckie_replay_recorder::replay_file::{ReplayConsoleType, ReplayHeaderBlake3Hash};
use alloc::boxed::Box;
use crate::{MonotonicTimestampProvider, TimestampMicros};

/// A NDS emulator.
///
/// Uses [melonDS](http://melonds.kuribo64.net) as the underlying core.
pub struct NintendoDS {
    screens: [ScreenData; 2],
    rom_checksum: ReplayHeaderBlake3Hash,
    core: Core,
    last_frame_microseconds: TimestampMicros,
    microseconds_per_frames: TimestampMicros,
    clock: Box<dyn MonotonicTimestampProvider>,
    skip_drawing: bool,
    audio_enabled: bool,
    jit: bool
}

impl NintendoDS {
    /// Instantiate from a ROM.
    pub fn new_from_rom(rom: &[u8], sram: Option<&[u8]>, clock: Box<dyn MonotonicTimestampProvider>, jit: bool) -> Result<Self, String> {
        Ok(Self {
            rom_checksum: blake3_hash(rom),
            screens: core::array::from_fn(|_| ScreenData {
                pixels: alloc::vec![0u32; 256*192],
                width: 256,
                height: 192,
                encoding: ScreenDataEncoding::A8R8G8B8
            }),
            core: Core::new(rom, sram.unwrap_or(&[]), jit).map_err(|e| alloc::format!("melonDS rejected the ROM: {e}"))?,
            last_frame_microseconds: 0,
            microseconds_per_frames: DEFAULT_MICROSECONDS_PER_FRAME,
            clock,
            skip_drawing: false,
            audio_enabled: false,
            jit
        })
    }

    /// Set the date.
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
        self.core.set_date(year, month, day, hour, minute, second);
    }
}

const DEFAULT_MICROSECONDS_PER_FRAME: u64 = 1000000 / 60;

const NDS_REGION_MAIN_RAM: usize = 0;
const NDS_REGION_SHARED_WRAM: usize = 1;
const NDS_REGION_ARM7_WRAM: usize = 2;

/// The Nintendo DS address space as seen by `read_ram`/`write_ram`, in ARM9 bus addresses. Main
/// RAM is the region Poke-A-Byte has always used; the work RAM regions start past its 4 MiB, where
/// no address was valid before.
const NDS_MEMORY_REGIONS: [MemoryRegionInfo; 3] = [
    MemoryRegionInfo { name: "Main RAM", short_name: "MAIN", base_address: 0x0200_0000, len: 0x40_0000, default_big_endian: false, writable: true },
    MemoryRegionInfo { name: "Shared WRAM", short_name: "SWRAM", base_address: 0x0300_0000, len: 0x8000, default_big_endian: false, writable: true },
    MemoryRegionInfo { name: "ARM7 WRAM", short_name: "WRAM7", base_address: 0x0380_0000, len: 0x1_0000, default_big_endian: false, writable: true },
];

#[allow(unused_variables)]
impl EmulatorCore for NintendoDS {
    fn run(&mut self) -> RunTime {
        let expected_next = self.last_frame_microseconds + self.microseconds_per_frames;
        let now = self.clock.get_timestamp_microseconds();
        if now < expected_next {
            return RunTime::NONE
        }

        let rval = self.run_unlocked();

        // if our clock is way too far behind, limit it a bit
        let several_frames_ago = self.clock
            .get_timestamp_microseconds()
            .saturating_sub(self.microseconds_per_frames * 16);

        self.last_frame_microseconds = (self.last_frame_microseconds + self.microseconds_per_frames)
            .max(several_frames_ago);

        rval
    }

    fn run_unlocked(&mut self) -> RunTime {
        self.core.run_frame();

        // A skipped frame was not composited, so the core's framebuffer still holds the last
        // drawn frame; leave our screens alone rather than re-copying it.
        if self.skip_drawing {
            return RunTime { frames: 1, presented: false }
        }

        let pixels_a = self.core.get_pixels(0).expect("no pixels????");
        let pixels_b = self.core.get_pixels(1).expect("no pixels????");

        self.screens[0].pixels.copy_from_slice(pixels_a.as_slice());
        self.screens[1].pixels.copy_from_slice(pixels_b.as_slice());

        RunTime::ONE_FRAME
    }

    fn set_skip_drawing(&mut self, skip: bool) {
        if self.skip_drawing != skip {
            self.skip_drawing = skip;
            self.core.set_skip_drawing(skip);
        }
    }

    fn set_audio_enabled(&mut self, enabled: bool) {
        if self.audio_enabled == enabled {
            return
        }
        self.audio_enabled = enabled;
        // The SPU mixes regardless (it is part of emulation); what was mixed while nobody
        // listened must not play now.
        self.core.drain_audio();
    }

    fn take_audio(&mut self, into: &mut Vec<i16>) {
        if !self.audio_enabled {
            return
        }
        // One frame is ~802 frames at 48 kHz; the ring holds 2048, so this is one iteration
        // unless several frames were run before a drain.
        let mut chunk = [0i16; 2048 * 2];
        loop {
            let frames = self.core.read_audio(&mut chunk);
            if frames == 0 {
                break
            }
            into.extend_from_slice(&chunk[..frames * 2]);
        }
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
        let (index, offset) = locate_memory(&NDS_MEMORY_REGIONS, address, from.len()).ok_or("unknown address or range")?;
        let (memory, jit_region) = match index {
            NDS_REGION_MAIN_RAM => (self.core.get_main_ram_mut(), JitRegion::MainRAM),
            NDS_REGION_SHARED_WRAM => (self.core.get_shared_wram_mut(), JitRegion::SharedWRAM),
            NDS_REGION_ARM7_WRAM => (self.core.get_arm7_wram_mut(), JitRegion::ARM7WRAM),
            _ => unreachable!("NDS_MEMORY_REGIONS index {index}")
        };
        memory.get_mut(offset..offset + from.len()).ok_or("out of range write")?.copy_from_slice(from);
        if self.jit {
            self.core.invalidate_jit(jit_region, offset as u32, from.len());
        }
        Ok(())
    }

    fn memory_regions(&self) -> &[MemoryRegionInfo] {
        &NDS_MEMORY_REGIONS
    }

    fn memory_region_data(&self, index: usize) -> Option<&[u8]> {
        Some(match index {
            NDS_REGION_MAIN_RAM => self.core.get_main_ram(),
            NDS_REGION_SHARED_WRAM => self.core.get_shared_wram(),
            NDS_REGION_ARM7_WRAM => self.core.get_arm7_wram(),
            _ => return None
        })
    }

    fn set_speed(&mut self, speed: f64) {
        self.microseconds_per_frames = (DEFAULT_MICROSECONDS_PER_FRAME as f64 / speed).clamp(0.0, u32::MAX as f64) as u64;
    }

    fn save_sram(&self) -> Vec<u8> {
        self.core.get_sram().to_vec()
    }

    fn create_save_state(&self) -> Vec<u8> {
        self.core.create_save_state().expect("failed to make NDS save state???")
    }

    fn create_save_state_into(&self, into: &mut Vec<u8>) {
        assert!(self.core.create_save_state_into(into), "failed to make NDS save state???");
    }

    fn load_save_state(&mut self, state: &[u8]) -> Result<(), String> {
        self.core
            .load_save_state(state)
            .then_some(())
            .ok_or_else(|| "failed to load nds save state".to_owned())
    }

    fn encode_input(&self, input: Input, into: &mut Vec<u8>) {
        let mut value = 0u32;

        value |= (input.a as u32) << 0;
        value |= (input.b as u32) << 1;
        value |= (input.x as u32) << 10;
        value |= (input.y as u32) << 11;
        value |= (input.d_left as u32) << 5;
        value |= (input.d_right as u32) << 4;
        value |= (input.d_up as u32) << 6;
        value |= (input.d_down as u32) << 7;
        value |= (input.l as u32) << 9;
        value |= (input.r as u32) << 8;
        value |= (input.select as u32) << 2;
        value |= (input.start as u32) << 3;

        if let Some((x,y)) = input.touch {
            value |= 0x1000;
            value |= (x as u32) << 16;
            value |= (y as u32) << 24;
        }

        into.clear();
        into.extend_from_slice(value.to_le_bytes().as_slice());
    }

    fn set_input_encoded(&mut self, input: &[u8]) {
        self.core.set_input(u32::from_le_bytes(input.try_into().unwrap_or([0u8; 4])))
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
        self.core.reset();
    }

    fn replay_console_type(&self) -> Option<ReplayConsoleType> {
        Some(ReplayConsoleType::NintendoDS)
    }

    fn rom_checksum(&self) -> &ReplayHeaderBlake3Hash {
        &self.rom_checksum
    }

    fn bios_checksum(&self) -> &ReplayHeaderBlake3Hash {
        &const { unsafe { core::mem::zeroed() } }
    }

    fn core_name(&self) -> &'static str {
        "melonDS 1.1 [SUPERSHUCKIE-EXPERIMENTAL-0]"
    }

    #[inline]
    fn frame_rate(&self) -> (u32, u32) {
        // NDS ~= 59.8261 Hz. melonDS does not expose a frame-rate getter in the vendored binding,
        // so this rational (33513982/560190) is used directly; verify against melonDS if it ever
        // surfaces its own frame timing.
        (33513982, 560190)
    }
}
