//! Checks for the RAM tools' core plumbing on real ROMs: memory regions, the memory monitor
//! (samples, traces, pause conditions, freezes, edits) and the replay write path.
//!
//! ```text
//! ram_tools_smoke [--gbc rom.gbc] [--gba rom.gba] [--nds rom.nds]
//! ```
//!
//! Link it like `nds_bench` (see that file's header). Every check asserts; the run ends with
//! "all checks passed".

use std::fs::File;
use std::io::BufWriter;
use std::num::NonZeroU64;
use std::sync::Arc;
use std::time::{Duration, Instant};

use supershuckie_core::emulator::{EmulatorCore, GameBoyAdvance, GameBoyColor, Model, NintendoDS, PartialReplayRecordMetadata};
use supershuckie_core::memory_monitor::{
    AddressPath, FreezeSpec, MemoryEdit, MemoryMonitorLocal, MemoryMonitorShared, MonitorEvent, MonitorSample, TraceCondition, TraceSpec, ValueDecode, ViewWindow, WriteFailure
};
use supershuckie_core::{std_timestamp_provider, SuperShuckieCore, ThreadedSuperShuckieCore};
use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::replay_file::record::{NullReplayFileSink, ReplayFileRecorder, ReplayFileRecorderSettings};
use supershuckie_replay_recorder::replay_file::{ReplayFileMetadata, ReplayHeaderBlake3Hash, ReplayPatchFormat};
use supershuckie_replay_recorder::{ByteVec, Packet, Speed, TimestampMillis};

#[derive(Copy, Clone, PartialEq, Debug)]
enum Console {
    Gbc,
    Gba,
    Nds
}

fn make_emulator(console: Console, rom: &[u8]) -> Box<dyn EmulatorCore> {
    match console {
        Console::Gbc => {
            let bios = include_bytes!("../../bootrom/cgb/cgb_boot/cgb_boot_fast.bin");
            Box::new(GameBoyColor::new_from_rom(rom, bios, None, Model::Cgb0))
        }
        Console::Gba => Box::new(GameBoyAdvance::new_from_rom(rom, None, &[], std_timestamp_provider()).expect("failed to load ROM")),
        Console::Nds => Box::new(NintendoDS::new_from_rom(rom, None, std_timestamp_provider(), false).expect("failed to load ROM"))
    }
}

fn make_core(console: Console, rom: &[u8]) -> SuperShuckieCore {
    SuperShuckieCore::new(make_emulator(console, rom), std_timestamp_provider())
}

/// Run until one more whole frame has been emulated.
fn run_frame(core: &mut SuperShuckieCore) {
    let target = core.total_frames() + 1;
    while core.total_frames() < target && !core.is_replay_stalled() {
        core.run_unlocked();
    }
}

/// Every region's bytes, concatenated.
fn all_memory(core: &dyn EmulatorCore) -> Vec<u8> {
    let mut out = Vec::new();
    for index in 0..core.memory_regions().len() {
        out.extend_from_slice(core.memory_region_data(index).unwrap_or(&[]));
    }
    out
}

