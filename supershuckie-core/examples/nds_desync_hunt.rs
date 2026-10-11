//! Nintendo DS desync hunting: play a replay headlessly from one of its keyframes and compare the
//! console against every recorded keyframe on the way, reporting each mismatch in detail (which
//! save-state sections differ, and for main RAM the differing addresses with both values).
//!
//! ```text
//! nds_desync_hunt <rom.nds> <file.replay> [options]
//!
//!   --from <frame>        start at the keyframe at/before this frame (default 0), loaded the way the
//!                         app loads one (state, then the keyframe's input)
//!   --to <frame>          stop after this frame (default: the end of the replay)
//!   --history <k>:<n>     before loading the start keyframe, load the keyframe at/before frame k
//!                         and run n frames of the replay from there (a different pre-load history;
//!                         default: none, a freshly created core)
//!   --present-every <n>   skip drawing on all frames but one in n (as the app does when sped up)
//!   --phase <p>           which frame in n is drawn (frame % n == p; default 0)
//!   --resync              after each comparison load the recorded keyframe, so every 120-frame
//!                         segment is checked on its own
//!   --report <n>          describe the first n mismatching keyframes in detail (default 20)
//!   --unsaved             print the state save states do not carry after every load and reset
//!   --adopt               after loading the start keyframe, do what replay playback does with it
//!                         (`EmulatorCore::adopt_replay_keyframe`: an old keyframe gets a WFC ID)
//!   --check-keyframes     at every keyframe, do what replay playback does
//!                         (`EmulatorCore::check_replay_keyframe`) and resync when it says to
//! ```
//!
//! Every keyframe gets one summary line (`kf <frame> ok|BAD ...`) with a hash of the canonical
//! state, so the outputs of runs with different options can be diffed against each other.

use supershuckie_core::emulator::{EmulatorCore, NintendoDS};
use supershuckie_core::std_timestamp_provider;
use supershuckie_replay_recorder::keyframe_masks::transient_ranges;
use supershuckie_replay_recorder::replay_file::playback::{ReplayFilePlayer, ReplaySeekError};
use supershuckie_replay_recorder::replay_file::ReplayConsoleType;
use supershuckie_replay_recorder::Packet;

struct Options {
    rom: String,
    replay: String,
    from: u64,
    to: u64,
    history: Option<(u64, u64)>,
    present_every: u64,
    phase: u64,
    resync: bool,
    report: u64,
    unsaved: bool,
    perturb: bool,
    firmware_patches: Vec<(usize, Vec<u8>)>,
    watch_firmware: bool,
    shots: Vec<u64>,
    shot_dir: String,
    boot: u64,
    adopt: bool,
    check_keyframes: bool,
}

fn parse_args() -> Options {
    let mut args = std::env::args().skip(1);
    let rom = args.next().expect("usage: nds_desync_hunt <rom.nds> <file.replay> [options]");
    let replay = args.next().expect("usage: nds_desync_hunt <rom.nds> <file.replay> [options]");
    let mut o = Options { rom, replay, from: 0, to: u64::MAX, history: None, present_every: 1, phase: 0, resync: false, report: 20, unsaved: false, perturb: false, firmware_patches: Vec::new(), watch_firmware: false, shots: Vec::new(), shot_dir: ".".into(), boot: 0, adopt: false, check_keyframes: false };
    while let Some(a) = args.next() {
        let mut value = || args.next().unwrap_or_else(|| panic!("{a} needs a value"));
        match a.as_str() {
            "--from" => o.from = value().parse().expect("--from"),
            "--to" => o.to = value().parse().expect("--to"),
            "--history" => {
                let v = value();
                let (k, n) = v.split_once(':').expect("--history k:n");
                o.history = Some((k.parse().expect("history k"), n.parse().expect("history n")));
            }
            "--present-every" => o.present_every = value().parse::<u64>().expect("--present-every").max(1),
            "--phase" => o.phase = value().parse().expect("--phase"),
            "--resync" => o.resync = true,
            "--report" => o.report = value().parse().expect("--report"),
            "--unsaved" => o.unsaved = true,
            "--perturb" => o.perturb = true,
            "--adopt" => o.adopt = true,
            "--check-keyframes" => o.check_keyframes = true,
            "--watch-firmware" => o.watch_firmware = true,
            "--shot" => o.shots.push(value().parse().expect("--shot frame")),
            "--shot-dir" => o.shot_dir = value(),
            "--boot" => o.boot = value().parse().expect("--boot frames"),
            "--patch-firmware" => {
                let v = value();
                let (offset, hex) = v.split_once(':').expect("--patch-firmware offset:hexbytes");
                let offset = usize::from_str_radix(offset, 16).expect("firmware offset (hex)");
                let data = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex byte")).collect();
                o.firmware_patches.push((offset, data));
            }
            other => panic!("unknown option {other}"),
        }
    }
    o
}

