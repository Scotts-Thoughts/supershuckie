//! Headless Nintendo DS throughput benchmark.
//!
//! Drives the melonDS core the same way the app's core thread does (replay packets -> input ->
//! `run_unlocked` -> framebuffer copy), but with no UI, no pacing and no recorder, and reports how
//! many emulated frames per second the core alone can sustain.
//!
//! ```text
//! cargo run --release -p supershuckie-core --example nds_bench -- <rom.nds> [options]
//!
//!   --replay <file>     feed inputs from a replay (starts from its first keyframe)
//!   --state <file>      load a raw save state before running (instead of --replay)
//!   --start <frame>     with --replay: start from the keyframe at/before this frame instead of 0
//!   --frames <n>        frames to time (default 3000)
//!   --warmup <n>        frames to run before timing (default 300)
//!   --jit               enable the melonDS JIT
//!   --keyframes <n>     call create_save_state() every n frames and time it (0 = off)
//!   --present-every <n> draw only one frame in n (skip-drawing hint on the others), like the
//!                       app does at >= 2x; combine with --verify to prove states are unaffected
//!   --verify            with --replay: compare the live state against every recorded keyframe
//!                       (transient buffers excluded) and report desyncs
//!   --audio             enable audio and drain it after every frame, like the app does when the
//!                       user has audio on; combine with --verify to prove states are unaffected
//!   --repro <k>         with --replay: reproducibility test. Run core A for k frames, snapshot it,
//!                       load the snapshot into a fresh core B, then run both in lockstep on the
//!                       same inputs for --frames frames comparing states every --keyframes frames
//!
//! RAM tools (the memory monitor is serviced after every frame, the way the core thread does,
//! and its cost is reported separately):
//!   --mm-attach         attach a monitor with an idle request (the cost of merely attaching)
//!   --mm-window <bytes> sample a viewer window of this many bytes of main RAM
//!   --mm-hz <n>         samples per second of wall-clock time (default 30)
//!   --mm-probes <n>     sample n scattered 4-byte values
//!   --mm-traces <n>     compare n 4-byte watches every frame
//!   --mm-freezes <n>    freeze the n bytes the game changes most often (worst case: a restore
//!                       every frame; state-changing, so --verify reports desyncs)
//!   --mm-snapshot-every <n>  take a full memory snapshot every n frames (stress only; the app
//!                       takes one per search step)
//! ```
//!
//! Linking: the melonDS/mGBA static libraries are normally supplied by the CMake build, so pass
//! them on the command line, e.g. (MSYS2 UCRT64):
//!
//! ```text
//! cargo rustc --release -p supershuckie-core --example nds_bench -- \
//!     -L native=build/melonDS/src -L native=build/melonDS/src/teakra/src -L native=build/mgba \
//!     -l static=core -l static=teakra -l static=mgba -l shlwapi -l ws2_32
//! ```

use std::time::{Duration, Instant};

use supershuckie_core::emulator::{memory_slice, EmulatorCore, NintendoDS};
use supershuckie_core::memory_monitor::{AddressPath, FreezeSpec, MemoryMonitorLocal, MemoryMonitorShared, MonitoredCore, MonitorSample, Probe, TraceSpec, ValueDecode, ViewWindow};
use supershuckie_core::std_timestamp_provider;
use supershuckie_replay_recorder::keyframe_masks::transient_ranges;
use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::replay_file::ReplayConsoleType;
use supershuckie_replay_recorder::Packet;

struct Options {
    rom: String,
    replay: Option<String>,
    state: Option<String>,
    frames: u64,
    warmup: u64,
    start: u64,
    jit: bool,
    keyframes: u64,
    verify: bool,
    repro: Option<u64>,
    present_every: u64,
    audio: bool,
    mm_attach: bool,
    mm_window: u32,
    mm_hz: u32,
    mm_probes: u32,
    mm_traces: u32,
    mm_freezes: u32,
    mm_snapshot_every: u64,
}

