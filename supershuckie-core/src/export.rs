//! Video export from replays.
//!
//! Renders an attached replay to a [`VideoFrameSink`] frame-by-frame, deterministically and as
//! fast as the machine allows (not real time). Frames come from stepping the replay (load keyframe
//! state, replay inputs), so the output is identical regardless of host CPU load.
//!
//! The recorded per-frame millisecond timestamps are deliberately ignored for video timing: one
//! emulated frame becomes exactly one video frame at the console's native refresh rate (see
//! [`EmulatorCore::frame_rate`]). This keeps the render clean (no baked-in pauses/slowdowns).
//!
//! This module is free of any ffmpeg/process concerns; it only produces composited ARGB frames
//! through the [`VideoFrameSink`] trait, mirroring the `ReplayFileSink` pattern. The encoder (e.g.
//! an ffmpeg subprocess) lives in the frontend.

use alloc::borrow::Cow;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::emulator::ScreenData;
use crate::SuperShuckieCore;
use supershuckie_replay_recorder::UnsignedInteger;

/// Consumes composited frames during a video export.
///
/// Implemented by the frontend (e.g. an ffmpeg subprocess). Mirrors the `ReplayFileSink` pattern.
pub trait VideoFrameSink: Send {
    /// Called once before the first frame with the fixed output geometry and fps.
    fn begin(&mut self, width: u32, height: u32, fps_num: u32, fps_den: u32) -> Result<(), VideoExportError>;

    /// One frame, ARGB packed as `0xAARRGGBB` (`len == width*height`). Called once per emulated frame.
    fn push_frame(&mut self, argb: &[u32]) -> Result<(), VideoExportError>;

    /// Called once after the last frame on success. Must flush/finalize.
    fn finish(&mut self) -> Result<(), VideoExportError>;

    /// Called instead of [`finish`](VideoFrameSink::finish) if the export was cancelled or errored.
    /// Must clean up (e.g. kill the encoder, delete the partial output).
    fn abort(&mut self);
}

/// Error that can occur during a video export.
#[derive(Clone, Debug)]
pub enum VideoExportError {
    /// No replay player was attached to export from.
    NoReplayAttached,

    /// The sink failed (pipe/encoder failure, spawn failure, etc.).
    #[allow(missing_docs)]
    Sink { explanation: Cow<'static, str> },

    /// The export was cancelled by the caller.
    Cancelled,
}

impl core::fmt::Display for VideoExportError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            VideoExportError::NoReplayAttached => f.write_str("No replay is attached to export"),
            VideoExportError::Sink { explanation } => f.write_str(explanation),
            VideoExportError::Cancelled => f.write_str("The export was cancelled"),
        }
    }
}

/// Frame range to export. Inclusive start, exclusive end. `end_frame == None` => up to total frames.
#[derive(Copy, Clone, Debug)]
#[allow(missing_docs)]
pub struct ExportRange {
    pub start_frame: UnsignedInteger,
    pub end_frame: Option<UnsignedInteger>,
}

/// How to lay out multi-screen consoles (Nintendo DS).
#[derive(Copy, Clone, PartialEq, Debug, Default)]
pub enum ScreenLayout {
    /// Top screen above the bottom screen (NDS default).
    #[default]
    VerticalStack,
    /// Top screen left of the bottom screen.
    HorizontalStack,
    /// Only the first (top) screen.
    TopOnly,
    /// Only the second (bottom) screen.
    BottomOnly,
}

/// Compute the output geometry `(width, height)` for the given screens and layout.
///
/// Returns `None` if there are no screens.
pub fn output_geometry(screens: &[ScreenData], layout: ScreenLayout) -> Option<(u32, u32)> {
    match screens {
        [] => None,
        [only] => Some((only.width as u32, only.height as u32)),
        [a, b, ..] => {
            let (aw, ah) = (a.width as u32, a.height as u32);
            let (bw, bh) = (b.width as u32, b.height as u32);
            Some(match layout {
                ScreenLayout::VerticalStack => (aw.max(bw), ah + bh),
                ScreenLayout::HorizontalStack => (aw + bw, ah.max(bh)),
                ScreenLayout::TopOnly => (aw, ah),
                ScreenLayout::BottomOnly => (bw, bh),
            })
        }
    }
}

