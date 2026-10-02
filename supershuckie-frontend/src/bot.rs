//! Bot control: letting an external program play the game over the external-commands server.
//!
//! Two ways to play, switchable at any time, with no mode flag between them:
//!
//! - **Real time** (`/input`, `/press`): the game runs at its own pace and the bot changes what it
//!   holds whenever it likes, taking effect on the next frame.
//! - **Lockstep** (`/step`): the game is paused and only moves when the bot asks for N frames,
//!   frame-exact (see `ThreadedSuperShuckieCore::step_frames`). `/set-paused?paused=false` goes
//!   back to real time.
//!
//! What the bot holds is a layer of its own in the core (see `SuperShuckieCore::set_bot_input`),
//! so it and the keyboard never overwrite each other; both are recorded into replays and published
//! to Play Together like any other input. Every route is refused unless the user turned bot control
//! on (Settings › Allow bot control), which is off by default.

use std::num::NonZeroU64;
use std::sync::mpsc::Sender;

use serde_json::json;
use supershuckie_core::emulator::{Input, ScreenData};
use supershuckie_core::StepOutcome;
use supershuckie_frontend_webserver::{BotBody, BotInputParams, BotReply, BotRequest};

use crate::SuperShuckieFrontend;

impl SuperShuckieFrontend {
    /// Whether bot control is allowed (see [`crate::settings::BotControlSettings`]).
    pub fn get_bot_control_enabled(&self) -> bool {
        self.settings.bot_control.enabled
    }

    /// Allow or forbid bot control. Forbidding it releases whatever the bot holds and ends a step
    /// it is running.
    pub fn set_bot_control_enabled(&mut self, enabled: bool) {
        if self.settings.bot_control.enabled == enabled {
            return
        }
        self.settings.bot_control.enabled = enabled;
        self.mark_settings_dirty();
        if !enabled {
            self.release_bot("bot control was turned off");
        }
    }

    /// Release whatever the bot holds and end a step it is running (answered with `reason`).
    pub(crate) fn release_bot(&mut self, reason: &str) {
        self.core.cancel_step(reason);
        self.core.set_bot_input(None);
    }

    /// Answer a bot route. `/step` is answered later, from the core thread, once its frames have
    /// run, so the UI thread never waits on it.
    pub(crate) fn handle_bot_request(&mut self, request: BotRequest, reply: Sender<BotReply>) {
        let refuse = |status: u16, error: &str| {
            let _ = reply.send(Err((status, format!("failed ({error})"))));
        };
        if !self.settings.bot_control.enabled {
            return refuse(403, "bot control is off; turn on Settings > Allow bot control");
        }
        if !self.is_game_running() {
            return refuse(409, "no game is loaded");
        }
        if self.current_export.is_some() {
            return refuse(409, "a video export is running");
        }

        match request {
            BotRequest::Input(params) => {
                if self.core.is_playing_back() {
                    return refuse(409, "a replay is playing back");
                }
                self.core.set_bot_input(Some(to_input(&params)));
                let _ = reply.send(Ok(self.frame_json()));
            }
            BotRequest::Press(params, frames) => {
                if self.core.is_playing_back() {
                    return refuse(409, "a replay is playing back");
                }
                let Some(frames) = NonZeroU64::new(frames) else {
                    return refuse(400, "frames must be at least 1");
                };
                self.core.press_for_frames(to_input(&params), frames);
                let _ = reply.send(Ok(self.frame_json()));
            }
            BotRequest::Step { input, frames, reads } => {
                if self.core.is_playing_back() {
                    return refuse(409, "a replay is playing back");
                }
                // The rest (a replay that ran out, following, a link cable) is refused by the
                // core thread itself, which knows for certain.
                let input = input.map(|params| Some(to_input(&params)).filter(|i| !i.is_empty()));
                let addresses: Vec<u32> = reads.iter().map(|(address, _)| *address).collect();
                self.core.step_frames(input, frames, reads, Box::new(move |result| {
                    let _ = reply.send(match result {
                        Ok(outcome) => Ok(BotBody::Json(step_json(&outcome, &addresses))),
                        Err(error) => Err((409, format!("failed ({error})")))
                    });
                }));
            }
            BotRequest::ReadMemory { address, length } => {
                match self.core.read_ram(address, length) {
                    Some(data) => {
                        let _ = reply.send(Ok(BotBody::Json(json!({ "address": address, "data": hex(&data) }).to_string())));
                    }
                    None => refuse(404, &format!("0x{address:08X} (+{length}) is not mapped")),
                }
            }
            BotRequest::Screenshot => {
                let png = self.core.read_screens(encode_screens_png);
                match png {
                    Some(png) => {
                        let _ = reply.send(Ok(BotBody::Png(png)));
                    }
                    None => refuse(409, "there is no picture to capture"),
                }
            }
        }
    }

    /// `{"frame": ...}`: the frame count the core last reported, which is when the input was
    /// queued (it applies from the next frame).
    fn frame_json(&self) -> BotBody {
        BotBody::Json(json!({ "frame": self.core.get_elapsed_time().frames }).to_string())
    }
}

fn to_input(params: &BotInputParams) -> Input {
    Input {
        a: params.a,
        b: params.b,
        start: params.start,
        select: params.select,
        d_up: params.up,
        d_down: params.down,
        d_left: params.left,
        d_right: params.right,
        l: params.l,
        r: params.r,
        x: params.x,
        y: params.y,
        zl: params.zl,
        zr: params.zr,
        circle: params.circle,
        c_stick: params.cstick,
        touch: params.touch
    }
}

