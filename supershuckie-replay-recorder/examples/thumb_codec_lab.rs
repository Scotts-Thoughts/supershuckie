//! What a lossy codec would make of the timeline pictures: JPEG at several qualities against the
//! zstd-against-previous encoding of format v9, on the pictures `n3ds_breakdown --dump-thumbs`
//! wrote (RGB565 `tNNNNN-top.rgb565` 200x120 and `-bottom.rgb565` 160x120). Research tool.
//!
//! ```text
//! thumb_codec_lab <dump dir>
//! ```

use std::time::Instant;

use supershuckie_replay_recorder::{compress_data, compress_small_data_with_prefix};

fn rgb565_to_rgb8(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() / 2 * 3);
    for px in raw.chunks_exact(2) {
        let p = u16::from_le_bytes([px[0], px[1]]);
        let r = ((p >> 11) & 31) as u8;
        let g = ((p >> 5) & 63) as u8;
        let b = (p & 31) as u8;
        out.push((r << 3) | (r >> 2));
        out.push((g << 2) | (g >> 4));
        out.push((b << 3) | (b >> 2));
    }
    out
}

fn main() {
    let dir = std::env::args().nth(1).expect("usage: thumb_codec_lab <dump dir>");
    let mut names: Vec<String> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with("-top.rgb565")).collect();
    names.sort();
    let mut tops = Vec::new();
    let mut bottoms = Vec::new();
    for n in &names {
        tops.push(std::fs::read(format!("{dir}/{n}")).unwrap());
        bottoms.push(std::fs::read(format!("{dir}/{}", n.replace("-top", "-bottom"))).unwrap());
    }
    println!("{} pairs", tops.len());
    let (mut zstd_alone, mut zstd_prev) = (0usize, 0usize);
    for list in [&tops, &bottoms] {
        for i in 0..list.len() {
            zstd_alone += compress_data(&list[i], 3).unwrap().len();
            zstd_prev += if i == 0 { compress_data(&list[i], 3).unwrap().len() } else { compress_small_data_with_prefix(&list[i], 3, &list[i - 1]).unwrap().len() };
        }
    }
    let n = tops.len();
    println!("zstd3 alone: {:.1} KB/pair; zstd3 against previous (v9, but these samples are 60+ s apart): {:.1} KB/pair", zstd_alone as f64 / n as f64 / 1e3, zstd_prev as f64 / n as f64 / 1e3);
    for quality in [50u8, 65, 75, 85, 92] {
        let (mut total, mut top_total, mut enc_ms, mut dec_ms) = (0usize, 0usize, 0f64, 0f64);
        for (list, w, h) in [(&tops, 200u16, 120u16), (&bottoms, 160u16, 120u16)] {
            for raw in list.iter() {
                let rgb = rgb565_to_rgb8(raw);
                let t = Instant::now();
                let mut out = Vec::new();
                let encoder = jpeg_encoder::Encoder::new(&mut out, quality);
                encoder.encode(&rgb, w, h, jpeg_encoder::ColorType::Rgb).unwrap();
                enc_ms += t.elapsed().as_secs_f64() * 1000.0;
                total += out.len();
                if w == 200 { top_total += out.len(); }
                let t = Instant::now();
                let mut decoder = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(out.as_slice()));
                let pixels = decoder.decode().unwrap();
                dec_ms += t.elapsed().as_secs_f64() * 1000.0;
                assert_eq!(pixels.len(), rgb.len());
            }
        }
        println!("jpeg q{quality}: {:.1} KB/pair (top {:.1} KB), encode {:.2} ms, decode {:.2} ms per picture", total as f64 / n as f64 / 1e3, top_total as f64 / n as f64 / 1e3, enc_ms / (2 * n) as f64, dec_ms / (2 * n) as f64);
    }
}