/// Composite `screens` into `out` (ARGB `u32`, `0xAARRGGBB`) according to `layout`.
///
/// `out` is resized to `width*height` (geometry from [`output_geometry`]) and any padding is filled
/// opaque black. The buffer is intended to be reused across frames.
pub fn composite_screens(screens: &[ScreenData], layout: ScreenLayout, out: &mut Vec<u32>) {
    const BLACK: u32 = 0xFF000000;

    let Some((width, height)) = output_geometry(screens, layout) else {
        out.clear();
        return;
    };
    let (width, height) = (width as usize, height as usize);

    out.clear();
    out.resize(width * height, BLACK);

    // Blit `src` (src_w x src_h) into `out` at (dst_x, dst_y).
    fn blit(out: &mut [u32], out_w: usize, src: &ScreenData, dst_x: usize, dst_y: usize) {
        let src_w = src.width;
        let src_h = src.height;
        for row in 0..src_h {
            let src_start = row * src_w;
            let Some(src_row) = src.pixels.get(src_start..src_start + src_w) else {
                break;
            };
            let dst_start = (dst_y + row) * out_w + dst_x;
            let Some(dst_row) = out.get_mut(dst_start..dst_start + src_w) else {
                break;
            };
            dst_row.copy_from_slice(src_row);
        }
    }

    match screens {
        [] => {}
        [only] => blit(out, width, only, 0, 0),
        [a, b, ..] => match layout {
            ScreenLayout::VerticalStack => {
                blit(out, width, a, 0, 0);
                blit(out, width, b, 0, a.height);
            }
            ScreenLayout::HorizontalStack => {
                blit(out, width, a, 0, 0);
                blit(out, width, b, a.width, 0);
            }
            ScreenLayout::TopOnly => blit(out, width, a, 0, 0),
            ScreenLayout::BottomOnly => blit(out, width, b, 0, 0),
        },
    }
}

