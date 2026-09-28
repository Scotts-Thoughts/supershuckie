//! Check the 3DS date setting (Azahar patch 0008) headlessly: boot a game with one date, read the
//! clock the game sees from the shared page, save a state, then load that state into a core set
//! to another date and check the clock still follows the state.
//!
//! ```text
//! n3ds_clock_check <game file> [--user-dir dir] [--frames N] [--hour]
//! ```
//!
//! `--hour` runs an emulated hour after the load (~5 minutes), until the kernel's hourly clock
//! update, which computes the time from the clock's start: the part a state must carry.
//!
//! The shared page (0x1FF81000) holds two date/time records the kernel rewrites on boot and
//! hourly: milliseconds since 1900-01-01 on the console's clock, and the tick they were taken at.

use azahar_rs::{Core, Settings};
use std::ffi::CString;

const SHARED_PAGE: u32 = 0x1FF8_1000;
/// 1900-01-01 to 1970-01-01, in milliseconds.
const MS_1900_TO_1970: u64 = 2_208_988_800_000;

fn describe(ms_since_1900: u64) -> String {
    let unix = (ms_since_1900.saturating_sub(MS_1900_TO_1970) / 1000) as i64;
    let (days, secs) = (unix.div_euclid(86_400), unix.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

/// (update counter, the record written last) from the shared page.
fn clock(core: &Core) -> (u32, u64) {
    let mut page = [0u8; 0x60];
    assert!(core.read_memory(SHARED_PAGE, &mut page), "shared page not mapped");
    let counter = u32::from_le_bytes(page[0..4].try_into().unwrap());
    let record = |offset: usize| u64::from_le_bytes(page[offset..offset + 8].try_into().unwrap());
    // The kernel writes record 0 on odd counts and record 1 on even ones, then increments.
    let latest = if counter.wrapping_sub(1) % 2 == 1 { record(0x20) } else { record(0x40) };
    (counter, latest)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let game = args.next().expect("usage: n3ds_clock_check <game> [--user-dir dir] [--frames N]");
    let mut user_dir = std::env::temp_dir().join("supershuckie-n3ds-clock").join("user");
    let mut frames = 120u32;
    let mut hour = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--user-dir" => user_dir = args.next().unwrap().into(),
            "--frames" => frames = args.next().unwrap().parse().unwrap(),
            "--hour" => hour = true,
            o => panic!("unknown option {o}")
        }
    }
    std::fs::create_dir_all(&user_dir).unwrap();
    let game = CString::new(game).unwrap();
    let dir = CString::new(user_dir.to_str().unwrap()).unwrap();

    let first = Settings { init_time: Settings::init_time_for(2026, 9, 26, 14, 30, 0), language: 2, ..Settings::default() };
    let second = Settings { init_time: Settings::init_time_for(2001, 2, 3, 4, 5, 6), language: 1, ..Settings::default() };
    let expected_first = (first.init_time - 946_684_800) * 1000 + 3_155_673_600_000;

    let mut failures = 0;
    let state = {
        let mut core = Core::new(&game, &dir, &first).expect("load game");
        for _ in 0..frames {
            core.run_frame(true);
        }
        let (counter, latest) = clock(&core);
        println!("booted at 2026-09-26 14:30:00: counter {counter}, clock {} ({latest} ms since 1900)", describe(latest));
        if latest.abs_diff(expected_first) > 5_000 {
            println!("  FAIL: expected about {}", describe(expected_first));
            failures += 1;
        }
        core.save_state().expect("save state")
    };

    let mut core = Core::new(&game, &dir, &second).expect("load game again");
    for _ in 0..10 {
        core.run_frame(true);
    }
    let (counter, latest) = clock(&core);
    println!("booted at 2001-02-03 04:05:06: counter {counter}, clock {}", describe(latest));
    let expected_second = (second.init_time - 946_684_800) * 1000 + 3_155_673_600_000;
    if latest.abs_diff(expected_second) > 5_000 {
        println!("  FAIL: expected about {}", describe(expected_second));
        failures += 1;
    }

    assert!(core.load_state(&state), "load state");
    let (counter_loaded, latest_loaded) = clock(&core);
    println!("state loaded: counter {counter_loaded}, clock {}", describe(latest_loaded));
    for _ in 0..frames {
        core.run_frame(true);
    }
    let (counter_after, latest_after) = clock(&core);
    println!("after {frames} frames: counter {counter_after}, clock {}", describe(latest_after));
    if latest_after.abs_diff(expected_first) > 10_000 {
        println!("  FAIL: the state's clock (about {}) did not survive the load", describe(expected_first));
        failures += 1;
    }
    if counter_after == counter_loaded && !hour {
        println!("  note: no clock update ran after the load, so this only shows the saved page (--hour runs until one does)");
    }

    if hour {
        // 59.8261 frames a second; stop at the first update after the load.
        let limit = 3600 * 60 + 600;
        let mut ran = 0u32;
        while ran < limit && clock(&core).0 == counter_after {
            core.run_frame(true);
            ran += 1;
        }
        let (counter_hour, latest_hour) = clock(&core);
        println!("after {ran} more frames: counter {counter_hour}, clock {}", describe(latest_hour));
        let expected_hour = expected_first + u64::from(2 * frames + ran) * 4_481_136 * 1000 / 268_111_856;
        if counter_hour == counter_after {
            println!("  FAIL: no hourly update ran");
            failures += 1;
        } else if latest_hour.abs_diff(expected_hour) > 60_000 {
            println!("  FAIL: expected about {} (the state's clock start), not the second core's", describe(expected_hour));
            failures += 1;
        }
    }

    if failures == 0 {
        println!("OK");
    } else {
        println!("{failures} FAILURE(S)");
        std::process::exit(1);
    }
}