fn check_regions(console: Console, core: &mut dyn EmulatorCore) {
    let regions = core.memory_regions().to_vec();
    assert!(!regions.is_empty(), "{console:?}: no regions");

    for (i, a) in regions.iter().enumerate() {
        for b in &regions[i + 1..] {
            assert_ne!(a.short_name, b.short_name, "{console:?}: duplicate short name");
            assert!(a.end_address() <= b.base_address as u64 || b.end_address() <= a.base_address as u64, "{console:?}: {} overlaps {}", a.name, b.name);
        }
    }

    for (index, region) in regions.iter().enumerate() {
        let data = core.memory_region_data(index).expect("region data").to_vec();
        assert!(data.len() <= region.len as usize, "{console:?} {}: more data than the region", region.name);
        println!("  {:>6} {:<18} 0x{:08X} {:>7} bytes ({} backed){}", region.short_name, region.name, region.base_address, region.len, data.len(), if region.writable { "" } else { ", read-only" });

        // read_ram sees exactly the region data.
        for offset in [0usize, data.len() / 2, data.len().saturating_sub(8)] {
            if offset + 8 > data.len() {
                continue
            }
            let mut buf = [0u8; 8];
            core.read_ram(region.base_address + offset as u32, &mut buf).expect("read_ram inside a region");
            assert_eq!(&buf, &data[offset..offset + 8], "{console:?} {}: read_ram differs from region data at +{offset:#x}", region.name);
        }

        // Writes land where reads see them, and read-only regions refuse them.
        if data.len() >= 2 {
            let address = region.base_address + data.len() as u32 / 3;
            let offset = (address - region.base_address) as usize;
            let original = [data[offset], data[offset + 1]];
            let result = core.write_ram(address, &[!original[0], original[1]]);
            if region.writable {
                result.expect("write to a writable region");
                assert_eq!(core.memory_region_data(index).unwrap()[offset], !original[0], "{console:?} {}: write not visible", region.name);
                core.write_ram(address, &original).expect("restore");
            }
            else {
                assert!(result.is_err(), "{console:?} {}: read-only region accepted a write", region.name);
            }
        }
    }

    // Addresses Poke-A-Byte has always used still resolve (and stay inside one region).
    let legacy: &[(u32, u32)] = match console {
        Console::Gbc => &[(0x8000, 0x2000), (0xC000, 0x2000), (0x10000, 0x2000), (0xFF80, 0x7F)],
        Console::Gba => &[(0x0200_0000, 0x40000), (0x0300_0000, 0x8000)],
        Console::Nds => &[(0x0200_0000, 0x40_0000)]
    };
    for &(address, len) in legacy {
        let mut buf = vec![0u8; len as usize];
        core.read_ram(address, &mut buf).unwrap_or_else(|e| panic!("{console:?}: legacy range {address:#x}+{len:#x} no longer readable: {e}"));
        let mut past = [0u8; 1];
        let past_end = address + len;
        let still_mapped = regions.iter().any(|r| r.offset_of(past_end, 1).is_some());
        assert!(still_mapped || core.read_ram(past_end, &mut past).is_err(), "{console:?}: {past_end:#x} should be unmapped");
    }
    println!("  regions ok");
}

/// The address in a writable region whose byte changed on the most of `frames` frames.
fn busiest_byte(core: &mut SuperShuckieCore, frames: u32) -> (u32, u32) {
    let regions = core.get_core().memory_regions().to_vec();
    let mut previous = all_memory(core.get_core());
    let mut changes = vec![0u32; previous.len()];
    for _ in 0..frames {
        run_frame(core);
        let now = all_memory(core.get_core());
        for (i, (a, b)) in previous.iter_mut().zip(now.iter()).enumerate() {
            if a != b {
                changes[i] += 1;
                *a = *b;
            }
        }
    }

    let mut best = (0u32, 0u32);
    let mut start = 0usize;
    for (index, region) in regions.iter().enumerate() {
        let len = core.get_core().memory_region_data(index).map(|d| d.len()).unwrap_or(0);
        if region.writable {
            // Leave room for the edit made next to it.
            for i in 8..len.saturating_sub(8) {
                if changes[start + i] > best.1 {
                    best = (region.base_address + i as u32, changes[start + i]);
                }
            }
        }
        start += len;
    }
    best
}

fn record_settings() -> ReplayFileRecorderSettings {
    ReplayFileRecorderSettings::default()
}

fn start_recording(core: &mut SuperShuckieCore, name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("supershuckie-ram-tools-smoke");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let final_path = dir.join(format!("{name}.replay"));
    let temp_path = dir.join(format!("{name}.temp.replay"));
    core.start_recording_replay(PartialReplayRecordMetadata {
        rom_name: name.into(),
        rom_filename: name.into(),
        settings: record_settings(),
        patch_format: ReplayPatchFormat::Unpatched,
        patch_target_checksum: Default::default(),
        patch_data: ByteVec::new(),
        frames_per_keyframe: NonZeroU64::new(120).unwrap(),
        final_file: BufWriter::new(File::create(&final_path).expect("create final")),
        temp_file: BufWriter::new(File::create(&temp_path).expect("create temp")),
    }).expect("start recording");
    final_path
}