/// Seek `player` to the keyframe at or before `frame`; returns that keyframe's frame.
fn seek(player: &mut ReplayFilePlayer, frame: u64) -> u64 {
    let target = player.all_keyframes().range(..=frame).next_back().map(|(f, _)| *f).unwrap_or(0);
    match player.go_to_keyframe(target) {
        Ok(()) => target,
        Err(ReplaySeekError::NoSuchKeyframe { best, .. }) => {
            player.go_to_keyframe(best).expect("seek best");
            best
        }
        Err(e) => panic!("seek to {target}: {e:?}"),
    }
}

/// Save both screens (top above bottom) as a JPEG.
fn save_screens(core: &NintendoDS, path: &str) {
    let mut rgb565 = Vec::with_capacity(256 * 384 * 2);
    for screen in core.get_screens() {
        for px in &screen.pixels {
            let (r, g, b) = ((px >> 16) & 0xFF, (px >> 8) & 0xFF, px & 0xFF);
            let v = (((r >> 3) << 11) | ((g >> 2) << 5) | (b >> 3)) as u16;
            rgb565.extend_from_slice(&v.to_le_bytes());
        }
    }
    let height = (rgb565.len() / 2 / 256) as u32;
    match supershuckie_replay_recorder::thumbnail_jpeg::encode_rgb565(256, height, &rgb565, 92) {
        Ok(jpeg) => {
            std::fs::write(path, jpeg).expect("write screenshot");
            println!("  screenshot: {path}");
        }
        Err(e) => println!("  screenshot failed: {e}"),
    }
}

/// The DS firmware's CRC16 (as melonDS's `CRC16`).
fn firmware_crc16(data: &[u8], start: u32) -> u16 {
    const TABLE: [u32; 8] = [0xC0C1, 0xC181, 0xC301, 0xC601, 0xCC01, 0xD801, 0xF001, 0xA001];
    let mut crc = start;
    for byte in data {
        crc ^= *byte as u32;
        for (j, value) in TABLE.iter().enumerate() {
            if crc & 1 != 0 {
                crc >>= 1;
                crc ^= value << (7 - j);
            } else {
                crc >>= 1;
            }
        }
    }
    (crc & 0xFFFF) as u16
}

/// Apply `--patch-firmware` writes, then fix the CRC16 of every Wi-Fi access point block
/// (0x100 bytes, checksum at +0xFE over +0x00..+0xFD) a write touched.
fn patch_firmware(core: &mut NintendoDS, patches: &[(usize, Vec<u8>)]) {
    let mut blocks: Vec<usize> = Vec::new();
    for (offset, data) in patches {
        core.debug_write_firmware(*offset, data);
        for b in [0x1FA00usize, 0x1FB00, 0x1FC00] {
            if *offset < b + 0x100 && offset + data.len() > b && !blocks.contains(&b) {
                blocks.push(b);
            }
        }
    }
    for b in blocks {
        let crc = firmware_crc16(&core.debug_firmware()[b..b + 0xFE], 0);
        core.debug_write_firmware(b + 0xFE, &crc.to_le_bytes());
    }
}

