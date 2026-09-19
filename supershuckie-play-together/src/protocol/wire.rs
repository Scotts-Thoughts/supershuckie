//! Framing and the primitive encoder/decoder.
//!
//! ```text
//! message := u32 length | u8 tag | payload      (length counts tag + payload)
//! string  := u32 length | UTF-8 bytes
//! bytes   := u32 length | raw bytes
//! vec<T>  := u32 count  | T*
//! ```
//!
//! Everything is little-endian. Every length and count is checked against its cap before a
//! single byte is allocated for it, and message bodies are read from the socket in
//! [`READ_CHUNK`]-sized pieces so a claimed length never becomes an allocation on its own.

use std::convert::Infallible;
use std::io::{self, Read};

use crate::error::DecodeError;

/// Longest framed message (tag + payload) for the tags that carry replay data
/// (`Stream`, `Snapshot`).
pub const MAX_MESSAGE_LENGTH: u32 = 48 << 20;

/// Longest framed message for every other tag, known or unknown.
pub const MAX_SMALL_MESSAGE_LENGTH: u32 = 64 << 10;

/// How much of a message body is read from the socket per step.
pub const READ_CHUNK: usize = 64 << 10;

/// The tags whose messages may be as long as [`MAX_MESSAGE_LENGTH`].
pub(crate) const LARGE_TAGS: [u8; 3] = [super::TAG_STREAM, super::TAG_SNAPSHOT, super::TAG_START_STATE];

/// The longest framed length (tag + payload) accepted for a message with this tag.
pub fn max_message_length(tag: u8) -> u32 {
    if LARGE_TAGS.contains(&tag) { MAX_MESSAGE_LENGTH } else { MAX_SMALL_MESSAGE_LENGTH }
}

// ---------------------------------------------------------------------------------------------
// Writing

pub(crate) struct Encoder<'a> {
    pub(crate) out: &'a mut Vec<u8>,
}

impl Encoder<'_> {
    pub(crate) fn u8(&mut self, v: u8) {
        self.out.push(v);
    }
    pub(crate) fn u16(&mut self, v: u16) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }
    pub(crate) fn u32(&mut self, v: u32) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }
    pub(crate) fn u64(&mut self, v: u64) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }
    pub(crate) fn i64(&mut self, v: i64) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }
    pub(crate) fn bool(&mut self, v: bool) {
        self.out.push(u8::from(v));
    }
    pub(crate) fn string(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }
    pub(crate) fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.out.extend_from_slice(b);
    }
    pub(crate) fn hash(&mut self, h: &[u8; 32]) {
        self.out.extend_from_slice(h);
    }
}

/// Append one framed message (length, tag, payload) to `out`. `body` writes tag + payload.
pub(crate) fn frame(out: &mut Vec<u8>, body: impl FnOnce(&mut Encoder<'_>)) {
    let start = out.len();
    out.extend_from_slice(&[0; 4]);
    body(&mut Encoder { out });
    let len = (out.len() - start - 4) as u32;
    out[start..start + 4].copy_from_slice(&len.to_le_bytes());
}

// ---------------------------------------------------------------------------------------------
// Reading

/// Why [`read_frame`] stopped.
#[derive(Debug)]
pub enum ReadError<S> {
    /// The socket failed (including a clean end of input inside a message: `UnexpectedEof`).
    Io(io::Error),
    /// The length prefix is unacceptable for the tag.
    Decode(DecodeError),
    /// The poll callback asked to stop.
    Stopped(S),
}

/// Read one framed message. Returns the whole frame (length prefix, tag, payload) so it can be
/// relayed byte for byte; `Ok(None)` at a clean end of input before any byte of a message.
///
/// `poll` is called whenever a socket read times out (`TimedOut` / `WouldBlock`) so the caller
/// can enforce its own deadlines and stop flags; returning `Err` stops the read. It receives how
/// many bytes of the current frame have arrived so far, so a caller can tell a stalled peer from
/// a slow one. Bodies are read in [`READ_CHUNK`] pieces: a message that claims 48 MiB over a
/// stream that ends after 10 bytes fails with `UnexpectedEof` having allocated only one chunk.
pub fn read_frame<R: Read, S>(
    r: &mut R,
    poll: &mut impl FnMut(usize) -> Result<(), S>,
) -> Result<Option<Vec<u8>>, ReadError<S>> {
    let mut frame = Vec::with_capacity(5);
    frame.resize(4, 0);
    let mut got = 0usize;
    if !read_fully(r, &mut frame[..4], poll, &mut got, true)? {
        return Ok(None);
    }
    let len = u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]);
    if len == 0 {
        return Err(ReadError::Decode(DecodeError::TooLong(len)));
    }
    // Tag first: the cap depends on it, and it is checked before the body is allocated.
    frame.push(0);
    read_fully(r, &mut frame[4..5], poll, &mut got, false)?;
    let tag = frame[4];
    if len > max_message_length(tag) {
        return Err(ReadError::Decode(DecodeError::TooLong(len)));
    }
    let mut remaining = len as usize - 1;
    while remaining > 0 {
        let step = remaining.min(READ_CHUNK);
        let start = frame.len();
        frame.resize(start + step, 0);
        read_fully(r, &mut frame[start..], poll, &mut got, false)?;
        remaining -= step;
    }
    Ok(Some(frame))
}