impl SuperShuckieCore {
    /// Export `range` of the attached replay to `sink`.
    ///
    /// Calls `progress(done, total)` once per frame. Returns [`VideoExportError::Cancelled`] if
    /// `cancel` flips. Requires a [`ReplayFilePlayer`](supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer)
    /// to already be attached (else [`VideoExportError::NoReplayAttached`]).
    ///
    /// One emulated frame becomes exactly one video frame; recorded timestamps and replay speed are
    /// ignored for timing. Stepping uses `run_unlocked`, so there is no real-time throttle.
    pub fn export_frames(
        &mut self,
        range: ExportRange,
        layout: ScreenLayout,
        sink: &mut dyn VideoFrameSink,
        cancel: &AtomicBool,
        mut progress: impl FnMut(UnsignedInteger, UnsignedInteger),
    ) -> Result<(), VideoExportError> {
        let total = match self.replay_player.as_ref() {
            Some(p) => p.get_total_frames(),
            None => return Err(VideoExportError::NoReplayAttached),
        };

        let start = range.start_frame.min(total);
        let end = range.end_frame.unwrap_or(total).min(total);
        let (fps_num, fps_den) = self.core.frame_rate();

        // Empty range: begin with the right geometry, push nothing, finish cleanly.
        if end <= start {
            self.go_to_replay_frame(start);
            let (width, height) = output_geometry(self.core.get_screens(), layout)
                .ok_or_else(|| VideoExportError::Sink { explanation: Cow::Borrowed("core has no screens") })?;
            sink.begin(width, height, fps_num, fps_den)?;
            progress(0, 0);
            sink.finish()?;
            return Ok(());
        }

        // Position the emulator at the start frame. Afterwards total_frames == start and the
        // framebuffer holds frame `start`.
        self.go_to_replay_frame(start);

        let (width, height) = match output_geometry(self.core.get_screens(), layout) {
            Some(g) => g,
            None => {
                sink.abort();
                return Err(VideoExportError::Sink { explanation: Cow::Borrowed("core has no screens") });
            }
        };

        if let Err(e) = sink.begin(width, height, fps_num, fps_den) {
            sink.abort();
            return Err(e);
        }

        let span = end - start;
        let mut frame_buf: Vec<u32> = Vec::new();

        loop {
            if self.total_frames >= end || self.replay_stalled {
                break;
            }

            if cancel.load(Ordering::Relaxed) {
                sink.abort();
                return Err(VideoExportError::Cancelled);
            }

            // Composite + push the CURRENT frame (frame `start` on the first iteration).
            composite_screens(self.core.get_screens(), layout, &mut frame_buf);
            if let Err(e) = sink.push_frame(&frame_buf) {
                sink.abort();
                return Err(e);
            }

            progress(self.total_frames - start, span);

            // Advance exactly one emulated frame.
            let target = self.total_frames + 1;
            while self.total_frames < target && !self.replay_stalled {
                self.run_unlocked();
            }
        }

        sink.finish()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emulator::ScreenDataEncoding;
    use alloc::vec;

    fn screen(w: usize, h: usize, fill: u32) -> ScreenData {
        ScreenData {
            pixels: vec![fill; w * h],
            width: w,
            height: h,
            encoding: ScreenDataEncoding::A8R8G8B8,
        }
    }

    #[test]
    fn geometry_single_screen() {
        let s = [screen(240, 160, 0)];
        assert_eq!(output_geometry(&s, ScreenLayout::VerticalStack), Some((240, 160)));
    }

    #[test]
    fn geometry_nds_layouts() {
        let s = [screen(256, 192, 0), screen(256, 192, 0)];
        assert_eq!(output_geometry(&s, ScreenLayout::VerticalStack), Some((256, 384)));
        assert_eq!(output_geometry(&s, ScreenLayout::HorizontalStack), Some((512, 192)));
        assert_eq!(output_geometry(&s, ScreenLayout::TopOnly), Some((256, 192)));
        assert_eq!(output_geometry(&s, ScreenLayout::BottomOnly), Some((256, 192)));
    }

    #[test]
    fn composite_vertical_stack_places_top_then_bottom() {
        let top = screen(256, 192, 0xFF112233);
        let bottom = screen(256, 192, 0xFF445566);
        let s = [top, bottom];
        let mut out = Vec::new();
        composite_screens(&s, ScreenLayout::VerticalStack, &mut out);

        assert_eq!(out.len(), 256 * 384);
        // First row belongs to the top screen.
        assert_eq!(out[0], 0xFF112233);
        // First pixel of row 192 (start of the bottom screen) belongs to the bottom screen.
        assert_eq!(out[192 * 256], 0xFF445566);
        // Last pixel belongs to the bottom screen.
        assert_eq!(out[256 * 384 - 1], 0xFF445566);
    }

    #[test]
    fn composite_horizontal_stack_places_left_then_right() {
        let left = screen(256, 192, 0xFFAAAAAA);
        let right = screen(256, 192, 0xFFBBBBBB);
        let s = [left, right];
        let mut out = Vec::new();
        composite_screens(&s, ScreenLayout::HorizontalStack, &mut out);

        assert_eq!(out.len(), 512 * 192);
        // Row 0: first 256 px are the left screen, next 256 are the right screen.
        assert_eq!(out[0], 0xFFAAAAAA);
        assert_eq!(out[255], 0xFFAAAAAA);
        assert_eq!(out[256], 0xFFBBBBBB);
        assert_eq!(out[511], 0xFFBBBBBB);
    }

    #[test]
    fn composite_single_screen_copies_through() {
        let s = [screen(160, 144, 0xFF777777)];
        let mut out = Vec::new();
        composite_screens(&s, ScreenLayout::VerticalStack, &mut out);
        assert_eq!(out.len(), 160 * 144);
        assert!(out.iter().all(|&p| p == 0xFF777777));
    }

    #[test]
    fn argb_reinterpret_is_bgra_on_le() {
        // 0xAARRGGBB in memory on little-endian is bytes [BB, GG, RR, AA] == bgra.
        let px: u32 = 0xAABBCCDD; // A=AA R=BB G=CC B=DD
        let bytes = px.to_le_bytes();
        if cfg!(target_endian = "little") {
            assert_eq!(bytes, [0xDD, 0xCC, 0xBB, 0xAA]); // B, G, R, A
        }
    }
}
