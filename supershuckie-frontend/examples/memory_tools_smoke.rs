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
use supershuckie_core::memory_monitor::MAX_FREEZE_BYTES;
use supershuckie_core::{std_timestamp_provider, ThreadedSuperShuckieCore};
use supershuckie_frontend::memory_tools::{LogKind, MemoryTools};
use supershuckie_memory_tools::search::{Comparison, SearchSettings};
use supershuckie_memory_tools::watch::{FreezeState, Watch, WatchAddress, WatchCondition};
use supershuckie_memory_tools::{DisplayBase, Number, ValueFormat, ValueType};

fn make_core(path: &str) -> ThreadedSuperShuckieCore {
    let rom = std::fs::read(path).expect("read rom");
    let core: Box<dyn EmulatorCore> = if path.ends_with(".nds") {
        Box::new(NintendoDS::new_from_rom(&rom, None, std_timestamp_provider(), false))
    } else {
        Box::new(GameBoyAdvance::new_from_rom(&rom, None, &[], std_timestamp_provider()))
    };
    ThreadedSuperShuckieCore::new(core)
}

fn wait_until(tools: &mut MemoryTools, core: &ThreadedSuperShuckieCore, what: &str, mut done: impl FnMut(&mut MemoryTools) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        tools.tick(core, false, false, false);
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
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let first_watches = dir.join("first-ram-watch.json");
    let second_watches = dir.join("second-ram-watch.json");
    let mut tools = MemoryTools::new(dir.join("tables"));
    let core = make_core(&first);
    tools.core_switched(&core, Some(first_watches.clone()));
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

    // Watches: a busy value found by the search, logged every frame, pausing when it changes.
    let busy = tools.search_results(0, 1)[0].address;
    let watch = Watch {
        id: 0,
        label: "busy byte".to_owned(),
        address: WatchAddress::direct(busy),
        format: ValueFormat::new(ValueType::U8, 1, false),
        display: DisplayBase::Hex,
        table: String::new(),
        group: "Test".to_owned(),
        notes: String::new(),
        trace: true,
        pause_when: None,
        freeze: None
    };
    let id = tools.upsert_watch(&core, watch.clone()).expect("add watch");
    tools.set_visible_watches(&core, &[id]);
    wait_until(&mut tools, &core, "a watch value", |t| t.watch_values().first().is_some_and(|v| v.value.is_some()));
    println!("watch value: {}", tools.watch_values()[0].text);
    // The search found bytes that were changing, but not all keep changing; log what happens.
    std::thread::sleep(Duration::from_millis(500));
    wait_until(&mut tools, &core, "log lines", |_| true);
    let (lines, _) = tools.drain_log(100);
    println!("{} log lines, e.g. {:?}", lines.len(), lines.first().map(|l| (l.frame, &l.text)));

    // Pause on change: find a byte that changes nearly every frame by logging several.
    let frame_counter = {
        let settings = SearchSettings { format: ValueFormat::new(ValueType::U8, 1, false), alignment: 1, regions: vec![1], range: None, epsilon: 0.01 };
        tools.search_scan(&core, Some(settings), Comparison::Unknown, false).unwrap();
        wait_until(&mut tools, &core, "iwram scan", |t| !t.search_status().busy);
        for _ in 0..3 {
            std::thread::sleep(Duration::from_millis(50));
            tools.search_scan(&core, None, Comparison::Changed, false).unwrap();
            wait_until(&mut tools, &core, "changed scan", |t| !t.search_status().busy);
        }
        tools.search_results(0, 1).first().map(|r| r.address).expect("a byte that keeps changing")
    };
    let pausing = Watch { label: "pauser".to_owned(), address: WatchAddress::direct(frame_counter), trace: false, pause_when: Some(WatchCondition::Changes), group: String::new(), ..watch.clone() };
    let pause_id = tools.upsert_watch(&core, pausing).expect("add pausing watch");
    wait_until(&mut tools, &core, "the pause condition", |_| core.is_paused());
    wait_until(&mut tools, &core, "the pause log line", |t| t.drain_log(1000).0.iter().any(|l| l.kind == LogKind::Paused && l.watch_id == pause_id));
    println!("pause condition paused emulation");
    tools.remove_watch(&core, pause_id);
    core.start();

    // Traced watch cap.
    let mut too_many = 0;
    for i in 0..70 {
        let w = Watch { label: format!("t{i}"), address: WatchAddress::direct(busy), ..watch.clone() };
        if tools.upsert_watch(&core, w).is_err() {
            too_many += 1;
        }
    }
    assert!(too_many > 0, "the traced watch cap is enforced");

    // Editing: a write lands, undo restores the exact old bytes, redo reapplies.
    let target = tools.regions()[0].base + 0x100;
    let edit_watch = Watch { label: "edited".to_owned(), address: WatchAddress::direct(target), format: ValueFormat::new(ValueType::U16, 2, false), trace: false, group: String::new(), ..watch.clone() };
    // Make room under the trace cap for these checks.
    let ids: Vec<u32> = tools.watches().iter().skip(1).map(|w| w.id).collect();
    for id in ids {
        tools.remove_watch(&core, id);
    }
    let edit_id = tools.upsert_watch(&core, edit_watch).expect("edit watch");
    tools.set_visible_watches(&core, &[edit_id]);
    wait_until(&mut tools, &core, "the edited watch's value", |t| t.watch_values().first().is_some_and(|v| v.value.is_some()));
    core.pause();
    std::thread::sleep(Duration::from_millis(50));
    wait_until(&mut tools, &core, "a settled value", |_| true);
    let original = tools.watch_values()[0].value.clone().unwrap();
    tools.write(&core, WatchAddress::direct(target), vec![0x34, 0x12]).expect("write");
    wait_until(&mut tools, &core, "the written value", |t| t.watch_values()[0].value.as_deref() == Some(&[0x34, 0x12][..]));
    assert!(tools.can_undo());
    tools.undo(&core).expect("undo");
    wait_until(&mut tools, &core, "the undone value", |t| t.watch_values()[0].value.as_deref() == Some(original.as_slice()));
    tools.redo(&core).expect("redo");
    wait_until(&mut tools, &core, "the redone value", |t| t.watch_values()[0].value.as_deref() == Some(&[0x34, 0x12][..]));
    println!("write, undo and redo ok ({:02X?} -> 34 12)", original);

    // Read-only memory refuses edits.
    if let Some(read_only) = tools.regions().iter().find(|r| !r.writable).map(|r| r.base) {
        tools.write(&core, WatchAddress::direct(read_only), vec![1]).unwrap();
        wait_until(&mut tools, &core, "the refused edit", |t| t.take_edit_message().is_some());
        println!("read-only edit refused");
    }

    // Freezing holds a value against the game; editing a frozen value changes the freeze.
    core.start();
    let frozen_id = tools.freeze_new(&core, WatchAddress::direct(frame_counter), ValueFormat::new(ValueType::U8, 1, false), vec![0x42], "Frozen").expect("freeze");
    tools.set_visible_watches(&core, &[edit_id, frozen_id]);
    wait_until(&mut tools, &core, "the freeze restoring the value", |t| t.freeze_status(frozen_id).is_some_and(|(restores, ok)| ok && restores > 5));
    let held = tools.watch_values().iter().find(|v| v.id == frozen_id).and_then(|v| v.value.clone());
    assert_eq!(held.as_deref(), Some(&[0x42][..]), "the frozen value holds at frame boundaries");
    tools.write(&core, WatchAddress::direct(frame_counter), vec![0x43]).expect("edit frozen");
    assert_eq!(tools.watches().iter().find(|w| w.id == frozen_id).unwrap().freeze.as_ref().unwrap().value, vec![0x43], "editing a frozen value changes the freeze");
    wait_until(&mut tools, &core, "the new frozen value", |t| t.watch_values().iter().any(|v| v.id == frozen_id && v.value.as_deref() == Some(&[0x43][..])));
    assert_eq!(tools.frozen_count(), 1);
    tools.undo(&core).expect("undo freeze change");
    assert_eq!(tools.watches().iter().find(|w| w.id == frozen_id).unwrap().freeze.as_ref().unwrap().value, vec![0x42]);
    tools.unfreeze_all(&core);
    assert_eq!(tools.frozen_count(), 0);
    println!("freeze held the value, took an edit, undid it and unfroze");

    // A watch saved with its freeze on follows the same rules as freezing it directly: no new
    // freezes during playback, and no more frozen bytes than the core takes. Paused, and frozen at
    // the bytes already there, so the game is left alone.
    core.pause();
    const CHUNK: usize = 64;
    let fill = MAX_FREEZE_BYTES / CHUNK;
    let start = tools.regions()[0].base + 0x1000;
    tools.set_viewer_window(&core, 0, Some((start, ((fill + 1) * CHUNK) as u32)));
    wait_until(&mut tools, &core, "a sample of the bytes to freeze", |t| t.read_viewer(0, 0).is_some_and(|s| s.address == start && s.valid_len as usize == (fill + 1) * CHUNK));
    let current = tools.read_viewer(0, 0).unwrap().bytes.to_vec();
    let frozen_bytes = |index: usize| Watch {
        label: format!("frozen bytes {index}"),
        address: WatchAddress::direct(start + (index * CHUNK) as u32),
        format: ValueFormat::new(ValueType::Bytes, CHUNK as u8, false),
        trace: false,
        pause_when: None,
        group: String::new(),
        freeze: Some(FreezeState { value: current[index * CHUNK..(index + 1) * CHUNK].to_vec(), active: true }),
        ..watch.clone()
    };
    tools.tick(&core, true, false, false);
    assert!(tools.upsert_watch(&core, frozen_bytes(0)).is_err(), "no new freezes during playback");
    tools.tick(&core, false, false, false);
    let limit_ids: Vec<u32> = (0..fill).map(|i| tools.upsert_watch(&core, frozen_bytes(i)).expect("a freeze within the limit")).collect();
    let error = tools.upsert_watch(&core, frozen_bytes(fill)).expect_err("a freeze past the byte limit");
    assert_eq!(tools.frozen_count(), fill);
    tools.unfreeze_all(&core);
    for id in limit_ids {
        tools.remove_watch(&core, id);
    }
    core.start();
    println!("saved freezes refused during playback and past the limit ({error})");

    // Persistence: switching games saves this list and loads the other game's (none yet).
    let count = tools.watches().len();
    let other = make_core(&second);
    tools.core_switched(&other, Some(second_watches.clone()));
    wait_until(&mut tools, &other, "the search reset", |t| !t.search_status().active);
    println!("after switching games: {:?}", tools.search_status().message);
    assert!(tools.watches().is_empty(), "the other game has no watches");
    assert!(first_watches.is_file(), "the first game's watches were saved");
    drop(core);

    let back = make_core(&first);
    tools.core_switched(&back, Some(first_watches.clone()));
    assert_eq!(tools.watches().len(), count, "watches reload with their game");
    println!("watches saved and reloaded ({count})");
    drop(other);

    println!("all checks passed");
}
