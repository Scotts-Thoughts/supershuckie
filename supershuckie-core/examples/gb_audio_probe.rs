//! Diagnostic: two raw SameBoy instances stepped by `GB_run` in lockstep, one with a sample rate
//! set, one without. Reports the first step where the cycles run or the vblank events differ.
//!
//! ```text
//! cargo run --release -p supershuckie-core --example gb_audio_probe -- <rom.gbc> [--steps n]
//! ```

use std::cell::RefCell;
use std::rc::Rc;

use safeboy::{BorderMode, Gameboy, GameboyCallbacks, Model, RtcMode, RunnableInstanceFunctions, RunningGameboy, TurboMode, VBlankType};

#[derive(Default, Clone, Debug, PartialEq)]
struct Events {
    vblanks: Vec<(u64, VBlankType)>,
    samples: u64,
}

struct Cb(Rc<RefCell<Events>>, Rc<RefCell<u64>>);

impl GameboyCallbacks for Cb {
    fn vblank(&mut self, _instance: &mut RunningGameboy, t: VBlankType) {
        let step = *self.1.borrow();
        self.0.borrow_mut().vblanks.push((step, t));
    }
    fn apu_sample(&mut self, _instance: &mut RunningGameboy, _l: i16, _r: i16) {
        self.0.borrow_mut().samples += 1;
    }
}

fn make(rom: &[u8], bios: &[u8], step: Rc<RefCell<u64>>) -> (Gameboy, Rc<RefCell<Events>>) {
    let mut gb = Gameboy::new(Model::Cgb0);
    gb.set_rtc_mode(RtcMode::Accurate);
    gb.load_boot_rom(bios);
    gb.load_rom(rom);
    gb.set_rendering_enabled(true);
    gb.set_border_mode(BorderMode::Never);
    let ev = Rc::new(RefCell::new(Events::default()));
    gb.set_callbacks(Some(Box::new(Cb(ev.clone(), step))));
    gb.reset();
    gb.set_turbo_mode(TurboMode::Enabled);
    (gb, ev)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let rom_path = args.next().expect("usage: gb_audio_probe <rom> [--steps n]");
    let mut steps: u64 = 3_000_000;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--steps" => steps = args.next().unwrap().parse().unwrap(),
            other => panic!("unexpected {other}"),
        }
    }
    let rom = std::fs::read(&rom_path).expect("rom");
    let bios = include_bytes!("../../bootrom/cgb/cgb_boot/cgb_boot_fast.bin");

    let step = Rc::new(RefCell::new(0u64));
    let (mut a, ea) = make(&rom, bios, step.clone());
    let (mut b, eb) = make(&rom, bios, step.clone());
    let boot = a.create_save_state();
    b.load_save_state(&boot).expect("load");
    let rate_a: u32 = std::env::var("RATE_A").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let rate_b: u32 = std::env::var("RATE_B").ok().and_then(|v| v.parse().ok()).unwrap_or(48000);
    a.set_sample_rate(rate_a);
    b.set_sample_rate(rate_b);
    println!("sample rates {rate_a} vs {rate_b}");

    let mut frames = 0u64;
    let mut cycles_a = 0u64;
    let mut cycles_b = 0u64;
    for s in 0..steps {
        *step.borrow_mut() = s;
        // same scripted input as gb_audio_check, by frame
        let mask = if (frames / 30) % 4 == 0 { 1 << 3 } else if (frames / 30) % 4 == 2 { 1 << 0 } else { 0 };
        a.set_input_button_mask(mask);
        b.set_input_button_mask(mask);

        let ca = a.run() as u64;
        let cb = b.run() as u64;
        cycles_a += ca;
        cycles_b += cb;

        let va = ea.borrow().vblanks.len();
        let vb = eb.borrow().vblanks.len();
        if ca != cb || va != vb {
            let ra = a.get_registers();
            let rb = b.get_registers();
            println!("step {s} (frame {frames}): cycles {ca} vs {cb} (total {cycles_a} vs {cycles_b}), vblanks {va} vs {vb}");
            println!("  pc {:#06x} vs {:#06x}, sp {:#06x} vs {:#06x}, af {:#06x} vs {:#06x}", ra.pc, rb.pc, ra.sp, rb.sp, ra.af, rb.af);
            let la: Vec<_> = ea.borrow().vblanks.iter().rev().take(3).cloned().collect();
            let lb: Vec<_> = eb.borrow().vblanks.iter().rev().take(3).cloned().collect();
            println!("  last vblanks silent {la:?}");
            println!("  last vblanks audible {lb:?}");
            // keep going a few steps to see if they re-align
            for _ in 0..8 {
                let ca = a.run() as u64;
                let cb = b.run() as u64;
                cycles_a += ca;
                cycles_b += cb;
                let ra = a.get_registers();
                let rb = b.get_registers();
                println!("  next: cycles {ca} vs {cb} (total {cycles_a} vs {cycles_b}) pc {:#06x} vs {:#06x}", ra.pc, rb.pc);
            }
            return;
        }
        frames = va as u64;
    }
    println!("{steps} steps in lockstep, {frames} frames, {} samples", eb.borrow().samples);
}