/// The bytes of the firmware image that differ from `reference` (a freshly created core's), as
/// offset ranges with their current contents.
fn firmware_changes(reference: &[u8], core: &NintendoDS) -> String {
    let current = core.debug_firmware();
    if current.len() != reference.len() {
        return format!("length {} vs {}", current.len(), reference.len());
    }
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for i in (0..current.len()).filter(|&i| current[i] != reference[i]) {
        match ranges.last_mut() {
            Some((_, end)) if i <= *end + 8 => *end = i,
            _ => ranges.push((i, i)),
        }
    }
    if ranges.is_empty() {
        return "same as a new core's".into();
    }
    let parts: Vec<String> = ranges.iter().take(16).map(|(s, e)| {
        let now: String = current[*s..=*e].iter().map(|b| format!("{b:02X}")).collect();
        let was: String = reference[*s..=*e].iter().map(|b| format!("{b:02X}")).collect();
        format!("{s:05X}..{e:05X} now {now} (new core {was})")
    }).collect();
    format!("{} ranges differ from a new core's: {}", ranges.len(), parts.join("; "))
}

fn print_unsaved(label: &str, core: &NintendoDS, firmware_reference: &[u8]) {
    println!("  firmware [{label}]: {}", firmware_changes(firmware_reference, core));
    let values = core.debug_unsaved_state();
    let text: Vec<String> = melonds_rs::DEBUG_UNSAVED_STATE_NAMES.iter().zip(values.iter()).map(|(n, v)| format!("{n}={v:X}")).collect();
    println!("  unsaved [{label}]: {}", text.join(" "));
}

/// What happened while feeding one frame's packets.
#[derive(Default)]
struct Fed {
    ended: bool,
    reset: bool,
    keyframe: Option<(u64, Vec<u8>)>,
}

/// Apply the packets up to the next `NextFrame` (exclusive of running it), the way
/// `SuperShuckieCore::handle_replay` does. The first keyframe packet after a seek is the start
/// state: it is loaded when `load_first` is set, otherwise handed back for comparison.
fn feed(player: &mut ReplayFilePlayer, core: &mut NintendoDS, load_first: &mut bool) -> Fed {
    let mut fed = Fed::default();
    loop {
        let packet = match player.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) | Err(_) => {
                fed.ended = true;
                return fed;
            }
        };
        match packet {
            Packet::NextFrame { .. } => return fed,
            Packet::ChangeInput { data } => core.set_input_encoded(data.as_slice()),
            Packet::WriteMemory { address, data } => {
                let _ = core.write_ram(*address as u32, data.as_slice());
            }
            Packet::ResetConsole => {
                core.hard_reset();
                fed.reset = true;
            }
            Packet::LoadSaveState { state } => {
                let _ = core.load_save_state(state.as_slice());
            }
            Packet::Keyframe { metadata, state } => {
                if *load_first {
                    *load_first = false;
                    let state = state.as_slice().to_vec();
                    let input = metadata.input.to_vec();
                    let stale = player.current_keyframe_has_stale_transients();
                    if stale {
                        core.load_save_state_with_stale_output(&state).expect("load keyframe");
                    } else {
                        core.load_save_state(&state).expect("load keyframe");
                    }
                    // As `load_replay_keyframe_at_or_before`: the keyframe's input after the state.
                    core.set_input_encoded(&input);
                } else {
                    fed.keyframe = Some((metadata.elapsed_frames, state.as_slice().to_vec()));
                }
            }
            _ => {}
        }
    }
}

/// One frame's worth of replay packets, owned.
#[derive(Clone, Default)]
struct FramePackets {
    writes: Vec<(u32, Vec<u8>)>,
    reset: bool,
    input: Option<Vec<u8>>,
}

