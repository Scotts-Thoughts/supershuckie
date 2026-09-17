//! The RAM tools' cost on the real core thread loop (pacing included) at a given speed: runs the
//! game with no tools, then with a busy tool setup (two viewer windows, visible watch and search
//! rows, traced watches on bytes the game keeps changing, freezes on bytes it leaves alone),
//! alternating, and compares emulated frames per second and frame times. Every run starts from the
//! same save state, so both emulate exactly the same frames.
//!
//! ```text
//! ram_tools_pacing <rom.nds|rom.gba> [--speed 4] [--seconds 20] [--rounds 2]
//! ```
//!
//! Link it like `supershuckie-core`'s `nds_bench` (see that file's header).

use std::time::{Duration, Instant};
use supershuckie_core::emulator::{EmulatorCore, GameBoyAdvance, NintendoDS};
use supershuckie_core::{std_timestamp_provider, Speed, ThreadedSuperShuckieCore};
use supershuckie_frontend::memory_tools::MemoryTools;
use supershuckie_memory_tools::search::{Comparison, SearchSettings};
use supershuckie_memory_tools::watch::{Watch, WatchAddress};
use supershuckie_memory_tools::{DisplayBase, ValueFormat, ValueType};

fn make_core(path: &str) -> ThreadedSuperShuckieCore {
    let rom = std::fs::read(path).expect("read rom");
    let core: Box<dyn EmulatorCore> = if path.ends_with(".nds") {
        Box::new(NintendoDS::new_from_rom(&rom, None, std_timestamp_provider(), false).expect("load nds rom"))
    } else {
        Box::new(GameBoyAdvance::new_from_rom(&rom, None, &[], std_timestamp_provider()).expect("load gba rom"))
    };
    ThreadedSuperShuckieCore::new(core)
}

struct Result {
    fps: f64,
    average_ms: f64,
    over_budget_percent: f64
}

