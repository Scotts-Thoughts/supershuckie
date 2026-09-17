//! The wire protocol, version 1.
//!
//! The contract is Cutter's `docs/frame-server-protocol.md`; this file is a direct transcription.
//! Everything is little-endian:
//!
//! ```text
//! message := u32 length | u8 tag | payload      (length counts tag + payload)
//! string  := u32 length | UTF-8 bytes
//! ```

use std::io::{self, Read, Write};

/// The protocol version this server speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// Longest message accepted from the client. Requests are tiny; anything bigger is a framing
/// error rather than a real request.
const MAX_REQUEST_LENGTH: u32 = 1 << 20;

// Request tags (client -> server).
const TAG_HELLO: u8 = 0x01;
const TAG_OPEN: u8 = 0x02;
const TAG_FRAME: u8 = 0x03;
const TAG_RUN: u8 = 0x04;
const TAG_AUDIO: u8 = 0x05;
const TAG_CANCEL: u8 = 0x06;
const TAG_CLOSE: u8 = 0x07;

// Reply tags (server -> client).
const TAG_R_HELLO: u8 = 0x81;
const TAG_R_INFO: u8 = 0x82;
const TAG_R_FRAME: u8 = 0x83;
const TAG_R_DONE: u8 = 0x84;
const TAG_R_AUDIO: u8 = 0x85;
const TAG_R_CANCELLED: u8 = 0x86;
const TAG_R_ERROR: u8 = 0x8F;

/// A request from the client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Hello { protocol: u32 },
    Open { rom: String, replay: String, layout: u8, audio: bool },
    Frame { id: u32, index: u64 },
    Run { id: u32, from: u64, to: u64 },
    Audio { id: u32, first_frame: u64, frames: u32 },
    Cancel { id: u32 },
    Close,
}

/// The `Info` reply's payload.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Info {
    pub console: String,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub frames: u64,
    pub sample_rate: u32,
    pub rom_ok: bool,
    pub core_recorded: String,
    pub core_running: String,
    pub keyframes: Vec<u64>,
    pub bookmarks: Vec<(String, u64)>,
    pub crop: Option<(u64, u64)>,
    pub counters: Vec<(String, i64)>,
}

/// A reply to the client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    Hello { protocol: u32, server: String, cores: Vec<String> },
    Info(Info),
    /// `pixels` is `width × height × 4` bytes, B,G,R,A per pixel, rows top to bottom.
    Frame { id: u32, index: u64, pixels: Vec<u8> },
    Done { id: u32 },
    /// `samples` is interleaved stereo (left, right, ...).
    Audio { id: u32, first_frame: u64, frames: u32, samples: Vec<i16> },
    Cancelled { id: u32 },
    Error { id: u32, message: String },
}

/// Why a message could not be decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    Truncated,
    UnknownTag(u8),
    BadString,
    TrailingBytes(usize),
    TooLong(u32),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Truncated => f.write_str("message is shorter than its fields"),
            DecodeError::UnknownTag(t) => write!(f, "unknown message tag 0x{t:02X}"),
            DecodeError::BadString => f.write_str("string is not UTF-8"),
            DecodeError::TrailingBytes(n) => write!(f, "{n} unexpected trailing bytes"),
            DecodeError::TooLong(n) => write!(f, "message of {n} bytes is longer than allowed"),
        }
    }
}

impl std::error::Error for DecodeError {}

// ---------------------------------------------------------------------------------------------
// Writing

struct Encoder<'a> {
    out: &'a mut Vec<u8>,
}

impl Encoder<'_> {
    fn u8(&mut self, v: u8) {
        self.out.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }
    fn i64(&mut self, v: i64) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }
    fn string(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.out.extend_from_slice(s.as_bytes());
    }
}