fn count_writes(bytes: &[u8], address: u32) -> (u64, u64) {
    let mut player = ReplayFilePlayer::new(bytes, false).expect("parse");
    player.go_to_keyframe(0).expect("seek");
    let (mut matching, mut total) = (0, 0);
    while let Ok(Some(packet)) = player.next_packet() {
        if let Packet::WriteMemory { address: a, .. } = packet {
            total += 1;
            if *a == address as u64 {
                matching += 1;
            }
        }
    }
    (matching, total)
}

fn drain(shared: &MemoryMonitorShared) -> Vec<MonitorEvent> {
    let mut events = Vec::new();
    shared.drain_events(&mut events);
    events
}

/// Traces, pause conditions, freezes and edits through a recording and its playback.
fn check_monitor(console: Console, rom: &[u8]) {
    let mut core = make_core(console, rom);
    for _ in 0..300 {
        run_frame(&mut core);
    }
    let (busy, changes) = busiest_byte(&mut core, 60);
    println!("  busiest byte 0x{busy:08X} changed on {changes}/60 frames");
    assert!(changes >= 10, "{console:?}: nothing in the first region changes often enough to test with");

    // --- Pause conditions fire on the exact frame. ---
    let shared = MemoryMonitorShared::new();
    let mut monitor = MemoryMonitorLocal::new(shared.clone());
    shared.update_request(|r| {
        r.traces.push(TraceSpec { id: 1, path: AddressPath::direct(busy), len: 1, decode: ValueDecode::default(), pause_when: Some(TraceCondition::Changes) });
    });
    monitor.service(&mut core, true);
    let mut reference = [0u8; 1];
    core.get_core().read_ram(busy, &mut reference).unwrap();
    let mut expected_frame = None;
    let mut paused_frame = None;
    for _ in 0..120 {
        run_frame(&mut core);
        let mut now = [0u8; 1];
        core.get_core().read_ram(busy, &mut now).unwrap();
        if expected_frame.is_none() && now != reference {
            expected_frame = Some(core.total_frames());
        }
        if monitor.service(&mut core, true).pause && paused_frame.is_none() {
            paused_frame = Some(core.total_frames());
        }
        if expected_frame.is_some() && paused_frame.is_some() {
            break
        }
    }
    assert_eq!(paused_frame, expected_frame, "{console:?}: pause condition fired on the wrong frame");
    println!("  pause condition fired at frame {}", paused_frame.unwrap());

    // --- Freeze + edit while recording, then play back. ---
    let shared = MemoryMonitorShared::new();
    let mut monitor = MemoryMonitorLocal::new(shared.clone());
    let replay_path = start_recording(&mut core, &format!("{console:?}-freeze"));
    let frozen = [0x5Au8];
    shared.update_request(|r| {
        r.sampling = true;
        r.freezes.push(FreezeSpec::new(1, AddressPath::direct(busy), &frozen).unwrap());
    });
    let edit_address = busy + 4;
    let mut edit_old = [0u8; 2];
    core.get_core().read_ram(edit_address, &mut edit_old).unwrap();
    let edit_new = [!edit_old[0], !edit_old[1]];
    for frame in 0..300 {
        if frame == 150 {
            shared.push_edit(MemoryEdit { edit_id: 42, path: AddressPath::direct(edit_address), data: edit_new.to_vec() });
        }
        run_frame(&mut core);
        monitor.service(&mut core, true);
        let mut value = [0u8; 1];
        core.get_core().read_ram(busy, &mut value).unwrap();
        assert_eq!(value, frozen, "{console:?}: freeze not holding at the boundary");
    }
    // Samples are rate-limited by wall-clock time; touching the request asks for a fresh one.
    shared.update_request(|_| {});
    monitor.service(&mut core, true);
    let recorded_memory = all_memory(core.get_core());
    assert_eq!(core.stop_recording_replay(), Some(true));

    let mut sample = MonitorSample::default();
    shared.take_sample(0, &mut sample).expect("a sample");
    let restores = sample.freeze_restores[0] as u64;
    let events = drain(&shared);
    assert!(events.iter().any(|e| matches!(e, MonitorEvent::Written { edit_id: 42, address, old, new, .. } if *address == edit_address && old.len() == 2 && new == &edit_new)), "{console:?}: edit event missing: {events:?}");

    let bytes = std::fs::read(&replay_path).expect("read replay");
    let (freeze_writes, all_writes) = count_writes(&bytes, busy);
    println!("  freeze restored the value on {restores} frames; replay holds {freeze_writes} writes to it ({all_writes} writes in total)");
    assert_eq!(freeze_writes, restores, "{console:?}: one WriteMemory per restore");
    assert_eq!(all_writes, restores + 1, "{console:?}: plus the edit");

    let mut player = ReplayFilePlayer::new(&bytes, false).expect("parse");
    player.enable_threading();
    let mut play = make_core(console, rom);
    play.attach_replay_player(player, true).expect("attach");
    while !play.is_replay_stalled() {
        play.run_unlocked();
    }
    assert!(all_memory(play.get_core()) == recorded_memory, "{console:?}: playback of a recording with freezes and edits diverged");
    assert_eq!(play.replay_write_failures(), 0);
    println!("  playback with freezes and edits matches the recording");

    // --- Traces across a seek, and writes during playback. ---
    let shared = MemoryMonitorShared::new();
    let mut monitor = MemoryMonitorLocal::new(shared.clone());
    shared.update_request(|r| {
        r.traces.push(TraceSpec { id: 2, path: AddressPath::direct(edit_address), len: 2, decode: ValueDecode::default(), pause_when: None });
        r.freezes.push(FreezeSpec::new(3, AddressPath::direct(edit_address), &[1, 2]).unwrap());
    });
    play.go_to_replay_frame(100).expect("seek");
    monitor.service(&mut play, false);
    let before = all_memory(play.get_core());
    play.go_to_replay_frame(250).expect("seek");
    monitor.service(&mut play, false);
    shared.push_edit(MemoryEdit { edit_id: 7, path: AddressPath::direct(edit_address), data: vec![9, 9] });
    monitor.service(&mut play, false);
    assert!(!play.enqueue_write(edit_address, ByteVec::from(&[3u8, 4][..])), "writes are refused during playback");
    let after_seek = all_memory(play.get_core());
    play.detach_replay_player();
    assert!(all_memory(play.get_core()) == after_seek, "{console:?}: a write queued during playback landed on detach");
    assert!(before != after_seek);
    monitor.service(&mut play, false);
    let events = drain(&shared);
    assert!(events.iter().any(|e| matches!(e, MonitorEvent::Discontinuity { .. })), "{console:?}: no discontinuity: {events:?}");
    assert!(!events.iter().any(|e| matches!(e, MonitorEvent::Changed { .. })), "{console:?}: a seek was reported as a change: {events:?}");
    assert!(events.contains(&MonitorEvent::WriteFailed { edit_id: 7, reason: WriteFailure::Playback }), "{console:?}: {events:?}");
    println!("  seeks are discontinuities, playback refuses writes and freezes");
}