/// `--perturb`: starting from the keyframe at `o.from`, play to the next keyframe once as
/// recorded and then once per input change with that change moved a frame later or earlier,
/// reporting which variants reproduce the recorded next keyframe exactly.
fn perturb(o: &Options, rom: &[u8], bytes: &[u8]) {
    let mut player = ReplayFilePlayer::new(bytes, true).expect("parse replay");
    let start = seek(&mut player, o.from);
    let mut start_state = Vec::new();
    let mut start_input = Vec::new();
    let mut frames: Vec<FramePackets> = Vec::new();
    let mut current = FramePackets::default();
    let target_state;
    let target_frame;
    loop {
        let packet = player.next_packet().expect("read").expect("replay ended before the next keyframe");
        match packet {
            Packet::Keyframe { metadata, state } if start_state.is_empty() => {
                start_state = state.as_slice().to_vec();
                start_input = metadata.input.to_vec();
            }
            Packet::Keyframe { metadata, state } => {
                target_frame = metadata.elapsed_frames;
                target_state = state.as_slice().to_vec();
                break;
            }
            Packet::NextFrame { .. } => frames.push(core::mem::take(&mut current)),
            Packet::ChangeInput { data } => current.input = Some(data.to_vec()),
            Packet::WriteMemory { address, data } => current.writes.push((*address as u32, data.to_vec())),
            Packet::ResetConsole => current.reset = true,
            _ => {}
        }
    }
    // The input the console holds while each frame runs.
    let mut inputs: Vec<Vec<u8>> = Vec::with_capacity(frames.len());
    let mut held = start_input.clone();
    for f in &frames {
        if let Some(i) = &f.input {
            held = i.clone();
        }
        inputs.push(held.clone());
    }
    println!("perturb: keyframe {start} -> {target_frame}, {} frames, start input {:02X?}", frames.len(), start_input);
    let mut core = NintendoDS::new_from_rom(rom, None, std_timestamp_provider(), false).expect("load rom");
    let mut live = Vec::new();
    let mut run = |inputs: &[Vec<u8>]| -> Vec<SectionDiff> {
        core.load_save_state(&start_state).expect("load start");
        core.set_input_encoded(&start_input);
        for (f, packets) in frames.iter().enumerate() {
            if packets.reset {
                core.hard_reset();
            }
            for (address, data) in &packets.writes {
                let _ = core.write_ram(*address, data);
            }
            core.set_input_encoded(&inputs[f]);
            core.run_unlocked();
        }
        core.create_save_state_into(&mut live);
        compare(&live, &target_state)
    };
    let summary = |d: &[SectionDiff]| -> String {
        if d.is_empty() { "MATCH".into() } else { d.iter().map(|d| format!("{}:{}", d.section, d.bytes)).collect::<Vec<_>>().join(" ") }
    };
    println!("  as recorded: {}", summary(&run(&inputs)));
    for f in 1..inputs.len() {
        if inputs[f] == inputs[f - 1] {
            continue;
        }
        let mut later = inputs.clone();
        later[f] = inputs[f - 1].clone();
        let mut earlier = inputs.clone();
        earlier[f - 1] = inputs[f].clone();
        let frame = start + f as u64;
        println!(
            "  change at frame {frame} ({:02X?} -> {:02X?}): one frame later {} | one frame earlier {}",
            inputs[f - 1],
            inputs[f],
            summary(&run(&later)),
            summary(&run(&earlier))
        );
    }
}

