//! End-to-end checks of the frontend's RAM tools state against a real threaded core: viewer
//! samples, the search worker (snapshots, scans, undo, visible-row values) and game switches.
//!
//! ```text
//! memory_tools_smoke <rom.gba> <another rom.gba or .nds>
//! ```
//!
//! Link it like `supershuckie-core`'s `nds_bench` (see that file's header).

use std::time::{Duration, Instant};
use supershuckie_core::emulator::{EmulatorCore, GameBoyAdvance, NintendoDS};
use supershuckie_core::{std_timestamp_provider, ThreadedSuperShuckieCore};
use supershuckie_frontend::memory_tools::MemoryTools;
use supershuckie_memory_tools::search::{Comparison, SearchSettings};
use supershuckie_memory_tools::{Number, ValueFormat, ValueType};

fn make_core(path: &str) -> ThreadedSuperShuckieCore {
    let rom = std::fs::read(path).expect("read rom");
    let core: Box<dyn EmulatorCore> = if path.ends_with(".nds") {
        Box::new(NintendoDS::new_from_rom(&rom, None, std_timestamp_provider(), false))
    } else {
        Box::new(GameBoyAdvance::new_from_rom(&rom, None, &[], std_timestamp_provider()))
    };
    ThreadedSuperShuckieCore::new(core)
}

fn wait_until(tools: &mut MemoryTools, core: &ThreadedSuperShuckieCore, what: &str, mut done: impl FnMut(&MemoryTools) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        tools.tick(core, false);
        if done(tools) {
            return
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let first = args.next().expect("usage: memory_tools_smoke <rom> <other rom>");
    let second = args.next().expect("usage: memory_tools_smoke <rom> <other rom>");

    let dir = std::env::temp_dir().join("supershuckie-memory-tools-smoke");
    let mut tools = MemoryTools::new(dir.join("tables"));
    let core = make_core(&first);
    tools.core_switched(&core);
    assert!(!tools.regions().is_empty());
    println!("regions: {}", tools.regions().iter().map(|r| r.short_name.as_str()).collect::<Vec<_>>().join(", "));

    // Viewer samples.
    let base = tools.regions()[0].base;
    tools.set_viewer_window(&core, 0, Some((base, 256)));
    wait_until(&mut tools, &core, "a viewer sample", |t| t.read_viewer(0, 0).is_some_and(|s| s.valid_len == 256));
    println!("viewer sample ok");

    // Let the game run a little so values change between scans.
    std::thread::sleep(Duration::from_millis(1500));

    // New search: every byte.
    let settings = SearchSettings { format: ValueFormat::new(ValueType::U8, 1, false), alignment: 1, regions: vec![0, 1], range: None, epsilon: 0.01 };
    tools.search_scan(&core, Some(settings), Comparison::Unknown, false).expect("start");
    wait_until(&mut tools, &core, "the first scan", |t| { let s = t.search_status(); s.active && !s.busy });
    let all = tools.search_status().result_count;
    println!("unknown value: {all} results");
    assert!(all > 0);

    std::thread::sleep(Duration::from_millis(300));
    tools.search_scan(&core, None, Comparison::Changed, false).expect("refine");
    wait_until(&mut tools, &core, "the changed scan", |t| { let s = t.search_status(); !s.busy && s.steps == 2 });
    let changed = tools.search_status().result_count;
    println!("changed: {changed} results");
    assert!(changed < all);

    // Current values for visible rows.
    tools.search_set_visible_rows(&core, 0, 20);
    wait_until(&mut tools, &core, "visible row values", |t| { let (_, values) = t.search_visible_values(); !values.is_empty() && values.iter().all(|v| v.is_some()) });
    let rows = tools.search_results(0, 3);
    println!("first rows: {}", rows.iter().map(|r| format!("0x{:08X}", r.address)).collect::<Vec<_>>().join(", "));

    // Paused: a refine still gets its snapshot right away.
    core.pause();
    let started = Instant::now();
    tools.search_scan(&core, None, Comparison::Unchanged, false).expect("refine paused");
    wait_until(&mut tools, &core, "a scan while paused", |t| { let s = t.search_status(); !s.busy && s.steps == 3 });
    println!("scan while paused took {:?}", started.elapsed());
    assert!(started.elapsed() < Duration::from_millis(500));
    let paused_count = tools.search_status().result_count;
    tools.search_scan(&core, None, Comparison::Unchanged, false).expect("refine paused again");
    wait_until(&mut tools, &core, "a second scan while paused", |t| { let s = t.search_status(); !s.busy && s.steps == 4 });
    assert_eq!(tools.search_status().result_count, paused_count, "nothing changes while paused");
    core.start();

    // Undo/redo.
    tools.search_undo();
    wait_until(&mut tools, &core, "undo", |t| t.search_status().steps == 3);
    tools.search_redo();
    wait_until(&mut tools, &core, "redo", |t| t.search_status().steps == 4);

    // Refining with an equal value on top.
    tools.search_scan(&core, None, Comparison::GreaterOrEqual(Number::Int(0)), true).expect("refine with pause");
    wait_until(&mut tools, &core, "a paused-for scan", |t| !t.search_status().busy);
    assert!(!core.is_paused(), "emulation resumes after a scan that paused it");

    // A different game invalidates the search.
    let other = make_core(&second);
    tools.core_switched(&other);
    wait_until(&mut tools, &other, "the search reset", |t| !t.search_status().active);
    println!("after switching games: {:?}", tools.search_status().message);
    drop(core);

    println!("all checks passed");
}