/// A replay carrying a write this version cannot apply plays back without panicking.
fn check_unmapped_replay_write(console: Console, rom: &[u8]) {
    let mut core = make_core(console, rom);
    for _ in 0..10 {
        run_frame(&mut core);
    }
    let emulator = core.get_core();
    let mut input = Vec::new();
    emulator.encode_input(Default::default(), &mut input);
    let mut recorder = ReplayFileRecorder::new_with_metadata(
        ReplayFileMetadata {
            console_type: emulator.replay_console_type().unwrap(),
            rom_name: "unmapped".into(),
            rom_filename: "unmapped".into(),
            rom_checksum: *emulator.rom_checksum(),
            bios_checksum: *emulator.bios_checksum(),
            emulator_core_name: emulator.core_name().into(),
            patch_format: ReplayPatchFormat::Unpatched,
            patch_target_checksum: ReplayHeaderBlake3Hash::default(),
            crop_start: None,
            crop_end: None,
            timer_offset: None
        },
        ByteVec::new(),
        record_settings(),
        TimestampMillis(0),
        ByteVec::from(input.as_slice()),
        Speed::default(),
        ByteVec::Heap(emulator.create_save_state()),
        Vec::<u8>::new(),
        NullReplayFileSink
    ).expect("recorder");
    for frame in 1..=10u64 {
        if frame == 5 {
            recorder.write_memory(0x7FFF_FFF0, ByteVec::from(&[1u8, 2, 3][..])).expect("write packet");
        }
        recorder.next_frame(TimestampMillis(frame * 16)).expect("frame");
    }
    let (bytes, _) = recorder.close().map_err(|(_, _, e)| e).expect("close");

    let player = ReplayFilePlayer::new(&bytes, false).expect("parse");
    let mut play = make_core(console, rom);
    play.attach_replay_player(player, true).expect("attach");
    while !play.is_replay_stalled() {
        play.run_unlocked();
    }
    assert_eq!(play.replay_write_failures(), 1);
    println!("  an unmappable WriteMemory is skipped, not fatal");
}

