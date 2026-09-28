//! Play a real Nintendo 3DS recording frame by frame, every frame drawn (as the app does at any
//! speed), and measure where the frame time goes: per-frame wall time split into emulating to
//! the VBlank, taking the picture out of the renderer and converting it, the slowest one-second
//! windows at a given speed, and optionally a stack-sampling profile of the whole run.
//!
//! ```text
//! n3ds_perf_lab <game file> <file.replay> [--user-dir dir] [--start F] [--frames N]
//!               [--speed 4] [--record] [--profile out.txt] [--csv frames.csv] [--worst 10]
//! ```
//! `--paced` runs frames the way the app does at `--speed` (`SuperShuckieCore::run` with its
//! presentation choices, e.g. one picture in four at 4x) against a clock that is always due,
//! instead of `run_unlocked`, which hands out every picture.
//! `--record` also takes a keyframe every 240 frames into two alternating recycled buffers and
//! turns on the ROM read log and VRAM access tracking, as a 3DS recording does.
//! `--hashes out.txt` writes, every 240 frames, a hash of the game's heap, its linear heap and
//! both screens (for checking that a change leaves emulation and pictures exactly as they were).
//! `--profile` samples the emulation thread's stack about once a millisecond and writes
//! `frame addr addr ...` lines plus the module table; `scripts/n3ds-perf-report.py` turns that
//! into per-function self and inclusive times (optionally for chosen frame ranges only).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use supershuckie_core::emulator::{Nintendo3DS, Nintendo3DSSettings};
use supershuckie_core::{std_timestamp_provider, MonotonicTimestampProvider, SuperShuckieCore};
use supershuckie_replay_recorder::Speed;

/// A clock one second further on at every reading: a paced core always finds its next frame due.
struct AlwaysDue(u64);

impl MonotonicTimestampProvider for AlwaysDue {
    fn get_timestamp_microseconds(&mut self) -> u64 {
        self.0 += 1_000_000;
        self.0
    }
}
use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::replay_file::RomBytes;

fn map(path: &str) -> RomBytes {
    let file = std::fs::File::open(path).unwrap_or_else(|e| panic!("open {path}: {e}"));
    // SAFETY: read-only mapping of a file nobody writes while this runs.
    Arc::new(unsafe { memmap2::Mmap::map(&file) }.expect("map"))
}

#[cfg(windows)]
mod sampler {
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;

    pub const MAX_DEPTH: usize = 64;

    /// x64 `CONTEXT` (1232 bytes, 16-aligned); only the fields named below are touched here.
    #[repr(C, align(16))]
    struct Context([u8; 1232]);

    impl Context {
        fn u64_at(&self, offset: usize) -> u64 {
            u64::from_le_bytes(self.0[offset..offset + 8].try_into().unwrap())
        }
        fn rip(&self) -> u64 { self.u64_at(0xF8) }
        fn rsp(&self) -> u64 { self.u64_at(0x98) }
        fn set_rip(&mut self, v: u64) { self.0[0xF8..0x100].copy_from_slice(&v.to_le_bytes()) }
        fn set_rsp(&mut self, v: u64) { self.0[0x98..0xA0].copy_from_slice(&v.to_le_bytes()) }
    }

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct ModuleInfo {
        base: usize,
        size: u32,
        entry: usize,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThread() -> isize;
        fn GetCurrentProcess() -> isize;
        fn DuplicateHandle(src_process: isize, src: isize, dst_process: isize, dst: *mut isize, access: u32, inherit: i32, options: u32) -> i32;
        fn SuspendThread(thread: isize) -> u32;
        fn ResumeThread(thread: isize) -> u32;
        fn GetThreadContext(thread: isize, context: *mut Context) -> i32;
        fn RtlLookupFunctionEntry(pc: u64, image_base: *mut u64, history: *mut c_void) -> *mut c_void;
        fn RtlVirtualUnwind(handler_type: u32, image_base: u64, pc: u64, entry: *mut c_void, context: *mut Context, handler_data: *mut *mut c_void, establisher: *mut u64, pointers: *mut c_void) -> *mut c_void;
        fn ReadProcessMemory(process: isize, address: *const c_void, buffer: *mut c_void, size: usize, read: *mut usize) -> i32;
        fn K32EnumProcessModules(process: isize, modules: *mut isize, cb: u32, needed: *mut u32) -> i32;
        fn K32GetModuleInformation(process: isize, module: isize, info: *mut ModuleInfo, cb: u32) -> i32;
        fn K32GetModuleFileNameExA(process: isize, module: isize, name: *mut u8, size: u32) -> u32;
    }
    #[link(name = "winmm")]
    unsafe extern "system" {
        fn timeBeginPeriod(period: u32) -> u32;
    }