impl Options {
    fn memory_monitor(&self) -> bool {
        self.mm_attach || self.mm_window > 0 || self.mm_probes > 0 || self.mm_traces > 0 || self.mm_freezes > 0 || self.mm_snapshot_every > 0
    }
}

/// The benchmark's bare core as seen by the memory monitor.
struct BenchCore<'a> {
    nds: &'a mut NintendoDS,
    frame: u64,
}

impl MonitoredCore for BenchCore<'_> {
    fn emulator(&self) -> &dyn EmulatorCore {
        self.nds
    }
    fn frame(&self) -> u64 {
        self.frame
    }
    fn state_epoch(&self) -> u64 {
        0
    }
    fn is_mid_frame(&self) -> bool {
        false
    }
    fn is_playing_back(&self) -> bool {
        false
    }
    fn finish_current_frame(&mut self) {}
    fn write(&mut self, address: u32, data: &[u8]) -> bool {
        self.nds.write_ram(address, data).is_ok()
    }
    fn write_if_changed(&mut self, address: u32, data: &[u8]) -> bool {
        match memory_slice(self.nds, address, data.len()) {
            Some(current) if current != data => self.write(address, data),
            _ => false,
        }
    }
}

fn parse_args() -> Options {
    let mut args = std::env::args().skip(1);
    let mut o = Options {
        rom: String::new(),
        replay: None,
        state: None,
        frames: 3000,
        warmup: 300,
        start: 0,
        jit: false,
        keyframes: 0,
        verify: false,
        repro: None,
        present_every: 1,
        audio: false,
        mm_attach: false,
        mm_window: 0,
        mm_hz: 30,
        mm_probes: 0,
        mm_traces: 0,
        mm_freezes: 0,
        mm_snapshot_every: 0,
    };
    while let Some(a) = args.next() {
        match a.as_str() {
            "--replay" => o.replay = args.next(),
            "--state" => o.state = args.next(),
            "--frames" => o.frames = args.next().expect("--frames n").parse().expect("frames"),
            "--warmup" => o.warmup = args.next().expect("--warmup n").parse().expect("warmup"),
            "--start" => o.start = args.next().expect("--start frame").parse().expect("start"),
            "--keyframes" => o.keyframes = args.next().expect("--keyframes n").parse().expect("keyframes"),
            "--jit" => o.jit = true,
            "--verify" => o.verify = true,
            "--audio" => o.audio = true,
            "--repro" => o.repro = Some(args.next().expect("--repro k").parse().expect("repro")),
            "--present-every" => o.present_every = args.next().expect("--present-every n").parse::<u64>().expect("present-every").max(1),
            "--mm-attach" => o.mm_attach = true,
            "--mm-window" => o.mm_window = args.next().expect("--mm-window bytes").parse().expect("mm-window"),
            "--mm-hz" => o.mm_hz = args.next().expect("--mm-hz n").parse::<u32>().expect("mm-hz").max(1),
            "--mm-probes" => o.mm_probes = args.next().expect("--mm-probes n").parse().expect("mm-probes"),
            "--mm-traces" => o.mm_traces = args.next().expect("--mm-traces n").parse().expect("mm-traces"),
            "--mm-freezes" => o.mm_freezes = args.next().expect("--mm-freezes n").parse().expect("mm-freezes"),
            "--mm-snapshot-every" => o.mm_snapshot_every = args.next().expect("--mm-snapshot-every n").parse().expect("mm-snapshot-every"),
            other if o.rom.is_empty() => o.rom = other.to_string(),
            other => panic!("unexpected argument {other}"),
        }
    }
    assert!(!o.rom.is_empty(), "usage: nds_bench <rom.nds> [--replay f] [--state f] [--frames n] [--warmup n] [--jit] [--keyframes n] [--verify]");
    o
}

/// Percentile of a sorted slice.
fn pct(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx]
}

