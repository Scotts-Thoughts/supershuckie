//! Timeline pictures of Nintendo 3DS files as JPEG (format v10, [`Packet::Thumbnail`] with
//! `jpeg`): a 200x120 screen is about 5 KB at quality 75 instead of 13 KB as lossless RGB565
//! against the previous picture, and each picture decodes on its own in well under a
//! millisecond (`thumb_codec_lab`). The pictures are only ever shown while the timeline is
//! dragged, so the loss does not matter. Needs the `std` feature; without it nothing encodes and
//! such a picture does not decode.
//!
//! [`Packet::Thumbnail`]: crate::Packet::Thumbnail

use alloc::borrow::Cow;
use alloc::vec::Vec;

/// RGB565 (little-endian) pixels of `width` x `height` as a baseline JPEG at `quality` (1-100).
pub fn encode_rgb565(width: u32, height: u32, pixels: &[u8], quality: u8) -> Result<Vec<u8>, Cow<'static, str>> {
    let (Ok(w), Ok(h)) = (u16::try_from(width), u16::try_from(height)) else {
        return Err(Cow::Borrowed("picture too large for a JPEG"));
    };
    if pixels.len() != width as usize * height as usize * 2 {
        return Err(Cow::Borrowed("picture size does not match its dimensions"));
    }
    #[cfg(feature = "std")]
    {
        let mut rgb = Vec::with_capacity(pixels.len() / 2 * 3);
        for px in pixels.chunks_exact(2) {
            let p = u16::from_le_bytes([px[0], px[1]]);
            let (r, g, b) = (((p >> 11) & 31) as u8, ((p >> 5) & 63) as u8, (p & 31) as u8);
            rgb.extend_from_slice(&[(r << 3) | (r >> 2), (g << 2) | (g >> 4), (b << 3) | (b >> 2)]);
        }
        let mut out = Vec::with_capacity(8 << 10);
        let encoder = jpeg_encoder::Encoder::new(&mut out, quality.clamp(1, 100));
        encoder.encode(&rgb, w, h, jpeg_encoder::ColorType::Rgb).map_err(|e| Cow::Owned(alloc::format!("JPEG encoding failed: {e}")))?;
        Ok(out)
    }
    #[cfg(not(feature = "std"))]
    {
        let _ = (w, h, quality);
        Err(Cow::Borrowed("JPEG pictures need the std feature"))
    }
}

/// The RGB565 (little-endian) pixels of a JPEG of [`encode_rgb565`], checked to be `width` x
/// `height`; `None` when it is not a picture this build reads.
pub fn decode_to_rgb565(data: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    #[cfg(feature = "std")]
    {
        use zune_jpeg::zune_core::colorspace::ColorSpace;
        use zune_jpeg::zune_core::options::DecoderOptions;
        let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB);
        let mut decoder = zune_jpeg::JpegDecoder::new_with_options(std::io::Cursor::new(data), options);
        let rgb = decoder.decode().ok()?;
        let info = decoder.info()?;
        if u32::from(info.width) != width || u32::from(info.height) != height || rgb.len() != width as usize * height as usize * 3 {
            return None;
        }
        let mut out = Vec::with_capacity(rgb.len() / 3 * 2);
        for px in rgb.chunks_exact(3) {
            let p = (u16::from(px[0] >> 3) << 11) | (u16::from(px[1] >> 2) << 5) | u16::from(px[2] >> 3);
            out.extend_from_slice(&p.to_le_bytes());
        }
        Some(out)
    }
    #[cfg(not(feature = "std"))]
    {
        let _ = (data, width, height);
        None
    }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;

    #[test]
    fn a_picture_survives_a_round_trip_roughly() {
        let (w, h) = (40u32, 24u32);
        let mut pixels = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let p = (((x * 31 / w) as u16) << 11) | (((y * 63 / h) as u16) << 5) | ((x + y) % 32) as u16;
                pixels.extend_from_slice(&p.to_le_bytes());
            }
        }
        let jpeg = encode_rgb565(w, h, &pixels, 90).unwrap();
        assert!(jpeg.len() < pixels.len(), "{} bytes", jpeg.len());
        let back = decode_to_rgb565(&jpeg, w, h).unwrap();
        assert_eq!(back.len(), pixels.len());
        // Close in colour: every channel within a few steps on average.
        let mut error = 0u64;
        for (a, b) in pixels.chunks_exact(2).zip(back.chunks_exact(2)) {
            let (a, b) = (u16::from_le_bytes([a[0], a[1]]), u16::from_le_bytes([b[0], b[1]]));
            error += (a >> 11).abs_diff(b >> 11) as u64 + ((a >> 5) & 63).abs_diff((b >> 5) & 63) as u64 + (a & 31).abs_diff(b & 31) as u64;
        }
        assert!(error / (w * h) as u64 <= 6, "average error {} per pixel", error / (w * h) as u64);
        assert!(decode_to_rgb565(&jpeg, w + 1, h).is_none(), "wrong dimensions are refused");
        assert!(decode_to_rgb565(b"not a jpeg", w, h).is_none());
    }
}