fn main() {
    let o = parse_args();
    let rom = std::fs::read(&o.rom).expect("read rom");
    let bytes = std::fs::read(&o.replay).expect("read replay");
    if o.perturb {
        perturb(&o, &rom, &bytes);
        return;
    }
    if o.boot > 0 {
        // A freshly created core (the game boots directly) run with no input: when does the game
        // first write to the firmware?
        let mut core = NintendoDS::new_from_rom(&rom, None, std_timestamp_provider(), false).expect("load rom");
        let mut firmware = core.debug_firmware().to_vec();
        for frame in 1..=o.boot {
            core.run_unlocked();
            let now = core.debug_firmware().to_vec();
            if now != firmware {
                println!("boot: firmware written during frame {frame}: {}", firmware_changes(&firmware, &core));
                save_screens(&core, &format!("{}/boot-firmware-write-{frame}.jpg", o.shot_dir));
                firmware = now;
            }
            if o.shots.contains(&frame) {
                save_screens(&core, &format!("{}/boot-{frame}.jpg", o.shot_dir));
            }
        }
        println!("boot: ran {} frames", o.boot);
        return;
    }
    let mut player = ReplayFilePlayer::new(&bytes, true).expect("parse replay");
    let mut core = NintendoDS::new_from_rom(&rom, None, std_timestamp_provider(), false).expect("load rom");
    let firmware_reference = core.debug_firmware().to_vec();
    let mut last_firmware = String::new();
    println!(
        "{}: {} frames, {} keyframes; from {} to {}, history {:?}, present every {} (phase {}), resync {}",
        o.replay,
        player.get_total_frames(),
        player.all_keyframes().len(),
        o.from,
        o.to,
        o.history,
        o.present_every,
        o.phase,
        o.resync
    );

    if let Some((k, n)) = o.history {
        let at = seek(&mut player, k);
        let mut load_first = true;
        for _ in 0..n {
            let fed = feed(&mut player, &mut core, &mut load_first);
            if fed.ended {
                break;
            }
            core.run_unlocked();
        }
        println!("history: loaded keyframe {at} and ran {n} frames");
        if o.unsaved {
            print_unsaved("after history", &core, &firmware_reference);
        }
    }

    if !o.firmware_patches.is_empty() {
        patch_firmware(&mut core, &o.firmware_patches);
        println!("  firmware [patched]: {}", firmware_changes(&firmware_reference, &core));
    }

    let mut watched_firmware = core.debug_firmware().to_vec();
    let start = seek(&mut player, o.from);
    let mut load_first = true;
    let mut frame = start;
    let mut checked = 0u64;
    let mut bad = 0u64;
    let mut reported = 0u64;
    let mut first_bad = None;
    let mut first_load = true;
    let mut live = Vec::new();
    loop {
        let fed = feed(&mut player, &mut core, &mut load_first);
        if first_load {
            first_load = false;
            println!("loaded keyframe {start}");
            if o.adopt {
                println!("  adopt_replay_keyframe: {}", if core.adopt_replay_keyframe() { "gave the firmware a WFC ID" } else { "nothing to do" });
            }
            if o.unsaved {
                print_unsaved("after load", &core, &firmware_reference);
            }
        }
        if let Some((kf_frame, recorded)) = fed.keyframe {
            core.create_save_state_into(&mut live);
            let diff = compare(&live, &recorded);
            checked += 1;
            let hash: String = supershuckie_replay_recorder::blake3_hash(&canonical(&live, &recorded).0)[..8].iter().map(|b| format!("{b:02x}")).collect();
            if diff.is_empty() {
                println!("kf {kf_frame} ok {hash}");
            } else {
                bad += 1;
                first_bad.get_or_insert(kf_frame);
                let summary: Vec<String> = diff.iter().map(|d| format!("{}:{}", d.section, d.bytes)).collect();
                println!("kf {kf_frame} BAD {hash} [{}]", summary.join(" "));
                if reported < o.report {
                    reported += 1;
                    for d in &diff {
                        println!("    {}", d.detail);
                    }
                }
            }
            if o.unsaved {
                let firmware = firmware_changes(&firmware_reference, &core);
                if firmware != last_firmware {
                    println!("  firmware at kf {kf_frame}: {firmware}");
                    last_firmware = firmware;
                }
            }
            if o.check_keyframes && !o.resync && core.check_replay_keyframe(&recorded) {
                println!("  check_replay_keyframe at kf {kf_frame}: the firmware was used differently -> resync");
                let stale = player.current_keyframe_has_stale_transients();
                if stale {
                    core.load_save_state_with_stale_output(&recorded).expect("resync");
                } else {
                    core.load_save_state(&recorded).expect("resync");
                }
            }
            if o.resync {
                let stale = player.current_keyframe_has_stale_transients();
                if stale {
                    core.load_save_state_with_stale_output(&recorded).expect("resync");
                } else {
                    core.load_save_state(&recorded).expect("resync");
                }
            }
        }
        if fed.reset {
            println!("reset at frame {frame}");
            if o.unsaved {
                print_unsaved("after reset", &core, &firmware_reference);
            }
        }
        if fed.ended || frame >= o.to {
            break;
        }
        core.set_skip_drawing(frame % o.present_every != o.phase);
        core.run_unlocked();
        frame += 1;
        if o.watch_firmware {
            let now = core.debug_firmware().to_vec();
            if now != watched_firmware {
                println!("firmware written during frame {} (frame {frame} now): {}", frame - 1, firmware_changes(&watched_firmware, &core));
                save_screens(&core, &format!("{}/firmware-write-{frame}.jpg", o.shot_dir));
                watched_firmware = now;
            }
        }
        if o.shots.contains(&frame) {
            save_screens(&core, &format!("{}/frame-{frame}.jpg", o.shot_dir));
        }
    }
    println!(
        "done at frame {frame}: {checked} keyframes checked, {bad} mismatched{}",
        first_bad.map(|f| format!(" (first at {f})")).unwrap_or_default()
    );
}