/// Palette writes go through mGBA's patch path and reach the renderer.
fn check_gba_palette(rom: &[u8]) {
    let mut core = make_core(Console::Gba, rom);
    let distinct_colors = |core: &SuperShuckieCore| {
        let mut colors: Vec<u32> = core.get_core().get_screens()[0].pixels.iter().map(|p| *p & 0xFFFFFF).collect();
        colors.sort_unstable();
        colors.dedup();
        colors.len()
    };
    for _ in 0..5000 {
        run_frame(&mut core);
        if core.total_frames() > 300 && distinct_colors(&core) >= 4 {
            break
        }
    }
    println!("  GBA screen shows {} colors at frame {}", distinct_colors(&core), core.total_frames());
    let color = [0x1F, 0x7C]; // BGR555 magenta
    let count_magenta = |core: &SuperShuckieCore| {
        core.get_core().get_screens()[0].pixels.iter().filter(|p| (**p & 0xFFFFFF) == 0xFF00FF || (**p & 0xFFFFFF) == 0xF800F8 || (**p & 0xFFFFFF) == 0xFF08FF).count()
    };
    let before = count_magenta(&core);
    let mut palette = [0u8; 0x400];
    core.get_core().read_ram(0x0500_0000, &mut palette).unwrap();
    // Every background and object color: whatever is on screen, something turns magenta.
    let all = color.repeat(0x200);
    assert!(core.enqueue_write(0x0500_0000, ByteVec::from(all.as_slice())));
    let mut written = [0u8; 0x400];
    core.get_core().read_ram(0x0500_0000, &mut written).unwrap();
    assert!(written == all.as_slice(), "palette write not visible in palette RAM");
    run_frame(&mut core);
    let after = count_magenta(&core);
    // Games that re-upload their palette buffer every VBlank (Pokemon among them) overwrite the
    // write before the next frame is drawn, so this is reported rather than asserted.
    println!("  GBA palette write: {before} -> {after} magenta pixels on the next frame{}", if after > before { "" } else { " (the game re-uploaded its palette)" });
    core.enqueue_write(0x0500_0000, ByteVec::from(&palette[..]));
}