/// Append one framed message (length, tag, payload) to `out`. `body` writes tag + payload.
fn frame(out: &mut Vec<u8>, body: impl FnOnce(&mut Encoder<'_>)) {
    let start = out.len();
    out.extend_from_slice(&[0; 4]);
    body(&mut Encoder { out });
    let len = (out.len() - start - 4) as u32;
    out[start..start + 4].copy_from_slice(&len.to_le_bytes());
}

impl Request {
    /// Append the framed message to `out` (what a client does; used here to test the codec).
    #[cfg(test)]
    pub fn encode(&self, out: &mut Vec<u8>) {
        frame(out, |e| match self {
            Request::Hello { protocol } => {
                e.u8(TAG_HELLO);
                e.u32(*protocol);
            }
            Request::Open { rom, replay, layout, audio } => {
                e.u8(TAG_OPEN);
                e.string(rom);
                e.string(replay);
                e.u8(*layout);
                e.u8(u8::from(*audio));
            }
            Request::Frame { id, index } => {
                e.u8(TAG_FRAME);
                e.u32(*id);
                e.u64(*index);
            }
            Request::Run { id, from, to } => {
                e.u8(TAG_RUN);
                e.u32(*id);
                e.u64(*from);
                e.u64(*to);
            }
            Request::Audio { id, first_frame, frames } => {
                e.u8(TAG_AUDIO);
                e.u32(*id);
                e.u64(*first_frame);
                e.u32(*frames);
            }
            Request::Cancel { id } => {
                e.u8(TAG_CANCEL);
                e.u32(*id);
            }
            Request::Close => e.u8(TAG_CLOSE),
        });
    }

    /// Decode one message body (tag + payload, without the length prefix).
    pub fn decode(body: &[u8]) -> Result<Request, DecodeError> {
        let mut d = Decoder { bytes: body };
        let tag = d.u8()?;
        let request = match tag {
            TAG_HELLO => Request::Hello { protocol: d.u32()? },
            TAG_OPEN => Request::Open {
                rom: d.string()?,
                replay: d.string()?,
                layout: d.u8()?,
                audio: d.u8()? != 0,
            },
            TAG_FRAME => Request::Frame { id: d.u32()?, index: d.u64()? },
            TAG_RUN => Request::Run { id: d.u32()?, from: d.u64()?, to: d.u64()? },
            TAG_AUDIO => Request::Audio { id: d.u32()?, first_frame: d.u64()?, frames: d.u32()? },
            TAG_CANCEL => Request::Cancel { id: d.u32()? },
            TAG_CLOSE => Request::Close,
            other => return Err(DecodeError::UnknownTag(other)),
        };
        d.finish()?;
        Ok(request)
    }

    /// Read one framed request from `r`. `Ok(None)` at a clean end of input.
    pub fn read(r: &mut impl Read) -> io::Result<Option<Result<Request, DecodeError>>> {
        Ok(read_message(r)?.map(|body| Request::decode(&body)))
    }
}

impl Reply {
    /// Append the framed message to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        frame(out, |e| match self {
            Reply::Hello { protocol, server, cores } => {
                e.u8(TAG_R_HELLO);
                e.u32(*protocol);
                e.string(server);
                e.u32(cores.len() as u32);
                for c in cores {
                    e.string(c);
                }
            }
            Reply::Info(info) => {
                e.u8(TAG_R_INFO);
                e.string(&info.console);
                e.u32(info.width);
                e.u32(info.height);
                e.u32(info.fps_num);
                e.u32(info.fps_den);
                e.u64(info.frames);
                e.u32(info.sample_rate);
                e.u8(u8::from(info.rom_ok));
                e.string(&info.core_recorded);
                e.string(&info.core_running);
                e.u32(info.keyframes.len() as u32);
                for k in &info.keyframes {
                    e.u64(*k);
                }
                e.u32(info.bookmarks.len() as u32);
                for (name, frame) in &info.bookmarks {
                    e.string(name);
                    e.u64(*frame);
                }
                match info.crop {
                    Some((start, end)) => {
                        e.u8(1);
                        e.u64(start);
                        e.u64(end);
                    }
                    None => {
                        e.u8(0);
                        e.u64(0);
                        e.u64(0);
                    }
                }
                e.u32(info.counters.len() as u32);
                for (name, value) in &info.counters {
                    e.string(name);
                    e.i64(*value);
                }
            }
            Reply::Frame { id, index, pixels } => {
                e.u8(TAG_R_FRAME);
                e.u32(*id);
                e.u64(*index);
                e.out.extend_from_slice(pixels);
            }
            Reply::Done { id } => {
                e.u8(TAG_R_DONE);
                e.u32(*id);
            }
            Reply::Audio { id, first_frame, frames, samples } => {
                e.u8(TAG_R_AUDIO);
                e.u32(*id);
                e.u64(*first_frame);
                e.u32(*frames);
                e.u32(samples.len() as u32);
                e.out.reserve(samples.len() * 2);
                for s in samples {
                    e.out.extend_from_slice(&s.to_le_bytes());
                }
            }
            Reply::Cancelled { id } => {
                e.u8(TAG_R_CANCELLED);
                e.u32(*id);
            }
            Reply::Error { id, message } => {
                e.u8(TAG_R_ERROR);
                e.u32(*id);
                e.string(message);
            }
        });
    }

    /// Decode one message body (what a client does; used here to test the codec).
    #[cfg(test)]
    pub fn decode(body: &[u8]) -> Result<Reply, DecodeError> {
        let mut d = Decoder { bytes: body };
        let tag = d.u8()?;
        let reply = match tag {
            TAG_R_HELLO => {
                let protocol = d.u32()?;
                let server = d.string()?;
                let n = d.u32()?;
                let mut cores = Vec::new();
                for _ in 0..n {
                    cores.push(d.string()?);
                }
                Reply::Hello { protocol, server, cores }
            }
            TAG_R_INFO => {
                let mut info = Info {
                    console: d.string()?,
                    width: d.u32()?,
                    height: d.u32()?,
                    fps_num: d.u32()?,
                    fps_den: d.u32()?,
                    frames: d.u64()?,
                    sample_rate: d.u32()?,
                    rom_ok: d.u8()? != 0,
                    core_recorded: d.string()?,
                    core_running: d.string()?,
                    ..Info::default()
                };
                let k = d.u32()?;
                for _ in 0..k {
                    info.keyframes.push(d.u64()?);
                }
                let b = d.u32()?;
                for _ in 0..b {
                    let name = d.string()?;
                    let frame = d.u64()?;
                    info.bookmarks.push((name, frame));
                }
                let has_crop = d.u8()? != 0;
                let crop_start = d.u64()?;
                let crop_end = d.u64()?;
                info.crop = has_crop.then_some((crop_start, crop_end));
                let c = d.u32()?;
                for _ in 0..c {
                    let name = d.string()?;
                    let value = d.i64()?;
                    info.counters.push((name, value));
                }
                Reply::Info(info)
            }
            TAG_R_FRAME => {
                let id = d.u32()?;
                let index = d.u64()?;
                let pixels = d.rest().to_vec();
                Reply::Frame { id, index, pixels }
            }
            TAG_R_DONE => Reply::Done { id: d.u32()? },
            TAG_R_AUDIO => {
                let id = d.u32()?;
                let first_frame = d.u64()?;
                let frames = d.u32()?;
                let n = d.u32()? as usize;
                let raw = d.take(n * 2)?;
                let samples = raw.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
                Reply::Audio { id, first_frame, frames, samples }
            }
            TAG_R_CANCELLED => Reply::Cancelled { id: d.u32()? },
            TAG_R_ERROR => Reply::Error { id: d.u32()?, message: d.string()? },
            other => return Err(DecodeError::UnknownTag(other)),
        };
        d.finish()?;
        Ok(reply)
    }

    /// Read one framed reply from `r`. `Ok(None)` at a clean end of input.
    #[cfg(test)]
    pub fn read(r: &mut impl Read) -> io::Result<Option<Result<Reply, DecodeError>>> {
        Ok(read_message_up_to(r, u32::MAX)?.map(|body| Reply::decode(&body)))
    }
}