struct SectionDiff {
    section: String,
    bytes: usize,
    detail: String,
}

/// The two states with the parts that are not game state neutralised (see `canonical`), then
/// compared section by section.
fn compare(live: &[u8], recorded: &[u8]) -> Vec<SectionDiff> {
    let (live, recorded) = common_parts(live, recorded);
    if live.len() != recorded.len() {
        return vec![SectionDiff { section: "LENGTH".into(), bytes: 0, detail: format!("length {} vs {}", live.len(), recorded.len()) }];
    }
    let (a, b) = canonical(live, recorded);
    let mut out = Vec::new();
    let mut offset = 16usize;
    while offset + 16 <= a.len() {
        let len = u32::from_le_bytes([a[offset + 4], a[offset + 5], a[offset + 6], a[offset + 7]]) as usize;
        if len < 16 || offset + len > a.len() {
            break;
        }
        let magic = String::from_utf8_lossy(&a[offset..offset + 4]).into_owned();
        let sa = &a[offset..offset + len];
        let sb = &b[offset..offset + len];
        let differing: Vec<usize> = (0..len).filter(|&i| sa[i] != sb[i]).collect();
        if !differing.is_empty() {
            let detail = if magic == "NDSG" {
                describe_ram(sa, sb, &differing)
            } else {
                let offsets: Vec<String> = differing.iter().take(32).map(|i| format!("+{i}:{:02X}/{:02X}", sa[*i], sb[*i])).collect();
                format!("{magic}: {} bytes differ (live/recorded): {}", differing.len(), offsets.join(" "))
            };
            out.push(SectionDiff { section: magic, bytes: differing.len(), detail });
        }
        offset += len;
    }
    out
}

