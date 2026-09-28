//! Find where a gen 6 Pokémon game keeps its party, and check Poke-A-Byte read ranges against
//! what the 3DS core can read. Boots the game, taps A until a save is loaded, then scans the
//! process heap and the linear heap for PK6 structures whose checksum verifies.
//!
//! ```text
//! n3ds_party_probe <game.3ds> --user-dir <dir> [--frames n] [--range start-end]...
//! ```

use std::time::Instant;

use supershuckie_core::emulator::{Input, Nintendo3DS, Nintendo3DSSettings};
use supershuckie_core::{std_timestamp_provider, SuperShuckieCore};

const BLOCK_POSITION: [u8; 128] = [
    0,1,2,3, 0,1,3,2, 0,2,1,3, 0,3,1,2, 0,2,3,1, 0,3,2,1,
    1,0,2,3, 1,0,3,2, 2,0,1,3, 3,0,1,2, 2,0,3,1, 3,0,2,1,
    1,2,0,3, 1,3,0,2, 2,1,0,3, 3,1,0,2, 2,3,0,1, 3,2,0,1,
    1,2,3,0, 1,3,2,0, 2,1,3,0, 3,1,2,0, 2,3,1,0, 3,2,1,0,
    0,1,2,3, 0,1,3,2, 0,2,1,3, 0,3,1,2, 0,2,3,1, 0,3,2,1,
    1,0,2,3, 1,0,3,2,
];

/// The species of a PK6 whose checksum verifies.
fn decrypt_checks(bytes: &[u8]) -> Option<u16> {
    let ec = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
    if ec == 0 || bytes[4..6] != [0, 0] {
        return None;
    }
    let stored = u16::from_le_bytes([bytes[6], bytes[7]]);
    let mut seed = ec;
    let mut sum = 0u16;
    let mut dec = [0u16; 112];
    for (i, w) in bytes[8..232].chunks_exact(2).enumerate() {
        seed = seed.wrapping_mul(0x41C6_4E6D).wrapping_add(0x6073);
        let v = u16::from_le_bytes([w[0], w[1]]) ^ (seed >> 16) as u16;
        dec[i] = v;
        sum = sum.wrapping_add(v);
    }
    if sum != stored || stored == 0 {
        return None;
    }
    let sv = ((ec >> 13) & 31) as usize;
    let src = BLOCK_POSITION[sv * 4] as usize;
    Some(dec[src * 28])
}