/// Write an already-encoded buffer and flush it.
pub fn write_all_flushed(w: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    w.write_all(bytes)?;
    w.flush()
}

// ---------------------------------------------------------------------------------------------
// Reading

/// Read one framed message body (tag + payload). `Ok(None)` at a clean end of input, an
/// `UnexpectedEof` error if the input ends inside a message.
fn read_message(r: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    read_message_up_to(r, MAX_REQUEST_LENGTH)
}

fn read_message_up_to(r: &mut impl Read, max_len: u32) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len);
    if len == 0 || len > max_len {
        return Err(io::Error::new(io::ErrorKind::InvalidData, DecodeError::TooLong(len)));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    Ok(Some(body))
}

struct Decoder<'a> {
    bytes: &'a [u8],
}

impl<'a> Decoder<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.bytes.len() < n {
            return Err(DecodeError::Truncated);
        }
        let (head, tail) = self.bytes.split_at(n);
        self.bytes = tail;
        Ok(head)
    }
    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    #[cfg(test)]
    fn i64(&mut self) -> Result<i64, DecodeError> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn string(&mut self) -> Result<String, DecodeError> {
        let len = self.u32()? as usize;
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| DecodeError::BadString)
    }
    #[cfg(test)]
    fn rest(&mut self) -> &'a [u8] {
        let all = self.bytes;
        self.bytes = &[];
        all
    }
    fn finish(self) -> Result<(), DecodeError> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err(DecodeError::TrailingBytes(self.bytes.len()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip_request(r: Request) {
        let mut bytes = Vec::new();
        r.encode(&mut bytes);
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        assert_eq!(len, bytes.len() - 4, "length counts tag + payload");
        assert_eq!(Request::decode(&bytes[4..]), Ok(r.clone()));
        let mut cursor = io::Cursor::new(bytes);
        assert_eq!(Request::read(&mut cursor).unwrap(), Some(Ok(r)));
        assert_eq!(Request::read(&mut cursor).unwrap(), None, "clean EOF after the message");
    }

    fn round_trip_reply(r: Reply) {
        let mut bytes = Vec::new();
        r.encode(&mut bytes);
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        assert_eq!(len, bytes.len() - 4);
        assert_eq!(Reply::decode(&bytes[4..]), Ok(r.clone()));
        let mut cursor = io::Cursor::new(bytes);
        assert_eq!(Reply::read(&mut cursor).unwrap(), Some(Ok(r)));
        assert_eq!(Reply::read(&mut cursor).unwrap(), None);
    }

    #[test]
    fn every_request_round_trips() {
        round_trip_request(Request::Hello { protocol: 1 });
        round_trip_request(Request::Open {
            rom: "/roms/game.gba".into(),
            replay: "/replays/run — ünïcode.replay".into(),
            layout: 3,
            audio: true,
        });
        round_trip_request(Request::Open { rom: String::new(), replay: String::new(), layout: 0, audio: false });
        round_trip_request(Request::Frame { id: 7, index: 0 });
        round_trip_request(Request::Frame { id: u32::MAX, index: u64::MAX });
        round_trip_request(Request::Run { id: 9, from: 2000, to: 2060 });
        round_trip_request(Request::Audio { id: 11, first_frame: 3000, frames: 10 });
        round_trip_request(Request::Cancel { id: 9 });
        round_trip_request(Request::Close);
    }

    #[test]
    fn every_reply_round_trips() {
        round_trip_reply(Reply::Hello {
            protocol: 1,
            server: "supershuckie-frame-server 0.4.14".into(),
            cores: vec!["Game Boy".into(), "Nintendo DS".into()],
        });
        round_trip_reply(Reply::Hello { protocol: 1, server: String::new(), cores: vec![] });
        round_trip_reply(Reply::Info(Info {
            console: "Game Boy Advance".into(),
            width: 240,
            height: 160,
            fps_num: 4194304,
            fps_den: 70224,
            frames: 36000,
            sample_rate: 48000,
            rom_ok: true,
            core_recorded: "mGBA 0.10.5".into(),
            core_running: "mGBA 0.10.5".into(),
            keyframes: vec![0, 120, 240, 36000],
            bookmarks: vec![("start".into(), 5), ("end".into(), 35000)],
            crop: Some((100, 35900)),
            counters: vec![("resets".into(), -3), ("encounters".into(), i64::MAX)],
        }));
        round_trip_reply(Reply::Info(Info { rom_ok: false, crop: None, ..Info::default() }));
        round_trip_reply(Reply::Frame { id: 3, index: 1000, pixels: (0..=255u8).cycle().take(240 * 160 * 4).collect() });
        round_trip_reply(Reply::Frame { id: 3, index: 1000, pixels: vec![] });
        round_trip_reply(Reply::Done { id: 4 });
        round_trip_reply(Reply::Audio {
            id: 5,
            first_frame: 3000,
            frames: 10,
            samples: vec![0, -1, i16::MAX, i16::MIN, 1234, -1234],
        });
        round_trip_reply(Reply::Audio { id: 5, first_frame: 0, frames: 0, samples: vec![] });
        round_trip_reply(Reply::Cancelled { id: 6 });
        round_trip_reply(Reply::Error { id: 0, message: "no such file".into() });
    }

    #[test]
    fn frame_pixels_are_the_raw_words() {
        // The server writes u32 0xAARRGGBB words as little-endian bytes: B, G, R, A.
        let word: u32 = 0xAABBCCDD;
        let mut pixels = Vec::new();
        pixels.extend_from_slice(&word.to_le_bytes());
        let reply = Reply::Frame { id: 1, index: 2, pixels };
        let mut bytes = Vec::new();
        reply.encode(&mut bytes);
        // length(4) tag(1) id(4) index(8) then the pixel
        assert_eq!(&bytes[17..], &[0xDD, 0xCC, 0xBB, 0xAA]);
        assert_eq!(bytes[4], TAG_R_FRAME);
    }

    #[test]
    fn wire_layout_matches_the_document() {
        let mut bytes = Vec::new();
        Request::Run { id: 1, from: 2, to: 3 }.encode(&mut bytes);
        assert_eq!(
            bytes,
            [
                21, 0, 0, 0, // length = 1 + 4 + 8 + 8
                0x04, // Run
                1, 0, 0, 0, // id
                2, 0, 0, 0, 0, 0, 0, 0, // from
                3, 0, 0, 0, 0, 0, 0, 0, // to
            ]
        );

        let mut bytes = Vec::new();
        Reply::Error { id: 0, message: "hi".into() }.encode(&mut bytes);
        assert_eq!(bytes, [11, 0, 0, 0, 0x8F, 0, 0, 0, 0, 2, 0, 0, 0, b'h', b'i']);
    }

    #[test]
    fn bad_input_is_an_error_not_a_panic() {
        assert_eq!(Request::decode(&[]), Err(DecodeError::Truncated));
        assert_eq!(Request::decode(&[0x03, 1, 0, 0, 0]), Err(DecodeError::Truncated));
        assert_eq!(Request::decode(&[0x42]), Err(DecodeError::UnknownTag(0x42)));
        assert_eq!(Request::decode(&[0x07, 0]), Err(DecodeError::TrailingBytes(1)));
        assert_eq!(Request::decode(&[0x02, 1, 0, 0, 0, 0xFF]), Err(DecodeError::BadString));
        assert_eq!(Reply::decode(&[0x85, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 5, 0, 0, 0, 1, 2]), Err(DecodeError::Truncated));

        // Length claims more than there is.
        let mut cursor = io::Cursor::new(vec![9, 0, 0, 0, 0x07]);
        assert_eq!(Request::read(&mut cursor).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        // A zero-length message is not a message.
        let mut cursor = io::Cursor::new(vec![0, 0, 0, 0]);
        assert_eq!(Request::read(&mut cursor).unwrap_err().kind(), io::ErrorKind::InvalidData);
        // Absurd lengths are refused before allocating.
        let mut cursor = io::Cursor::new(vec![0xFF, 0xFF, 0xFF, 0xFF]);
        assert_eq!(Request::read(&mut cursor).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn several_messages_in_one_stream() {
        let mut bytes = Vec::new();
        Request::Hello { protocol: 1 }.encode(&mut bytes);
        Request::Frame { id: 1, index: 5 }.encode(&mut bytes);
        Request::Close.encode(&mut bytes);
        let mut cursor = io::Cursor::new(bytes);
        assert_eq!(Request::read(&mut cursor).unwrap(), Some(Ok(Request::Hello { protocol: 1 })));
        assert_eq!(Request::read(&mut cursor).unwrap(), Some(Ok(Request::Frame { id: 1, index: 5 })));
        assert_eq!(Request::read(&mut cursor).unwrap(), Some(Ok(Request::Close)));
        assert_eq!(Request::read(&mut cursor).unwrap(), None);
    }
}