/// Feed replay packets up to (and including) the next `NextFrame` into `core`. Returns `false`
/// when the replay is exhausted. The first keyframe is loaded as the initial state when
/// `load_initial` is set.
fn feed_frame(p: &mut ReplayFilePlayer, core: &mut NintendoDS, seen: &mut u64, load_initial: bool, last_input: &mut Vec<u8>) -> bool {
    loop {
        match p.next_packet() {
            Ok(None) | Err(_) => return false,
            Ok(Some(packet)) => match packet {
                Packet::NextFrame { .. } => return true,
                Packet::ChangeInput { data } => {
                    last_input.clear();
                    last_input.extend_from_slice(data.as_slice());
                    core.set_input_encoded(data.as_slice())
                }
                Packet::WriteMemory { address, data } => {
                    let _ = core.write_ram(*address as u32, data.as_slice());
                }
                Packet::ResetConsole => core.hard_reset(),
                Packet::LoadSaveState { state } => {
                    let _ = core.load_save_state(state.as_slice());
                }
                Packet::Keyframe { state, .. } => {
                    if *seen == 0 && load_initial {
                        core.load_save_state(state.as_slice()).expect("load initial keyframe");
                    }
                    *seen += 1;
                }
                _ => {}
            },
        }
    }
}

/// Reproducibility test (see module docs).
fn repro(o: &Options, rom: &[u8], k: u64) {
    let bytes = std::fs::read(o.replay.as_ref().expect("--repro needs --replay")).expect("read replay");
    let mut pa = ReplayFilePlayer::new(&bytes, true).expect("parse replay");
    let mut pb = ReplayFilePlayer::new(&bytes, true).expect("parse replay");
    pa.go_to_keyframe(0).expect("seek");
    pb.go_to_keyframe(0).expect("seek");

    let mut a = NintendoDS::new_from_rom(rom, None, std_timestamp_provider(), o.jit).expect("failed to load ROM");
    let mut seen_a = 0u64;
    let mut seen_b = 0u64;
    let mut input_a = Vec::new();
    let mut input_b = Vec::new();
    println!("repro: jit={}, warming core A for {k} frames", o.jit);
    for _ in 0..k {
        assert!(feed_frame(&mut pa, &mut a, &mut seen_a, true, &mut input_a), "replay too short");
        a.run_unlocked();
    }
    // Advance player B to the same position without emulating (its core is replaced below).
    let mut scratch = NintendoDS::new_from_rom(rom, None, std_timestamp_provider(), o.jit).expect("failed to load ROM");
    for _ in 0..k {
        assert!(feed_frame(&mut pb, &mut scratch, &mut seen_b, false, &mut input_b), "replay too short");
    }
    drop(scratch);

    let snapshot = a.create_save_state();
    let mut b = NintendoDS::new_from_rom(rom, None, std_timestamp_provider(), o.jit).expect("failed to load ROM");
    b.load_save_state(&snapshot).expect("load snapshot into B");
    // melonDS save states do not carry KeyInput, so mirror A's current input explicitly.
    if !input_a.is_empty() {
        b.set_input_encoded(&input_a);
    }
    println!("repro: core B loaded A's {}-byte snapshot; running both for {} frames", snapshot.len(), o.frames);

    let every = o.keyframes.max(1);
    let mut first_bad = None;
    let mut checked = 0;
    for f in 1..=o.frames {
        let ca = feed_frame(&mut pa, &mut a, &mut seen_a, false, &mut input_a);
        let cb = feed_frame(&mut pb, &mut b, &mut seen_b, false, &mut input_b);
        if !ca || !cb {
            println!("replay ended at +{f}");
            break;
        }
        a.run_unlocked();
        b.run_unlocked();
        if f % every == 0 {
            checked += 1;
            let sa = a.create_save_state();
            let sb = b.create_save_state();
            if !states_match(ReplayConsoleType::NintendoDS, &sa, &sb) {
                first_bad = Some(f);
                println!("repro: DIVERGED at +{f} frames after the snapshot: {}", describe_diff(&sa, &sb));
                break;
            }
        }
    }
    match first_bad {
        None => println!("repro: {checked} comparisons, core B (fresh) matched core A (warm) throughout"),
        Some(_) => println!("repro: {checked} comparisons, first mismatch reported above"),
    }
}