/// Poke-A-Byte freezes write (and record) at most once per frame, and only when the game changed the
/// value: one on a byte the game keeps changing writes about once per frame, one on a byte it
/// leaves alone writes once. Talks to the integration server over its UDP protocol like the
/// Poke-A-Byte client does.
fn check_pokeabyte_freeze(console: Console, rom: &[u8]) {
    use std::net::UdpSocket;

    // The freeze below covers roughly frames 60-300 of the threaded core; find a byte the game keeps
    // changing over that stretch.
    let mut probe = make_core(console, rom);
    for _ in 0..60 {
        run_frame(&mut probe);
    }
    let before = all_memory(probe.get_core());
    let (busy, changes) = busiest_byte(&mut probe, 240);
    assert!(changes >= 200, "{console:?}: need a byte the game changes nearly every frame ({changes}/240)");
    // A byte in the busy byte's region that stayed the same the whole time (and is not 0xA5).
    let after = all_memory(probe.get_core());
    let regions = probe.get_core().memory_regions().to_vec();
    let mut offset = 0usize;
    let mut quiet = None;
    for (index, region) in regions.iter().enumerate() {
        let len = probe.get_core().memory_region_data(index).map(|d| d.len()).unwrap_or(0);
        if region.offset_of(busy, 1).is_some() {
            let i = (busy - region.base_address) as usize / 2;
            quiet = (i..len).chain(0..i).find(|i| before[offset + i] == after[offset + i] && after[offset + i] != 0xA5).map(|i| region.base_address + i as u32);
        }
        offset += len;
    }
    drop(probe);
    let quiet = quiet.expect("a byte that does not change");

    let core = ThreadedSuperShuckieCore::new(make_emulator(console, rom));
    core.set_speed(supershuckie_core::Speed::from_multiplier_float(1.0));
    while core.get_elapsed_time().frames < 55 {
        std::thread::sleep(Duration::from_millis(5));
    }
    if let Err(e) = core.set_pokeabyte_enabled(true) {
        println!("  skipping the Poke-A-Byte check: {e}");
        return
    }
    let client = UdpSocket::bind("127.0.0.1:0").expect("client socket");
    client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let server = "127.0.0.1:55356";

    let header = |instruction: u8| {
        let mut h = vec![0u8; 32];
        h[0] = 1;
        h[4] = instruction;
        h
    };

    // Setup: one 16-byte read block around the busy byte.
    let mut setup = header(2);
    setup.resize(0x20 + 0xC * 128, 0);
    setup[8..12].copy_from_slice(&1u32.to_le_bytes());
    setup[12..16].copy_from_slice(&(-1i32).to_le_bytes());
    setup[32..36].copy_from_slice(&0u32.to_le_bytes());
    setup[36..40].copy_from_slice(&busy.to_le_bytes());
    setup[40..44].copy_from_slice(&16u32.to_le_bytes());
    client.send_to(&setup, server).expect("send setup");
    let mut response = [0u8; 64];
    let (len, _) = client.recv_from(&mut response).expect("setup response");
    assert!(len >= 32 && response[4] == 2 && response[5] == 1, "unexpected setup response");

    let replay_path = std::env::temp_dir().join("supershuckie-ram-tools-smoke").join(format!("{console:?}-pokeabyte.replay"));
    let temp_path = replay_path.with_extension("temp.replay");
    std::fs::create_dir_all(replay_path.parent().unwrap()).unwrap();
    core.start_recording_replay(PartialReplayRecordMetadata {
        rom_name: "pokeabyte".into(),
        rom_filename: "pokeabyte".into(),
        settings: record_settings(),
        patch_format: ReplayPatchFormat::Unpatched,
        patch_target_checksum: Default::default(),
        patch_data: ByteVec::new(),
        frames_per_keyframe: NonZeroU64::new(120).unwrap(),
        final_file: BufWriter::new(File::create(&replay_path).expect("create final")),
        temp_file: BufWriter::new(File::create(&temp_path).expect("create temp")),
    });

    for (address, value) in [(busy, 0x5Au8), (quiet, 0xA5u8)] {
        let mut freeze = header(4);
        freeze.resize(0x21, 0);
        freeze[8..16].copy_from_slice(&(address as u64).to_le_bytes());
        freeze[16..20].copy_from_slice(&1u32.to_le_bytes());
        freeze[0x20] = value;
        client.send_to(&freeze, server).expect("send freeze");
    }

    std::thread::sleep(Duration::from_secs(4));
    let mut close = header(0xFF);
    close.resize(32, 0);
    let _ = client.send_to(&close, server);
    assert!(core.stop_recording_replay(), "{console:?}: recording closed");
    let _ = core.set_pokeabyte_enabled(false);
    drop(core);

    let bytes = std::fs::read(&replay_path).expect("read replay");
    let frames = ReplayFilePlayer::new(&bytes, false).expect("parse").get_total_frames();
    let (writes, _) = count_writes(&bytes, busy);
    let (quiet_writes, _) = count_writes(&bytes, quiet);
    println!("  Poke-A-Byte freezes over {frames} frames: {writes} writes to a byte the game keeps changing, {quiet_writes} to one it leaves alone");
    assert!(frames > 60, "{console:?}: the recording is too short to judge");
    assert!(writes > frames / 2, "{console:?}: the game should have fought the freeze on most frames ({writes} writes)");
    assert!(writes <= frames + 1, "{console:?}: Poke-A-Byte freezes wrote more than once per frame ({writes} over {frames} frames)");
    // Before the fix Poke-A-Byte freezes wrote (and recorded) on every frame whether or not anything
    // had changed the value.
    assert!(quiet_writes <= frames / 10, "{console:?}: a freeze on a value the game rarely touches kept writing ({quiet_writes} writes)");
}