fn main() {
    let mut args = std::env::args().skip(1);
    let rom_path = args.next().expect("rom");
    let mut user_dir = String::new();
    let mut frames = 3000u64;
    let mut ranges: Vec<(u32, u32)> = Vec::new();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--user-dir" => user_dir = args.next().unwrap(),
            "--frames" => frames = args.next().unwrap().parse().unwrap(),
            "--range" => {
                let r = args.next().unwrap();
                let (s, e) = r.split_once('-').unwrap();
                let p = |x: &str| u32::from_str_radix(x.trim_start_matches("0x"), 16).unwrap();
                ranges.push((p(s), p(e)));
            }
            other => panic!("unknown option {other}"),
        }
    }
    let rom = std::fs::read(&rom_path).expect("read rom");
    let n3ds = Nintendo3DS::new_from_path(&rom_path, &rom, &user_dir, std_timestamp_provider(), &Nintendo3DSSettings::default())
        .expect("load");
    let mut core = SuperShuckieCore::new(Box::new(n3ds), std_timestamp_provider());

    let t = Instant::now();
    for k in 0..frames {
        let mut i = Input::new();
        // A twice a second after boot; Start now and then for the title screen.
        i.a = k > 600 && k % 30 < 4;
        i.start = k > 600 && k % 240 < 4 && k < 1800;
        core.enqueue_input(i);
        core.run_unlocked();
    }
    println!("ran {frames} frames in {:.1} s", t.elapsed().as_secs_f64());

    let c = core.get_core();
    for base in [0x0800_0000u32, 0x1400_0000, 0x3000_0000, 0x3300_0000, 0x3400_0000, 0x3800_0000] {
        let mut b = [0u8; 4];
        println!("probe {base:#010X}: {}", if c.read_ram(base, &mut b).is_ok() { "mapped" } else { "unmapped" });
    }
    for (s, e) in &ranges {
        let mut buf = vec![0u8; (e - s) as usize];
        match c.read_ram(*s, &mut buf) {
            Ok(()) => println!("range {s:#010X}-{e:#010X}: readable, {} of {} bytes non-zero", buf.iter().filter(|b| **b != 0).count(), buf.len()),
            Err(err) => {
                // Find which part is unmapped.
                let mut first_bad = None;
                let mut page = s & !0xFFF;
                while page < *e {
                    let mut b = [0u8; 1];
                    if c.read_ram(page.max(*s), &mut b).is_err() {
                        first_bad = Some(page);
                        break;
                    }
                    page += 0x1000;
                }
                println!("range {s:#010X}-{e:#010X}: NOT readable ({err}); first unmapped page {first_bad:#010X?}");
            }
        }
    }

    // Region extents: walk pages from each base while mapped.
    let mut regions = Vec::new();
    for base in [0x0800_0000u32, 0x1400_0000, 0x3000_0000] {
        let mut end = base;
        let mut b = [0u8; 1];
        while end < base + 0x1000_0000 && c.read_ram(end, &mut b).is_ok() {
            end += 0x1000;
        }
        if end > base {
            println!("mapped run {base:#010X}-{end:#010X} ({} MB)", (end - base) >> 20);
            regions.push((base, end));
        }
    }

    for (base, end) in regions {
        let mut data = vec![0u8; (end - base) as usize];
        c.read_ram(base, &mut data).unwrap();
        let mut hits = Vec::new();
        let mut off = 0usize;
        while off + 232 <= data.len() {
            if let Some(first) = decrypt_checks(&data[off..off + 232]) {
                hits.push((base + off as u32, first));
            }
            off += 4;
        }
        println!("{} PK6 structures in {base:#010X}-{end:#010X}", hits.len());
        // The trainer block: TID, SID and, 0x48 further on, the OT name of a caught Pokémon.
        if let Some((addr, _)) = hits.iter().find(|(_, s)| (1..=721).contains(s)) {
            let off = (addr - base) as usize;
            let ec = u32::from_le_bytes(data[off..off + 4].try_into().unwrap());
            let mut seed = ec;
            let mut dec = [0u8; 224];
            for k in (0..224).step_by(2) {
                seed = seed.wrapping_mul(0x41C6_4E6D).wrapping_add(0x6073);
                let v = u16::from_le_bytes([data[off + 8 + k], data[off + 9 + k]]) ^ (seed >> 16) as u16;
                dec[k..k + 2].copy_from_slice(&v.to_le_bytes());
            }
            let sv = ((ec >> 13) & 31) as usize;
            let block = |pos: usize| { let s = BLOCK_POSITION[sv * 4 + pos] as usize * 56; &dec[s..s + 56] };
            let a = block(0);
            let d = block(3);
            let tid_sid = &a[4..8];
            // Block A starts at 0x08 (TID 0x0C, SID 0x0E); block D at 0xB0 is the OT name.
            let ot = &d[..12];
            println!("  OT of {addr:#010X}: tid {} sid {} name {:?}", u16::from_le_bytes([a[4], a[5]]), u16::from_le_bytes([a[6], a[7]]),
                     String::from_utf16_lossy(&ot.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|c| *c != 0).collect::<Vec<_>>()));
            for i in (0..data.len().saturating_sub(0x48 + 12)).step_by(2) {
                if &data[i..i + 4] == tid_sid {
                    let name_here = &data[i + 0x48..i + 0x48 + 12] == ot;
                    println!("  tid/sid at {:#010X}{}", base + i as u32, if name_here { " (OT name at +0x48)" } else { "" });
                }
            }
        }
        for (addr, species) in hits.iter().take(80) {
            let run = |stride: u32| (1..6).take_while(|k| hits.iter().any(|(a, _)| *a == addr + k * stride)).count();
            println!("  {addr:#010X} species {species} (slots following at stride 484: {}, at 260: {})", run(484), run(260));
            // Party stats (level, HP) decrypted with a fresh EC seed, at the save layout's 0xE8
            // and the live layout's 0x158.
            let off = (addr - base) as usize;
            let ec = u32::from_le_bytes(data[off..off + 4].try_into().unwrap());
            for (name, at) in [("save@0xE8", 0xE8usize), ("live@0x158", 0x158)] {
                if off + at + 28 > data.len() {
                    continue;
                }
                let mut seed = ec;
                let mut s = [0u8; 28];
                for k in (0..28).step_by(2) {
                    seed = seed.wrapping_mul(0x41C6_4E6D).wrapping_add(0x6073);
                    let v = u16::from_le_bytes([data[off + at + k], data[off + at + k + 1]]) ^ (seed >> 16) as u16;
                    s[k..k + 2].copy_from_slice(&v.to_le_bytes());
                }
                println!("    {name}: level {} hp {}/{}", s[4], u16::from_le_bytes([s[8], s[9]]), u16::from_le_bytes([s[10], s[11]]));
            }
        }
    }
}