fn main() {
    let o = parse_args();

    let rom = std::fs::read(&o.rom).expect("read rom");
    if let Some(k) = o.repro {
        repro(&o, &rom, k);
        return;
    }
    let mut core = NintendoDS::new_from_rom(&rom, None, std_timestamp_provider(), o.jit).expect("failed to load ROM");
    core.set_audio_enabled(o.audio);
    let mut audio_scratch: Vec<i16> = Vec::new();
    let mut audio_frames: u64 = 0;
    println!(
        "{} ({} MiB), jit={}, frames={}, warmup={}, keyframes every {}, present every {}, audio={}",
        o.rom,
        rom.len() >> 20,
        o.jit,
        o.frames,
        o.warmup,
        o.keyframes,
        o.present_every,
        o.audio
    );

    let mut player = o.replay.as_ref().map(|path| {
        let t = Instant::now();
        let bytes = std::fs::read(path).expect("read replay");
        let mut p = ReplayFilePlayer::new(&bytes, true).expect("parse replay");
        println!(
            "replay {path}: v{}, {} frames, {} keyframes, parsed in {:.0} ms",
            p.get_replay_version(),
            p.get_total_frames(),
            p.all_keyframes().len(),
            t.elapsed().as_secs_f64() * 1000.0
        );
        if let Err(e) = p.go_to_keyframe(o.start) {
            match e {
                supershuckie_replay_recorder::replay_file::playback::ReplaySeekError::NoSuchKeyframe { best, .. } => {
                    p.go_to_keyframe(best).expect("seek best");
                }
                other => panic!("seek: {other:?}"),
            }
        }
        p
    });

    if let Some(path) = o.state.as_ref() {
        let state = std::fs::read(path).expect("read state");
        core.load_save_state(&state).expect("load state");
        println!("loaded state {path} ({} bytes)", state.len());
    }

    // Replay bookkeeping, mirroring SuperShuckieCore::handle_replay.
    let mut frame: u64 = 0;
    let mut stalled = false;
    let mut keyframes_seen = 0u64;
    let mut keyframes_ok = 0u64;
    let mut keyframes_bad = 0u64;
    let mut first_bad: Option<u64> = None;

    // RAM tools: the monitor, a stand-in UI that drains samples, and a stand-in search worker
    // that recycles snapshot buffers.
    let monitor_shared = MemoryMonitorShared::new();
    let mut monitor = o.memory_monitor().then(|| MemoryMonitorLocal::new(monitor_shared.clone()));
    let mut monitor_times: Vec<Duration> = Vec::new();
    let mut snapshot_times: Vec<Duration> = Vec::new();
    let mut samples_taken = 0u64;
    let mut ui_sample = MonitorSample::default();
    let mut ui_sample_generation = 0u64;
    let mut snapshot_job = 0u64;
    let mut change_counts: Vec<u16> = Vec::new();
    let mut last_ram: Vec<u8> = Vec::new();
    let snapshot_receiver = if o.mm_snapshot_every > 0 {
        let (sender, receiver) = std::sync::mpsc::channel::<supershuckie_core::memory_monitor::FullSnapshot>();
        monitor_shared.set_snapshot_sender(Some(sender));
        Some(receiver)
    } else {
        None
    };
    const MAIN_RAM: u32 = 0x0200_0000;
    const MAIN_RAM_LEN: u32 = 0x40_0000;

    let mut frame_times: Vec<Duration> = Vec::with_capacity(o.frames as usize);
    let mut state_times: Vec<Duration> = Vec::new();
    let mut packet_time = Duration::ZERO;
    let mut timing = false;
    let mut timed_frames = 0u64;

    // Warm-up and timed runs share one loop so the replay cursor is continuous.
    let mut i = 0u64;
    loop {
        if i == o.warmup {
            timing = true;
            frame_times.clear();
            state_times.clear();
            packet_time = Duration::ZERO;
        }
        if i >= o.warmup + o.frames || stalled {
            break;
        }
        i += 1;

        // Configure the monitor once warm-up has shown which bytes the game keeps changing.
        if i == o.warmup + 1 && monitor.is_some() {
            let mut busiest: Vec<(u16, u32)> = change_counts.iter().enumerate().map(|(offset, count)| (*count, offset as u32)).collect();
            busiest.sort_unstable_by(|a, b| b.0.cmp(&a.0));
            monitor_shared.update_request(|r| {
                r.sampling = o.mm_window > 0 || o.mm_probes > 0;
                r.interval = Duration::from_micros(1_000_000 / o.mm_hz as u64);
                if o.mm_window > 0 {
                    r.windows[0] = Some(ViewWindow { address: MAIN_RAM + 0x10_0000, len: o.mm_window });
                }
                let spread = |index: u32, count: u32| MAIN_RAM + (MAIN_RAM_LEN / count.max(1)) * index;
                r.probes = (0..o.mm_probes).map(|n| Probe { path: AddressPath::direct(spread(n, o.mm_probes)), len: 4 }).collect();
                r.traces = (0..o.mm_traces).map(|n| TraceSpec {
                    id: n,
                    path: AddressPath::direct(spread(n, o.mm_traces) & !3),
                    len: 4,
                    decode: ValueDecode::default(),
                    pause_when: None,
                }).collect();
                r.freezes = busiest.iter().take(o.mm_freezes as usize).enumerate().map(|(n, (_, offset))| {
                    FreezeSpec::new(n as u32, AddressPath::direct(MAIN_RAM + offset), &[0x5A]).unwrap()
                }).collect();
            });
            if o.mm_freezes > 0 {
                let changing = busiest.iter().take(o.mm_freezes as usize).filter(|(count, _)| *count as u64 >= o.warmup.min(60) / 2).count();
                println!("freezing the {} most-changed bytes ({changing} of them changed on at least half the last warm-up frames)", o.mm_freezes);
            }
        }

        // 1. Replay packets up to the next NextFrame.
        if let Some(p) = player.as_mut() {
            let t = Instant::now();
            loop {
                match p.next_packet() {
                    Ok(None) | Err(_) => {
                        stalled = true;
                        break;
                    }
                    Ok(Some(packet)) => match packet {
                        Packet::NextFrame { .. } => break,
                        Packet::ChangeInput { data } => core.set_input_encoded(data.as_slice()),
                        Packet::WriteMemory { address, data } => {
                            let _ = core.write_ram(*address as u32, data.as_slice());
                        }
                        Packet::ResetConsole => core.hard_reset(),
                        Packet::LoadSaveState { state } => {
                            let _ = core.load_save_state(state.as_slice());
                        }
                        Packet::Keyframe { metadata, state } => {
                            if keyframes_seen == 0 {
                                // The first keyframe is the recording's initial state.
                                core.load_save_state(state.as_slice()).expect("load initial keyframe");
                                frame = metadata.elapsed_frames;
                            } else if o.verify {
                                let live = core.create_save_state();
                                let ok = states_match(ReplayConsoleType::NintendoDS, &live, state.as_slice());
                                if ok {
                                    keyframes_ok += 1;
                                } else {
                                    keyframes_bad += 1;
                                    if first_bad.is_none() {
                                        first_bad = Some(metadata.elapsed_frames);
                                        println!("first mismatch at keyframe frame {}: {}", metadata.elapsed_frames, describe_diff(&live, state.as_slice()));
                                    }
                                }
                            }
                            keyframes_seen += 1;
                        }
                        _ => {}
                    },
                }
            }
            if timing {
                packet_time += t.elapsed();
            }
            if stalled {
                break;
            }
        }

        // 2. Emulate one frame (includes the framebuffer copy, like the app).
        core.set_skip_drawing(o.present_every > 1 && frame % o.present_every != 0);
        let t = Instant::now();
        core.run_unlocked();
        if o.audio {
            core.take_audio(&mut audio_scratch);
            audio_frames += (audio_scratch.len() / 2) as u64;
            audio_scratch.clear();
        }
        let dt = t.elapsed();
        frame += 1;
        if timing {
            frame_times.push(dt);
            timed_frames += 1;
        }

        // RAM tools, serviced between frames like the core thread does.
        if let Some(m) = monitor.as_mut() {
            if !timing && o.mm_freezes > 0 && o.warmup.saturating_sub(i) < 60 {
                let ram = core.memory_region_data(0).expect("main RAM");
                if last_ram.is_empty() {
                    last_ram = ram.to_vec();
                    change_counts = vec![0; ram.len()];
                } else {
                    for ((last, now), count) in last_ram.iter_mut().zip(ram.iter()).zip(change_counts.iter_mut()) {
                        if last != now {
                            *count += 1;
                            *last = *now;
                        }
                    }
                }
            }
            if o.mm_snapshot_every > 0 && frame % o.mm_snapshot_every == 0 {
                snapshot_job += 1;
                monitor_shared.request_snapshot(snapshot_job);
            }
            let snapshot_due = o.mm_snapshot_every > 0 && frame % o.mm_snapshot_every == 0;
            let t = Instant::now();
            m.service(&mut BenchCore { nds: &mut core, frame }, true);
            let dt = t.elapsed();
            if timing {
                if snapshot_due {
                    snapshot_times.push(dt);
                } else {
                    monitor_times.push(dt);
                }
            }
            if let Some(g) = monitor_shared.take_sample(ui_sample_generation, &mut ui_sample) {
                ui_sample_generation = g;
                samples_taken += 1;
            }
            if let Some(receiver) = snapshot_receiver.as_ref() {
                while let Ok(snapshot) = receiver.try_recv() {
                    monitor_shared.recycle_snapshot_buffer(snapshot.bytes);
                }
            }
        }

        // 3. Periodic save state, like recording keyframes.
        if o.keyframes != 0 && frame % o.keyframes == 0 {
            let t = Instant::now();
            let s = core.create_save_state();
            let dt = t.elapsed();
            if timing {
                state_times.push(dt);
            }
            std::hint::black_box(s);
        }
    }

    if timed_frames == 0 {
        println!("no frames timed (replay too short?)");
        return;
    }

    let emu: Duration = frame_times.iter().sum();
    let states: Duration = state_times.iter().sum();
    let wall = emu + packet_time + states;
    frame_times.sort();

    println!();
    println!("timed {timed_frames} frames");
    println!(
        "  emulation only : {:8.1} fps  (mean {:.3} ms, p50 {:.3}, p90 {:.3}, p99 {:.3}, max {:.3} ms)",
        timed_frames as f64 / emu.as_secs_f64(),
        emu.as_secs_f64() * 1000.0 / timed_frames as f64,
        pct(&frame_times, 0.50).as_secs_f64() * 1000.0,
        pct(&frame_times, 0.90).as_secs_f64() * 1000.0,
        pct(&frame_times, 0.99).as_secs_f64() * 1000.0,
        frame_times.last().unwrap().as_secs_f64() * 1000.0
    );
    println!(
        "  incl. packets  : {:8.1} fps  (packet handling {:.2} ms total)",
        timed_frames as f64 / (emu + packet_time).as_secs_f64(),
        packet_time.as_secs_f64() * 1000.0
    );
    if !state_times.is_empty() {
        state_times.sort();
        println!(
            "  incl. keyframes: {:8.1} fps  ({} save states, mean {:.2} ms, max {:.2} ms each)",
            timed_frames as f64 / wall.as_secs_f64(),
            state_times.len(),
            states.as_secs_f64() * 1000.0 / state_times.len() as f64,
            state_times.last().unwrap().as_secs_f64() * 1000.0
        );
    }
    let over_budget = frame_times.iter().filter(|d| **d > Duration::from_micros(4166)).count();
    println!(
        "  frames over the 4x budget (4.166 ms): {over_budget} of {timed_frames} ({:.2}%)",
        over_budget as f64 * 100.0 / timed_frames as f64
    );
    if monitor.is_some() {
        let total: Duration = monitor_times.iter().sum();
        monitor_times.sort();
        let n = monitor_times.len().max(1) as f64;
        println!(
            "  memory monitor : mean {:.1} us per frame, p99 {:.1} us, max {:.1} us ({:.3}% of the emulation time); {samples_taken} samples published",
            total.as_secs_f64() * 1e6 / n,
            pct(&monitor_times, 0.99).as_secs_f64() * 1e6,
            monitor_times.last().copied().unwrap_or_default().as_secs_f64() * 1e6,
            total.as_secs_f64() * 100.0 / emu.as_secs_f64()
        );
        if !snapshot_times.is_empty() {
            let total: Duration = snapshot_times.iter().sum();
            snapshot_times.sort();
            println!(
                "  full snapshots : {} taken, mean {:.3} ms, max {:.3} ms each",
                snapshot_times.len(),
                total.as_secs_f64() * 1000.0 / snapshot_times.len() as f64,
                snapshot_times.last().unwrap().as_secs_f64() * 1000.0
            );
        }
    }
    if o.audio {
        println!(
            "  audio: {audio_frames} stereo frames drained ({:.1} per emulated frame; ~802 expected at 48 kHz)",
            audio_frames as f64 / frame.max(1) as f64
        );
    }
    if o.verify {
        println!(
            "  keyframes checked: {} ok, {} desynced{}",
            keyframes_ok,
            keyframes_bad,
            first_bad.map(|f| format!(" (first at frame {f})")).unwrap_or_default()
        );
    }
    if stalled {
        println!("  (replay ended)");
    }
}