fn run(tools: &mut MemoryTools, core: &ThreadedSuperShuckieCore, state: &[u8], seconds: u64) -> Result {
    core.load_save_state(state.to_vec());
    std::thread::sleep(Duration::from_millis(200));
    core.reset_frame_time_stats();
    let start_frames = core.get_emulated_frame_count();
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(seconds) {
        // The UI thread ticks the frontend every millisecond.
        tools.tick(core, false, false, false);
        let _ = tools.read_viewer(0, 0);
        let _ = tools.watch_values();
        let _ = tools.search_visible_values();
        let mut log = Vec::new();
        log.extend(tools.drain_log(1000).0);
        std::thread::sleep(Duration::from_millis(1));
    }
    let frames = core.get_emulated_frame_count() - start_frames;
    let stats = core.get_frame_time_stats();
    Result {
        fps: frames as f64 / started.elapsed().as_secs_f64(),
        average_ms: stats.average_frame_micros as f64 / 1000.0,
        over_budget_percent: stats.frames_over_budget as f64 * 100.0 / stats.frames_measured.max(1) as f64
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let rom = args.next().expect("usage: ram_tools_pacing <rom> [--speed n] [--seconds n] [--rounds n]");
    let mut speed = 4.0;
    let mut seconds = 20;
    let mut rounds = 2;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--speed" => speed = args.next().unwrap().parse().unwrap(),
            "--seconds" => seconds = args.next().unwrap().parse().unwrap(),
            "--rounds" => rounds = args.next().unwrap().parse().unwrap(),
            other => panic!("unexpected {other}")
        }
    }

    let dir = std::env::temp_dir().join("supershuckie-ram-tools-pacing");
    let _ = std::fs::remove_dir_all(&dir);
    let core = make_core(&rom);
    core.set_speed(Speed::from_multiplier_float(speed));
    let mut tools = MemoryTools::new(dir.join("tables"));
    tools.core_switched(&core, Some(dir.join("ram-watch.json")));
    println!("{rom} at {speed}x, {seconds} s per run, {rounds} rounds");

    // Warm up past the boot sequence, then find bytes the game keeps changing for the traces and
    // freezes (the worst case: a freeze restoring its value every frame).
    std::thread::sleep(Duration::from_secs(8));
    let regions: Vec<usize> = (0..tools.regions().len().min(2)).collect();
    let settings = SearchSettings { format: ValueFormat::new(ValueType::U8, 1, false), alignment: 1, regions, range: None, epsilon: 0.01 };
    tools.search_scan(&core, Some(settings), Comparison::Unknown, false).unwrap();
    while tools.search_status().busy { tools.tick(&core, false, false, false); std::thread::sleep(Duration::from_millis(1)); }
    for _ in 0..4 {
        std::thread::sleep(Duration::from_millis(40));
        tools.search_scan(&core, None, Comparison::Changed, false).unwrap();
        while tools.search_status().busy { tools.tick(&core, false, false, false); std::thread::sleep(Duration::from_millis(1)); }
    }
    let busy: Vec<u32> = tools.search_results(0, 10).iter().map(|r| r.address).collect();
    println!("{} candidate bytes that keep changing; tracing {}", tools.search_status().result_count, busy.len());
    assert!(busy.len() >= 10, "not enough busy bytes to set up the test");

    // Bytes that stay put, to freeze without changing what the game does.
    let settings = SearchSettings { format: ValueFormat::new(ValueType::U8, 1, false), alignment: 1, regions: vec![0], range: None, epsilon: 0.01 };
    tools.search_scan(&core, Some(settings), Comparison::Unknown, false).unwrap();
    while tools.search_status().busy { tools.tick(&core, false, false, false); std::thread::sleep(Duration::from_millis(1)); }
    for _ in 0..6 {
        std::thread::sleep(Duration::from_millis(200));
        tools.search_scan(&core, None, Comparison::Unchanged, false).unwrap();
        while tools.search_status().busy { tools.tick(&core, false, false, false); std::thread::sleep(Duration::from_millis(1)); }
    }
    let middle = tools.search_status().result_count / 2;
    let steady: Vec<(u32, u8)> = tools.search_results(middle, 20).iter().map(|r| (r.address, r.previous[0])).collect();
    let state = core.create_save_state().expect("save state");

    let base = tools.regions()[0].base;
    let enable = |tools: &mut MemoryTools, on: bool| {
        if !on {
            tools.set_viewer_window(&core, 0, None);
            tools.set_viewer_window(&core, 1, None);
            tools.set_visible_watches(&core, &[]);
            tools.search_set_visible_rows(&core, 0, 0);
            for id in tools.watches().iter().map(|w| w.id).collect::<Vec<_>>() {
                tools.remove_watch(&core, id);
            }
            return
        }
        tools.set_viewer_window(&core, 0, Some((base + 0x10_0000, 64 * 32)));
        tools.set_viewer_window(&core, 1, Some((base, 40 * 16)));
        let mut ids = Vec::new();
        let addresses: Vec<u32> = busy.iter().copied().chain(steady.iter().map(|(a, _)| *a)).collect();
        for (i, address) in addresses.iter().enumerate() {
            let watch = Watch {
                id: 0,
                label: format!("w{i}"),
                address: WatchAddress::direct(*address),
                format: ValueFormat::new(ValueType::U8, 1, false),
                display: DisplayBase::Decimal,
                table: String::new(),
                group: String::new(),
                notes: String::new(),
                trace: i < 10,
                pause_when: None,
                freeze: None
            };
            ids.push(tools.upsert_watch(&core, watch).unwrap());
        }
        for (id, (_, value)) in ids.iter().skip(10).zip(steady.iter()).take(16) {
            tools.set_freeze(&core, *id, Some(vec![*value])).unwrap();
        }
        tools.set_visible_watches(&core, &ids[..20]);
        tools.search_set_visible_rows(&core, 0, 50);
    };

    let mut off = Vec::new();
    let mut on = Vec::new();
    for round in 0..rounds {
        enable(&mut tools, false);
        let r = run(&mut tools, &core, &state, seconds);
        println!("round {round} without tools: {:7.1} fps, {:.3} ms average frame, {:5.2}% over budget", r.fps, r.average_ms, r.over_budget_percent);
        off.push(r);
        enable(&mut tools, true);
        let r = run(&mut tools, &core, &state, seconds);
        println!("round {round} with tools:    {:7.1} fps, {:.3} ms average frame, {:5.2}% over budget ({} frozen)", r.fps, r.average_ms, r.over_budget_percent, tools.frozen_count());
        on.push(r);
    }
    let mean = |v: &[Result], f: fn(&Result) -> f64| v.iter().map(f).sum::<f64>() / v.len() as f64;
    println!();
    println!("without tools: {:7.1} fps, {:.3} ms, {:5.2}% over budget", mean(&off, |r| r.fps), mean(&off, |r| r.average_ms), mean(&off, |r| r.over_budget_percent));
    println!("with tools:    {:7.1} fps, {:.3} ms, {:5.2}% over budget", mean(&on, |r| r.fps), mean(&on, |r| r.average_ms), mean(&on, |r| r.over_budget_percent));
    println!("difference:    {:+.2}% fps", (mean(&on, |r| r.fps) / mean(&off, |r| r.fps) - 1.0) * 100.0);
}