/// NDSG: 16-byte section header, a 4-byte field, then main RAM (16 MiB as saved).
fn describe_ram(sa: &[u8], sb: &[u8], differing: &[usize]) -> String {
    const RAM_START: usize = 16 + 4;
    const RAM_LEN: usize = 0x100_0000;
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for &i in differing {
        match ranges.last_mut() {
            Some((_, end)) if i <= *end + 4 => *end = i,
            _ => ranges.push((i, i)),
        }
    }
    let mut parts = Vec::new();
    for (s, e) in ranges.iter().take(24) {
        let (s, e) = (*s, *e);
        let live: String = sa[s..=e].iter().map(|b| format!("{b:02X}")).collect();
        let rec: String = sb[s..=e].iter().map(|b| format!("{b:02X}")).collect();
        if (RAM_START..RAM_START + RAM_LEN).contains(&s) {
            parts.push(format!("{:08X}..{:08X} live {live} rec {rec}", 0x0200_0000 + s - RAM_START, 0x0200_0000 + e - RAM_START));
        } else {
            parts.push(format!("NDSG+{s}..{e} live {live} rec {rec}"));
        }
    }
    format!("NDSG: {} bytes differ in {} ranges: {}", differing.len(), ranges.len(), parts.join("; "))
}

/// Copies of both states with the regenerated 3D buffers that masked keyframes leave stale
/// zeroed and the GX command FIFOs rewritten as their occupied entries (as nds_bench's
/// `states_match`).
fn canonical(live: &[u8], recorded: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let (live, recorded) = common_parts(live, recorded);
    let mut a = live.to_vec();
    let mut b = recorded.to_vec();
    if a.len() != b.len() {
        return (a, b);
    }
    for r in transient_ranges(ReplayConsoleType::NintendoDS, recorded) {
        a[r.clone()].fill(0);
        b[r].fill(0);
    }
    if let Some(gp3d) = find_section(recorded, b"GP3D") {
        for (offset, count) in [(16usize, 256usize), (16 + 2060, 4), (1_563_889, 64)] {
            canonicalize_fifo(&mut a, gp3d + offset, count);
            canonicalize_fifo(&mut b, gp3d + offset, count);
        }
    }
    (a, b)
}

fn canonicalize_fifo(state: &mut [u8], at: usize, count: usize) {
    let end = at + 12 + 8 * count;
    if end > state.len() {
        return;
    }
    let u32_at = |s: &[u8], o: usize| u32::from_le_bytes([s[o], s[o + 1], s[o + 2], s[o + 3]]) as usize;
    let occupied = u32_at(state, at).min(count);
    let read = u32_at(state, at + 4) % count.max(1);
    let entries = at + 12;
    let mut canonical = vec![0u8; 8 * count];
    for i in 0..occupied {
        let src = entries + 8 * ((read + i) % count);
        canonical[8 * i..8 * i + 5].copy_from_slice(&state[src..src + 5]);
    }
    state[at + 4..at + 12].fill(0);
    state[entries..end].copy_from_slice(&canonical);
}

/// Where the SHUK section Super Shuckie appends to every state (melonds-rs/interface.cpp) starts,
/// when `state` ends with one.
fn extra_section_start(state: &[u8]) -> Option<usize> {
    find_section(state, b"SHUK").filter(|&at| {
        let len = u32::from_le_bytes([state[at + 4], state[at + 5], state[at + 6], state[at + 7]]) as usize;
        at + len == state.len()
    })
}

/// The two states cut to what both carry: a keyframe from before states carried SHUK is compared
/// on the sections it has (the state header, which holds the length, is not compared).
fn common_parts<'a>(a: &'a [u8], b: &'a [u8]) -> (&'a [u8], &'a [u8]) {
    match (extra_section_start(a), extra_section_start(b)) {
        (Some(x), None) => (&a[..x], b),
        (None, Some(y)) => (a, &b[..y]),
        _ => (a, b),
    }
}

fn find_section(state: &[u8], magic: &[u8; 4]) -> Option<usize> {
    let mut offset = 16usize;
    while offset + 16 <= state.len() {
        let len = u32::from_le_bytes([state[offset + 4], state[offset + 5], state[offset + 6], state[offset + 7]]) as usize;
        if len < 16 || offset + len > state.len() {
            return None;
        }
        if &state[offset..offset + 4] == magic {
            return Some(offset);
        }
        offset += len;
    }
    None
}