    const CONTEXT_AMD64: u32 = 0x0010_0000;
    const CONTEXT_CONTROL: u32 = CONTEXT_AMD64 | 1;
    const CONTEXT_INTEGER: u32 = CONTEXT_AMD64 | 2;

    pub struct Samples {
        pub stacks: Vec<(u64, Vec<u64>)>,
        pub modules: Vec<(u64, u64, String)>,
    }

    /// A handle to the calling thread that another thread can suspend.
    pub fn current_thread() -> isize {
        let mut handle = 0isize;
        unsafe {
            DuplicateHandle(GetCurrentProcess(), GetCurrentThread(), GetCurrentProcess(), &mut handle, 0, 0, 2);
        }
        handle
    }

    pub fn modules() -> Vec<(u64, u64, String)> {
        let mut handles = vec![0isize; 1024];
        let mut needed = 0u32;
        let mut out = Vec::new();
        unsafe {
            let process = GetCurrentProcess();
            if K32EnumProcessModules(process, handles.as_mut_ptr(), (handles.len() * 8) as u32, &mut needed) == 0 {
                return out;
            }
            for &m in &handles[..(needed as usize / 8).min(handles.len())] {
                let mut info = ModuleInfo::default();
                K32GetModuleInformation(process, m, &mut info, core::mem::size_of::<ModuleInfo>() as u32);
                let mut name = [0u8; 512];
                let n = K32GetModuleFileNameExA(process, m, name.as_mut_ptr(), name.len() as u32) as usize;
                out.push((info.base as u64, info.base as u64 + info.size as u64, String::from_utf8_lossy(&name[..n]).into_owned()));
            }
        }
        out
    }

    /// Sample `thread`'s stack every millisecond until `stop`, tagging each with `frame`.
    pub fn run(thread: isize, frame: Arc<AtomicU64>, stop: Arc<AtomicBool>) -> Samples {
        unsafe { timeBeginPeriod(1) };
        let modules = modules();
        let in_module = |pc: u64| modules.iter().any(|(start, end, _)| pc >= *start && pc < *end);
        let mut stacks = Vec::with_capacity(1 << 20);
        let mut ctx = Box::new(Context([0; 1232]));
        let mut buf = [0u64; MAX_DEPTH];
        let process = unsafe { GetCurrentProcess() };
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(1));
            let f = frame.load(Ordering::Relaxed);
            let mut depth = 0usize;
            // Nothing between suspend and resume may allocate: the thread may hold the heap lock.
            unsafe {
                if SuspendThread(thread) == u32::MAX {
                    continue;
                }
                ctx.0.fill(0);
                ctx.0[0x30..0x34].copy_from_slice(&(CONTEXT_CONTROL | CONTEXT_INTEGER).to_le_bytes());
                if GetThreadContext(thread, &mut *ctx) != 0 {
                    while depth < MAX_DEPTH {
                        let pc = ctx.rip();
                        if pc == 0 {
                            break;
                        }
                        buf[depth] = pc;
                        depth += 1;
                        let mut base = 0u64;
                        let entry = RtlLookupFunctionEntry(pc, &mut base, core::ptr::null_mut());
                        if entry.is_null() {
                            // Generated code has no unwind data: the stack above it is not walkable.
                            if !in_module(pc) {
                                break;
                            }
                            // A leaf function: the return address is on top of the stack.
                            let rsp = ctx.rsp();
                            let mut ret = 0u64;
                            let mut read = 0usize;
                            if ReadProcessMemory(process, rsp as *const c_void, (&mut ret) as *mut u64 as *mut c_void, 8, &mut read) == 0 {
                                break;
                            }
                            ctx.set_rip(ret);
                            ctx.set_rsp(rsp + 8);
                        } else {
                            let mut handler_data = core::ptr::null_mut();
                            let mut establisher = 0u64;
                            RtlVirtualUnwind(0, base, pc, entry, &mut *ctx, &mut handler_data, &mut establisher, core::ptr::null_mut());
                        }
                    }
                }
                ResumeThread(thread);
            }
            if depth > 0 {
                stacks.push((f, buf[..depth].to_vec()));
            }
        }
        Samples { stacks, modules: self::modules() }
    }
}