fn step_json(outcome: &StepOutcome, addresses: &[u32]) -> String {
    let reads: Vec<_> = addresses.iter().zip(&outcome.reads).map(|(address, data)| {
        json!({ "address": address, "data": data.as_deref().map(hex) })
    }).collect();
    json!({
        "frame": outcome.frame,
        "frames_run": outcome.frames_run,
        "was_running": outcome.was_running,
        "cancelled": outcome.cancelled.is_some(),
        "cancel_reason": outcome.cancelled,
        "reads": reads
    }).to_string()
}

fn hex(data: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(data.len() * 2);
    for byte in data {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The screens as one PNG, stacked top to bottom (the Nintendo DS and 3DS have two), each
/// left-aligned on black. `None` if there is nothing to show.
fn encode_screens_png(screens: &[ScreenData]) -> Option<Vec<u8>> {
    let width = screens.iter().map(|s| s.width).max()? as usize;
    let height: usize = screens.iter().map(|s| s.height as usize).sum();
    if width == 0 || height == 0 {
        return None
    }
    let mut rgb = vec![0u8; width * height * 3];
    let mut top = 0;
    for screen in screens {
        let (w, h) = (screen.width as usize, screen.height as usize);
        for y in 0..h {
            for x in 0..w {
                // 0xAARRGGBB (the only encoding there is).
                let pixel = screen.pixels.get(y * w + x).copied().unwrap_or(0);
                let at = ((top + y) * width + x) * 3;
                rgb[at..at + 3].copy_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8]);
            }
        }
        top += h;
    }
    Some(encode_png_rgb(width as u32, height as u32, &rgb))
}

/// An 8-bit RGB PNG of `rgb` (row-major, 3 bytes a pixel), stored without compression: a
/// screenshot is at most a few hundred kilobytes that way, and no compression library is needed.
pub(crate) fn encode_png_rgb(width: u32, height: u32, rgb: &[u8]) -> Vec<u8> {
    let row = width as usize * 3;
    let mut raw = Vec::with_capacity((row + 1) * height as usize);
    for line in rgb.chunks(row).take(height as usize) {
        raw.push(0); // filter: none
        raw.extend_from_slice(line);
    }

    // zlib: header, stored deflate blocks of at most 65535 bytes, Adler-32.
    let mut zlib = vec![0x78, 0x01];
    let mut blocks = raw.chunks(65535).peekable();
    if blocks.peek().is_none() {
        zlib.extend_from_slice(&[1, 0, 0, 0xFF, 0xFF]);
    }
    while let Some(block) = blocks.next() {
        zlib.push(u8::from(blocks.peek().is_none()));
        let len = block.len() as u16;
        zlib.extend_from_slice(&len.to_le_bytes());
        zlib.extend_from_slice(&(!len).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for byte in &raw {
        a = (a + *byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());

    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8 bits, RGB, deflate, no filter set, no interlace
    png_chunk(&mut png, b"IHDR", &ihdr);
    png_chunk(&mut png, b"IDAT", &zlib);
    png_chunk(&mut png, b"IEND", &[]);
    png
}

fn png_chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    png.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = png.len();
    png.extend_from_slice(kind);
    png.extend_from_slice(data);
    let crc = crc32(&png[start..]);
    png.extend_from_slice(&crc.to_be_bytes());
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in data {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_chunks_carry_the_right_checksums() {
        assert_eq!(crc32(b"IEND"), 0xAE42_6082);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);

        let png = encode_png_rgb(2, 2, &[255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255]);
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&png[png.len() - 12..], b"\0\0\0\0IEND\xAE\x42\x60\x82");
        // IHDR: 2x2, 8-bit RGB.
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(&png[16..29], &[0, 0, 0, 2, 0, 0, 0, 2, 8, 2, 0, 0, 0]);
    }

    #[test]
    fn screens_are_stacked_top_to_bottom() {
        let top = ScreenData { pixels: vec![0xFF102030; 4 * 2], width: 4, height: 2, encoding: supershuckie_core::emulator::ScreenDataEncoding::A8R8G8B8 };
        let bottom = ScreenData { pixels: vec![0xFFFFFFFF; 2 * 3], width: 2, height: 3, encoding: supershuckie_core::emulator::ScreenDataEncoding::A8R8G8B8 };
        let png = encode_screens_png(&[top, bottom]).expect("png");
        assert_eq!(&png[16..24], &[0, 0, 0, 4, 0, 0, 0, 5], "4 wide, 2 + 3 high");
        assert!(encode_screens_png(&[]).is_none());
    }

    #[test]
    fn step_outcomes_become_json() {
        let outcome = StepOutcome { frame: 12, frames_run: 2, was_running: true, cancelled: Some("the game was unpaused".into()), reads: vec![Some(vec![0xAB, 0x01]), None] };
        let value: serde_json::Value = serde_json::from_str(&step_json(&outcome, &[0xD158, 0x10])).unwrap();
        assert_eq!(value, json!({
            "frame": 12, "frames_run": 2, "was_running": true, "cancelled": true,
            "cancel_reason": "the game was unpaused",
            "reads": [{ "address": 0xD158, "data": "ab01" }, { "address": 16, "data": null }]
        }));
    }
}