/// Describe which melonDS save-state sections differ between two states (section magic, differing
/// byte count, first differing offset within the section).
fn describe_diff(a: &[u8], b: &[u8]) -> String {
    if a.len() != b.len() {
        return format!("length {} vs {}", a.len(), b.len());
    }
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
        let diff = sa.iter().zip(sb).filter(|(x, y)| x != y).count();
        if diff > 0 {
            let first = sa.iter().zip(sb).position(|(x, y)| x != y).unwrap_or(0);
            let offsets: Vec<String> = sa.iter().zip(sb).enumerate().filter(|(_, (x, y))| x != y).take(24).map(|(i, _)| i.to_string()).collect();
            out.push(format!("{magic}: {diff} bytes (first at +{first} of {len}; offsets {})", offsets.join(" ")));
        }
        offset += len;
    }
    out.join(", ")
}

/// Compare two NDS save states, ignoring the regenerated 3D buffers that masked keyframes leave
/// stale (see `keyframe_masks`) and normalising the GX command FIFOs.
///
/// The three FIFOs (`CmdFIFO`, `CmdPIPE`, `CmdStallQueue`) are ring buffers saved raw: their
/// read/write positions and the stale entries outside the occupied range are not game state (two
/// runs that loaded states at different points carry different ring phases yet emulate
/// identically), and before the padding fix bytes 5..8 of every entry were stack garbage. Each
/// FIFO is rewritten as its occupied entries in order, positions zeroed, entry padding zeroed.
fn states_match(console: ReplayConsoleType, live: &[u8], recorded: &[u8]) -> bool {
    if live.len() != recorded.len() {
        return false;
    }
    let mut a = live.to_vec();
    let mut b = recorded.to_vec();
    for r in transient_ranges(console, recorded) {
        a[r.clone()].fill(0);
        b[r].fill(0);
    }
    if let Some(gp3d) = find_section(recorded, b"GP3D") {
        // (section-relative offset of NumOccupied, entry count) for CmdFIFO, CmdPIPE and
        // CmdStallQueue as laid out by GPU3D::DoSavestate for savestate major 13.
        for (offset, count) in [(16usize, 256usize), (16 + 2060, 4), (1_563_889, 64)] {
            canonicalize_fifo(&mut a, gp3d + offset, count);
            canonicalize_fifo(&mut b, gp3d + offset, count);
        }
    }
    a == b
}

/// Rewrite one `FIFO<CmdFIFOEntry, N>` in place into its canonical form (see `states_match`).
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
        canonical[8 * i..8 * i + 5].copy_from_slice(&state[src..src + 5]); // Param + Command, no padding
    }
    state[at + 4..at + 12].fill(0);
    state[entries..end].copy_from_slice(&canonical);
}

/// Offset of the melonDS save-state section with the given magic.
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