/// The threaded core services an attached monitor, pauses on a condition and wakes early.
fn check_threaded(console: Console, rom: &[u8]) {
    let mut probe = make_core(console, rom);
    for _ in 0..300 {
        run_frame(&mut probe);
    }
    let (busy, _) = busiest_byte(&mut probe, 60);
    drop(probe);

    let core = ThreadedSuperShuckieCore::new(make_emulator(console, rom));
    assert!(!core.memory_regions().is_empty());
    let shared = MemoryMonitorShared::new();
    let first = core.memory_regions()[0];
    shared.update_request(|r| {
        r.sampling = true;
        r.windows[0] = Some(ViewWindow { address: first.base_address, len: 256 });
    });
    core.set_memory_monitor(Some(shared.clone()));

    let mut sample = MonitorSample::default();
    let mut seen = 0;
    let deadline = Instant::now() + Duration::from_secs(10);
    while core.get_elapsed_time().frames < 200 && Instant::now() < deadline {
        if let Some(g) = shared.take_sample(seen, &mut sample) {
            seen = g;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(seen > 3, "{console:?}: samples are not arriving ({seen})");
    assert_eq!(sample.windows[0].valid_len, 256);
    println!("  threaded: {seen} samples over {} frames", core.get_elapsed_time().frames);

    shared.update_request(|r| {
        r.traces.push(TraceSpec { id: 1, path: AddressPath::direct(busy), len: 1, decode: ValueDecode::default(), pause_when: Some(TraceCondition::Changes) });
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while !core.is_paused() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(core.is_paused(), "{console:?}: the pause condition did not pause the threaded core");

    // While paused the thread waits up to 100 ms between passes; a wake gets a new request
    // serviced right away.
    std::thread::sleep(Duration::from_millis(250));
    shared.take_sample(seen, &mut sample);
    seen = sample.generation.max(seen);
    let mut worst = Duration::ZERO;
    for i in 0..10u32 {
        std::thread::sleep(Duration::from_millis(37));
        let asked = Instant::now();
        shared.update_request(|r| r.windows[0] = Some(ViewWindow { address: first.base_address + i, len: 256 }));
        core.wake();
        loop {
            if let Some(g) = shared.take_sample(seen, &mut sample) {
                seen = g;
                if sample.windows[0].address == first.base_address + i {
                    break
                }
            }
            assert!(asked.elapsed() < Duration::from_secs(2), "no sample for the new window");
            std::thread::sleep(Duration::from_micros(200));
        }
        worst = worst.max(asked.elapsed());
    }
    println!("  threaded: paused on condition; woken requests answered within {:.1} ms", worst.as_secs_f64() * 1000.0);
    assert!(worst < Duration::from_millis(50), "{console:?}: waking the paused thread took {worst:?}");

    core.set_memory_monitor(None);
    drop(core);
    let _ = Arc::strong_count(&shared);
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut roms: Vec<(Console, String)> = Vec::new();
    while let Some(a) = args.next() {
        let console = match a.as_str() {
            "--gbc" => Console::Gbc,
            "--gba" => Console::Gba,
            "--nds" => Console::Nds,
            other => panic!("unexpected argument {other}")
        };
        roms.push((console, args.next().expect("rom path")));
    }
    assert!(!roms.is_empty(), "usage: ram_tools_smoke [--gbc rom] [--gba rom] [--nds rom]");

    for (console, path) in roms {
        println!("== {console:?}: {path}");
        let rom = std::fs::read(&path).expect("read rom");
        let mut emulator = make_emulator(console, &rom);
        for _ in 0..120 {
            emulator.run_unlocked();
        }
        check_regions(console, emulator.as_mut());
        drop(emulator);
        check_monitor(console, &rom);
        check_unmapped_replay_write(console, &rom);
        if console == Console::Gba {
            check_gba_palette(&rom);
        }
        check_threaded(console, &rom);
        check_pokeabyte_freeze(console, &rom);
    }
    println!("all checks passed");
}
