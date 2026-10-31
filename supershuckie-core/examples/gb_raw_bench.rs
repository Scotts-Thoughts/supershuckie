//! Diagnostic: how fast SameBoy alone runs a Game Boy ROM (one `GB_run` per loop pass, turbo, no
//! Super Shuckie code in between), and how many `GB_run` calls a frame takes. Compare with the
//! core thread's CPU time per frame (`supershuckie-frontend`'s `gb_frame_drop_probe`) to see
//! what the per-step loop around it costs.
//!
//! ```text
//! cargo run --release -p supershuckie-core --example gb_raw_bench -- <rom> [--frames n]
//! ```

use std::cell::Cell;
use std::rc::Rc;
use std::time::Instant;

use safeboy::{BorderMode, Gameboy, GameboyCallbacks, Model, RtcMode, RunnableInstanceFunctions, RunningGameboy, TurboMode, VBlankType};

struct Cb(Rc<Cell<u64>>);

impl GameboyCallbacks for Cb {
    fn vblank(&mut self, _instance: &mut RunningGameboy, _t: VBlankType) {
        self.0.set(self.0.get() + 1);
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let rom_path = args.next().expect("usage: gb_raw_bench <rom> [--frames n]");
    let mut frames: u64 = 20_000;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--frames" => frames = args.next().unwrap().parse().unwrap(),
            other => panic!("unexpected {other}"),
        }
    }
    let rom = std::fs::read(&rom_path).expect("rom");
    let bios = include_bytes!("../../bootrom/cgb/cgb_boot/cgb_boot_fast.bin");

    let vblanks = Rc::new(Cell::new(0u64));
    let mut gb = Gameboy::new(Model::Cgb0);
    gb.set_rtc_mode(RtcMode::Accurate);
    gb.load_boot_rom(bios);
    gb.load_rom(&rom);
    gb.set_rendering_enabled(true);
    gb.set_border_mode(BorderMode::Never);
    gb.set_callbacks(Some(Box::new(Cb(vblanks.clone()))));
    gb.reset();
    gb.set_turbo_mode(TurboMode::Enabled);

    // Past the boot ROM and the title screen's first frames.
    while vblanks.get() < 600 {
        gb.run();
    }

    let start_frames = vblanks.get();
    let mut steps = 0u64;
    let started = Instant::now();
    while vblanks.get() - start_frames < frames {
        gb.run();
        steps += 1;
    }
    let seconds = started.elapsed().as_secs_f64();
    println!(
        "{frames} frames in {seconds:.2} s: {:.0} fps uncapped, {:.3} ms/frame, {:.0} GB_run calls per frame, {:.1} ns per GB_run",
        frames as f64 / seconds, seconds * 1000.0 / frames as f64, steps as f64 / frames as f64, seconds * 1e9 / steps as f64
    );
}