/// Fill `buf`, calling `poll` on every timeout. `Ok(false)` if the input ended before the first
/// byte and `clean_eof_ok`; `UnexpectedEof` for any other early end.
fn read_fully<R: Read, S>(
    r: &mut R,
    buf: &mut [u8],
    poll: &mut impl FnMut(usize) -> Result<(), S>,
    got: &mut usize,
    clean_eof_ok: bool,
) -> Result<bool, ReadError<S>> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => {
                if filled == 0 && clean_eof_ok && *got == 0 {
                    return Ok(false);
                }
                return Err(ReadError::Io(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed inside a message")));
            }
            Ok(n) => {
                filled += n;
                *got += n;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if matches!(e.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) => {
                poll(*got).map_err(ReadError::Stopped)?;
            }
            Err(e) => return Err(ReadError::Io(e)),
        }
    }
    Ok(true)
}

/// [`read_frame`] without a poll callback (for readers that never time out, such as cursors).
pub fn read_frame_blocking<R: Read>(r: &mut R) -> Result<Option<Vec<u8>>, ReadError<Infallible>> {
    read_frame(r, &mut |_| Ok(()))
}

pub(crate) struct Decoder<'a> {
    bytes: &'a [u8],
}

impl<'a> Decoder<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Decoder { bytes }
    }
    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.bytes.len() < n {
            return Err(DecodeError::Truncated);
        }
        let (head, tail) = self.bytes.split_at(n);
        self.bytes = tail;
        Ok(head)
    }
    pub(crate) fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }
    pub(crate) fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().expect("two bytes")))
    }
    pub(crate) fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("four bytes")))
    }
    pub(crate) fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("eight bytes")))
    }
    pub(crate) fn i64(&mut self) -> Result<i64, DecodeError> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().expect("eight bytes")))
    }
    pub(crate) fn bool(&mut self) -> Result<bool, DecodeError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(DecodeError::BadBool(other)),
        }
    }
    pub(crate) fn hash(&mut self) -> Result<[u8; 32], DecodeError> {
        Ok(self.take(32)?.try_into().expect("thirty-two bytes"))
    }
    /// A length-prefixed byte field, refused before allocation when longer than `max`.
    pub(crate) fn bytes(&mut self, what: &'static str, max: usize) -> Result<&'a [u8], DecodeError> {
        let len = self.u32()?;
        if len as usize > max {
            return Err(DecodeError::FieldTooLong { what, len, max });
        }
        self.take(len as usize)
    }
    pub(crate) fn string(&mut self, what: &'static str, max: usize) -> Result<String, DecodeError> {
        let bytes = self.bytes(what, max)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| DecodeError::BadString)
    }
    /// A `vec` count, refused before allocation when larger than `max` or than what could
    /// possibly fit in the remaining bytes (`min_item_size` bytes per item).
    pub(crate) fn count(&mut self, what: &'static str, max: usize, min_item_size: usize) -> Result<usize, DecodeError> {
        let count = self.u32()?;
        if count as usize > max {
            return Err(DecodeError::TooMany { what, count, max });
        }
        if (count as usize).saturating_mul(min_item_size) > self.bytes.len() {
            return Err(DecodeError::Truncated);
        }
        Ok(count as usize)
    }
    pub(crate) fn finish(self) -> Result<(), DecodeError> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err(DecodeError::TrailingBytes(self.bytes.len()))
        }
    }
}