fn fnv_bytes(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for chunk in bytes.chunks(8) {
        let mut w = [0u8; 8];
        w[..chunk.len()].copy_from_slice(chunk);
        h = (h ^ u64::from_le_bytes(w)).wrapping_mul(0x100000001b3);
    }
    h
}

fn bytemuck_u32(pixels: &[u32]) -> &[u8] {
    // SAFETY: u32 has no padding and any byte pattern is a valid u8.
    unsafe { core::slice::from_raw_parts(pixels.as_ptr() as *const u8, pixels.len() * 4) }
}

fn n_frames<T>(rows: &[T]) -> f64 {
    rows.len().max(1) as f64
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn main() {
    let mut args = std::env::args().skip(1);
    let game_path = args.next().expect("usage: n3ds_perf_lab <game> <replay> ...");
    let replay_path = args.next().expect("replay");
    let mut user_dir = std::env::temp_dir().join("supershuckie-n3ds-perf-lab").join("user");
    let (mut start, mut frames, mut speed, mut worst) = (0u64, 0u64, 4.0f64, 10usize);
    let (mut record, mut profile, mut csv, mut hashes) = (false, None::<String>, None::<String>, None::<String>);
    let mut paced = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--user-dir" => user_dir = args.next().unwrap().into(),
            "--start" => start = args.next().unwrap().parse().unwrap(),
            "--frames" => frames = args.next().unwrap().parse().unwrap(),
            "--speed" => speed = args.next().unwrap().parse().unwrap(),
            "--worst" => worst = args.next().unwrap().parse().unwrap(),
            "--record" => record = true,
            "--paced" => paced = true,
            "--profile" => profile = args.next(),
            "--csv" => csv = args.next(),
            "--hashes" => hashes = args.next(),
            o => panic!("unknown option {o}")
        }
    }
    std::fs::create_dir_all(&user_dir).unwrap();

    let rom = map(&game_path);
    let t = Instant::now();
    let clock: Box<dyn MonotonicTimestampProvider> = if paced { Box::new(AlwaysDue(0)) } else { std_timestamp_provider() };
    let n3ds = Nintendo3DS::new_from_path(&game_path, &[], user_dir.to_str().unwrap(), clock, &Nintendo3DSSettings::default()).expect("load game");
    let mut n3ds = n3ds;
    println!("OpenGL: {}", n3ds.gl_renderer());
    let mut core = SuperShuckieCore::new(Box::new(n3ds), std_timestamp_provider());
    println!("core made in {:.0} ms", t.elapsed().as_secs_f64() * 1000.0);

    let source_bytes: Arc<dyn AsRef<[u8]> + Send + Sync> = map(&replay_path);
    let mut player = ReplayFilePlayer::new_shared(source_bytes, false).expect("parse");
    player.set_rom(rom.clone());
    let total = player.get_total_frames();
    println!("replay: version {}, {total} frames", player.get_replay_version());
    core.attach_replay_player(player, true).expect("attach");
    core.set_ignore_speed_changes_in_replays(true);
    core.set_speed(Speed::from_multiplier_float(speed));
    if start > 0 {
        start = core.go_to_replay_keyframe(start).expect("keyframe at the start");
        println!("starting at keyframe {start}");
    }
    if frames == 0 {
        frames = total.saturating_sub(start);
    }
    let end = (start + frames).min(total.saturating_sub(1));
    if record {
        core.get_core_mut().set_rom_read_log(true);
        core.get_core_mut().set_transient_page_tracking(true);
    }

    let frame_now = Arc::new(AtomicU64::new(start));
    let stop = Arc::new(AtomicBool::new(false));
    #[cfg(windows)]
    let sampler = profile.as_ref().map(|_| {
        let thread = sampler::current_thread();
        let (f, s) = (frame_now.clone(), stop.clone());
        std::thread::spawn(move || sampler::run(thread, f, s))
    });

    // Per frame: wall, emulate, readback, convert (ms), and whether a keyframe was taken.
    let mut rows: Vec<(u64, f64, f64, f64, f64, bool)> = Vec::with_capacity(frames as usize);
    let mut buffers: [Vec<u8>; 2] = [Vec::new(), Vec::new()];
    let mut next_buffer = 0usize;
    let (mut reads, mut access) = (Vec::new(), Vec::new());
    // Texture cache counters (azahar_rs::Core::cache_stats) after each frame.
    let mut stats: Vec<[u64; 9]> = Vec::with_capacity(frames as usize);
    let stats_start = azahar_rs::Core::cache_stats();
    let vram_start = core.get_core_mut().as_any_mut().downcast_mut::<Nintendo3DS>().unwrap().gpu_memory_available_kb();
    let mut hash_lines: Vec<String> = Vec::new();
    let t_all = Instant::now();
    while core.total_frames() < end && !core.is_replay_stalled() {
        let f = core.total_frames();
        frame_now.store(f, Ordering::Relaxed);
        let t = Instant::now();
        if paced {
            core.run();
        } else {
            core.run_unlocked();
        }
        let mut keyframe = false;
        if record {
            core.get_core_mut().take_rom_reads(&mut reads);
            reads.clear();
            if (f + 1 - start) % 240 == 0 {
                let _ = core.get_core_mut().take_transient_page_access(&mut access);
                core.get_core().create_save_state_into(&mut buffers[next_buffer]);
                next_buffer ^= 1;
                keyframe = true;
            }
        }
        let wall = t.elapsed().as_secs_f64() * 1000.0;
        let n3ds = core.get_core_mut().as_any_mut().downcast_mut::<Nintendo3DS>().unwrap();
        let [emu, readback, convert] = n3ds.frame_timing().map(|ns| ns as f64 / 1e6);
        rows.push((f, wall, emu, readback, convert, keyframe));
        if hashes.is_some() && (f + 1 - start) % 240 == 0 {
            let c = core.get_core();
            let screens = c.get_screens();
            hash_lines.push(format!("{} heap {:016x} linear {:016x} top {:016x} bottom {:016x}", f + 1,
                fnv_bytes(c.memory_region_data(0).unwrap_or(&[])), fnv_bytes(c.memory_region_data(1).unwrap_or(&[])),
                fnv_bytes(bytemuck_u32(&screens[0].pixels)), fnv_bytes(bytemuck_u32(&screens[1].pixels))));
        }
        stats.push(azahar_rs::Core::cache_stats());
    }
    let elapsed = t_all.elapsed().as_secs_f64();
    let vram_end = core.get_core_mut().as_any_mut().downcast_mut::<Nintendo3DS>().unwrap().gpu_memory_available_kb();
    stop.store(true, Ordering::Relaxed);

    let n = rows.len().max(1) as f64;
    let budget = 1000.0 / (59.8261 * speed);
    println!("\n{} frames in {:.1} s = {:.1} fps average ({:.2}x); budget at {speed}x = {budget:.2} ms/frame",
        rows.len(), elapsed, rows.len() as f64 / elapsed, rows.len() as f64 / elapsed / 59.8261);
    let mut walls: Vec<f64> = rows.iter().map(|r| r.1).collect();
    walls.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean = |i: usize| rows.iter().map(|r| [r.1, r.2, r.3, r.4][i]).sum::<f64>() / n;
    println!("mean ms: wall {:.3} = emulate {:.3} + readback {:.3} + convert {:.3} + rest {:.3}",
        mean(0), mean(1), mean(2), mean(3), mean(0) - mean(1) - mean(2) - mean(3));
    println!("wall ms percentiles: p50 {:.2}  p90 {:.2}  p99 {:.2}  p99.9 {:.2}  max {:.2}",
        percentile(&walls, 0.5), percentile(&walls, 0.9), percentile(&walls, 0.99), percentile(&walls, 0.999), walls.last().copied().unwrap_or(0.0));
    let over = rows.iter().filter(|r| r.1 > budget).count();
    println!("frames over budget: {over} ({:.1}%)", over as f64 * 100.0 / n);

    // One-second windows at this speed, as the app would feel them: frames it managed per second
    // if each window's frames ran back to back (frames over budget can be caught up by the rest).
    let window = (59.8261 * speed).round() as usize;
    let mut windows: Vec<(u64, f64, f64, f64, f64)> = rows.chunks(window).filter(|c| c.len() == window).map(|c| {
        let ms: f64 = c.iter().map(|r| r.1).sum();
        let emu: f64 = c.iter().map(|r| r.2).sum();
        let rb: f64 = c.iter().map(|r| r.3 + r.4).sum();
        let worst = c.iter().map(|r| r.1).fold(0.0, f64::max);
        (c[0].0, window as f64 * 1000.0 / ms, emu / window as f64, rb / window as f64, worst)
    }).collect();
    let slow = windows.iter().filter(|w| w.1 < 59.8261 * speed).count();
    let names = ["surfaces created", "recycled", "unregistered", "framebuffers created", "uploads", "downloads", "cpu invalidations", "draws", "destroyed"];
    if let Some(last) = stats.last() {
        let per_frame: Vec<String> = names.iter().enumerate().map(|(i, n)| format!("{n} {:.2}", (last[i] - stats_start[i]) as f64 / n_frames(&rows))).collect();
        println!("texture cache per frame: {}", per_frame.join(", "));
        println!("surfaces alive at the end (created - destroyed since start): {}; video memory available {} MB -> {} MB",
            (last[0] - stats_start[0]) as i64 - (last[8] - stats_start[8]) as i64, vram_start / 1024, vram_end / 1024);
    }
    println!("{}-frame windows: {} of {} below {speed}x", window, slow, windows.len());
    windows.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
    println!("slowest windows (start frame, fps, emulate ms/frame, picture ms/frame, worst frame ms, new surfaces/framebuffers/downloads/cpu invalidations per frame):");
    for w in windows.iter().take(worst) {
        let i = rows.iter().position(|r| r.0 == w.0).unwrap();
        let j = (i + window - 1).min(stats.len() - 1);
        let before = if i == 0 { stats_start } else { stats[i - 1] };
        let d = |k: usize| (stats[j][k] - before[k]) as f64 / window as f64;
        println!("  {:>8}  {:6.1} fps  emu {:.2}  pic {:.2}  worst {:5.1}  surf {:.2} fb {:.2} dl {:.2} inv {:.2} draws {:.0}",
            w.0, w.1, w.2, w.3, w.4, d(0), d(3), d(5), d(6), d(7));
    }
    let keyframe_rows: Vec<f64> = rows.iter().filter(|r| r.5).map(|r| r.1).collect();
    if !keyframe_rows.is_empty() {
        println!("keyframe frames: {} avg {:.2} ms max {:.2} ms", keyframe_rows.len(),
            keyframe_rows.iter().sum::<f64>() / keyframe_rows.len() as f64, keyframe_rows.iter().fold(0.0, |a: f64, b| a.max(*b)));
    }

    if let Some(path) = hashes {
        std::fs::write(&path, hash_lines.join("
") + "
").unwrap();
        println!("wrote {} checkpoints to {path}, digest {:016x}", hash_lines.len(), fnv_bytes(hash_lines.join("
").as_bytes()));
    }

    if let Some(path) = csv {
        use std::io::Write;
        let mut out = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
        writeln!(out, "frame,wall_ms,emulate_ms,readback_ms,convert_ms,keyframe").unwrap();
        for r in &rows {
            writeln!(out, "{},{:.4},{:.4},{:.4},{:.4},{}", r.0, r.1, r.2, r.3, r.4, r.5 as u8).unwrap();
        }
        println!("wrote {path}");
    }

    #[cfg(windows)]
    if let (Some(path), Some(handle)) = (profile, sampler) {
        use std::io::Write;
        let samples = handle.join().unwrap();
        let mut out = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
        for (start, end, name) in &samples.modules {
            writeln!(out, "module {start:x} {end:x} {name}").unwrap();
        }
        for (frame, stack) in &samples.stacks {
            write!(out, "{frame}").unwrap();
            for pc in stack {
                write!(out, " {pc:x}").unwrap();
            }
            writeln!(out).unwrap();
        }
        println!("wrote {} samples to {path}", samples.stacks.len());
    }
}
