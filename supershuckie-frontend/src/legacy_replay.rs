//! Importing replays of the original Super Shuckie (SuperShuckie64, Game Boy / Game Boy Color
//! only), whose files start with `GameREC>` instead of this app's `NIDO`.
//!
//! An old file is a 512-byte header and a (usually zstd-compressed) stream of packets, each a
//! frame delay, a type and a payload: inputs, full SameBoy save states every 240 frames, resets,
//! and memory writes. Importing re-plays that stream on today's Game Boy core from the old file's
//! first save state while recording an ordinary replay. The old app ran a different SameBoy
//! version, which now and then makes the game take a slightly different path (a step lands a few
//! pixels later, the random number generator advances differently); at every old save state the
//! emulated memory is compared with it, and on a difference the old state is loaded (recorded as
//! a state load in the new replay), so the result never strays more than one old keyframe
//! interval from what was actually played.

use std::collections::HashMap;
use std::ffi::c_void;
use std::fs::File;
use std::io::{BufWriter, Read};
use std::num::NonZeroU64;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use supershuckie_core::emulator::{EmulatorCore, GameBoyColor, Input, Model, PartialReplayRecordMetadata};
use supershuckie_core::{MonotonicTimestampProvider, SuperShuckieCore, TimestampMicros};
use supershuckie_replay_recorder::replay_file::convert::ConvertError;
use supershuckie_replay_recorder::replay_file::record::{NullReplayFileSink, ReplayFileRecorderSettings};
use supershuckie_replay_recorder::replay_file::ReplayPatchFormat;
use supershuckie_replay_recorder::ByteVec;

/// First eight bytes of an old file.
pub const LEGACY_MAGIC: &[u8; 8] = b"GameREC>";

const HEADER_SIZE: usize = 512;

/// Game Boy frame length: 70224 cycles at 4194304 Hz (double speed runs twice the cycles).
const MICROS_PER_FRAME: f64 = 1_000_000.0 * 70224.0 / 4_194_304.0;

/// What the header of an old file says.
#[derive(Clone, Debug, PartialEq)]
pub struct LegacyHeader {
    /// The ROM's internal file name as the old app knew it.
    pub rom_name: String,
    /// SHA-256 of the ROM it was recorded with.
    pub rom_sha256: [u8; 32],
    /// Whether the packet stream is zstd-compressed.
    pub compressed: bool,
}

/// Whether `path` is an old file (by its first eight bytes).
pub fn is_legacy_replay(path: &Path) -> bool {
    let mut magic = [0u8; 8];
    File::open(path).and_then(|mut f| f.read_exact(&mut magic)).is_ok() && &magic == LEGACY_MAGIC
}

/// Read the header of an old file.
pub fn read_legacy_header(path: &Path) -> Result<LegacyHeader, String> {
    let mut header = [0u8; HEADER_SIZE];
    File::open(path)
        .and_then(|mut f| f.read_exact(&mut header))
        .map_err(|e| format!("cannot read the header of {}: {e}", path.display()))?;
    parse_header(&header).ok_or_else(|| format!("{} is not a replay of the original Super Shuckie", path.display()))
}

fn parse_header(header: &[u8; HEADER_SIZE]) -> Option<LegacyHeader> {
    if &header[0..8] != LEGACY_MAGIC || header[8..16] != [0; 8] {
        return None
    }
    let name = &header[32..64];
    let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())];
    Some(LegacyHeader {
        rom_name: String::from_utf8_lossy(name).into_owned(),
        rom_sha256: header[96..128].try_into().expect("32 bytes"),
        compressed: header[160] & 1 != 0,
    })
}

/// One packet of an old file, as far as importing cares.
#[derive(Debug, PartialEq)]
enum LegacyPacket {
    AddState { index: u32, data: Vec<u8> },
    LoadState { index: u32 },
    Input(u8),
    WriteMemory { bank: u16, address: u16, byte: u8 },
    Reset,
    /// Anything that does not change the emulation (speed changes, bookmarks, padding).
    Ignored,
}

/// Streaming zstd decompression over a reader.
struct ZstdReader<R: Read> {
    inner: R,
    stream: *mut zstd_sys::ZSTD_DStream,
    input: Vec<u8>,
    input_pos: usize,
    input_len: usize,
    finished_input: bool,
    /// What the last `ZSTD_decompressStream` returned: 0 once a frame is complete.
    frame_unfinished: bool,
}

impl<R: Read> ZstdReader<R> {
    fn new(inner: R) -> Self {
        // SAFETY: plain constructor; freed in Drop.
        let stream = unsafe { zstd_sys::ZSTD_createDStream() };
        assert!(!stream.is_null(), "ZSTD_createDStream failed");
        // SAFETY: valid stream.
        unsafe { zstd_sys::ZSTD_initDStream(stream) };
        Self { inner, stream, input: vec![0; 1 << 17], input_pos: 0, input_len: 0, finished_input: false, frame_unfinished: true }
    }
}

impl<R: Read> Read for ZstdReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0)
        }
        loop {
            if self.input_pos == self.input_len && !self.finished_input {
                self.input_len = self.inner.read(&mut self.input)?;
                self.input_pos = 0;
                self.finished_input = self.input_len == 0;
            }
            let mut output = zstd_sys::ZSTD_outBuffer { dst: buf.as_mut_ptr() as *mut c_void, size: buf.len(), pos: 0 };
            let mut input = zstd_sys::ZSTD_inBuffer {
                src: self.input[self.input_pos..].as_ptr() as *const c_void,
                size: self.input_len - self.input_pos,
                pos: 0,
            };
            // SAFETY: both buffers point at live memory of the stated sizes.
            let result = unsafe { zstd_sys::ZSTD_decompressStream(self.stream, &mut output, &mut input) };
            // SAFETY: plain function.
            if unsafe { zstd_sys::ZSTD_isError(result) } != 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "the replay's compressed data is damaged"))
            }
            self.input_pos += input.pos;
            if input.pos > 0 || output.pos > 0 {
                self.frame_unfinished = result != 0;
            }
            if output.pos > 0 {
                return Ok(output.pos)
            }
            if self.finished_input && self.input_pos == self.input_len {
                return if self.frame_unfinished {
                    Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the replay's compressed data is cut short"))
                }
                else {
                    Ok(0)
                }
            }
        }
    }
}

impl<R: Read> Drop for ZstdReader<R> {
    fn drop(&mut self) {
        // SAFETY: created in `new`, freed once.
        unsafe { zstd_sys::ZSTD_freeDStream(self.stream) };
    }
}

/// Reads packets from the stream after the header, tracking the frame each one applies at.
struct PacketReader<R: Read> {
    stream: R,
    frame: u64,
}

impl<R: Read> PacketReader<R> {
    fn bytes<const N: usize>(&mut self) -> std::io::Result<[u8; N]> {
        let mut b = [0u8; N];
        self.stream.read_exact(&mut b)?;
        Ok(b)
    }

    fn sized(&mut self, length_width: usize, limit: u64) -> std::io::Result<Vec<u8>> {
        let mut len = [0u8; 8];
        self.stream.read_exact(&mut len[8 - length_width..])?;
        let len = u64::from_be_bytes(len);
        if len > limit {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "a packet claims an implausible size"))
        }
        let mut data = vec![0u8; len as usize];
        self.stream.read_exact(&mut data)?;
        Ok(data)
    }

    /// The next packet and the frame it applies at; `None` at the end of the stream.
    fn next(&mut self) -> Result<Option<(u64, LegacyPacket)>, String> {
        let mut delay = [0u8; 1];
        match self.stream.read(&mut delay) {
            Ok(0) => return Ok(None),
            Ok(_) => (),
            Err(e) => return Err(e.to_string()),
        }
        self.frame += delay[0] as u64;
        let packet = self.packet().map_err(|e| match e.kind() {
            std::io::ErrorKind::UnexpectedEof => "the replay ends in the middle of a packet".to_owned(),
            _ => e.to_string(),
        })?;
        Ok(Some((self.frame, packet)))
    }

    fn packet(&mut self) -> std::io::Result<LegacyPacket> {
        const MAX_STATE: u64 = 64 << 20;
        let [kind] = self.bytes::<1>()?;
        Ok(match kind {
            // custom data: 32-byte name, u64-sized data
            0 => { self.bytes::<32>()?; self.sized(8, MAX_STATE)?; LegacyPacket::Ignored }
            // load SRAM (the first state carries it anyway)
            1 => { self.sized(8, MAX_STATE)?; LegacyPacket::Ignored }
            // game speed (pacing only)
            2 => { self.bytes::<2>()?; LegacyPacket::Ignored }
            // bookmark
            3 => { self.bytes::<32>()?; LegacyPacket::Ignored }
            4 => LegacyPacket::Input(self.bytes::<1>()?[0]),
            5 => { self.bytes::<2>()?; LegacyPacket::Ignored }
            6 => { self.bytes::<4>()?; LegacyPacket::Ignored }
            7 => { self.bytes::<8>()?; LegacyPacket::Ignored }
            8..=11 => { self.sized(1 << (kind - 8), MAX_STATE)?; LegacyPacket::Ignored }
            12 => {
                let data = self.sized(8, MAX_STATE)?;
                let index = u32::from_be_bytes(self.bytes::<4>()?);
                LegacyPacket::AddState { index, data }
            }
            13 => LegacyPacket::LoadState { index: u32::from_be_bytes(self.bytes::<4>()?) },
            14 => {
                let address = u32::from_be_bytes(self.bytes::<4>()?);
                let [byte] = self.bytes::<1>()?;
                LegacyPacket::WriteMemory { bank: (address >> 16) as u16, address: address as u16, byte }
            }
            15 => { self.bytes::<9>()?; LegacyPacket::Ignored }
            16 => { self.bytes::<5>()?; LegacyPacket::Ignored }
            17 => { self.bytes::<9>()?; LegacyPacket::Ignored }
            18 => LegacyPacket::Reset,
            255 => LegacyPacket::Ignored,
            other => return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("unknown packet type {other}"))),
        })
    }
}

fn open_packets(path: &Path) -> Result<(LegacyHeader, PacketReader<Box<dyn Read>>), String> {
    let header = read_legacy_header(path)?;
    let mut file = std::io::BufReader::with_capacity(1 << 20, File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?);
    let mut skip = [0u8; HEADER_SIZE];
    file.read_exact(&mut skip).map_err(|e| e.to_string())?;
    let stream: Box<dyn Read> = if header.compressed {
        Box::new(std::io::BufReader::with_capacity(1 << 20, ZstdReader::new(file)))
    }
    else {
        Box::new(file)
    };
    Ok((header, PacketReader { stream, frame: 0 }))
}

/// Number of frames in an old file (reads the whole stream).
fn count_frames(path: &Path, cancelled: &mut dyn FnMut() -> bool) -> Result<u64, ConvertError> {
    let (_, mut packets) = open_packets(path)?;
    let mut frames = 0;
    let mut n = 0u64;
    while let Some((frame, _)) = packets.next()? {
        frames = frame;
        n += 1;
        if n % 4096 == 0 && cancelled() {
            return Err(ConvertError::Cancelled)
        }
    }
    Ok(frames)
}

/// The old app's button mask is SameBoy's `GB_key_mask_t`.
fn input_from_mask(mask: u8) -> Input {
    Input {
        d_right: mask & 0x01 != 0,
        d_left: mask & 0x02 != 0,
        d_up: mask & 0x04 != 0,
        d_down: mask & 0x08 != 0,
        a: mask & 0x10 != 0,
        b: mask & 0x20 != 0,
        select: mask & 0x40 != 0,
        start: mask & 0x80 != 0,
        ..Input::default()
    }
}

/// Whether every memory region of `a` equals `b`'s.
fn same_memory(a: &dyn EmulatorCore, b: &dyn EmulatorCore) -> bool {
    (0..a.memory_regions().len()).all(|i| a.memory_region_data(i) == b.memory_region_data(i))
}

/// The recording's clock: game time, not wall time.
struct FrameClock(Arc<AtomicU64>);

impl MonotonicTimestampProvider for FrameClock {
    fn get_timestamp_microseconds(&mut self) -> TimestampMicros {
        self.0.load(Ordering::Relaxed)
    }
}

/// Where an import should look for its parts.
pub struct LegacyImportOptions<'a> {
    /// The ROM the old file was recorded with (its SHA-256 must match the header).
    pub rom: &'a [u8],
    /// Its file name, stored in the new replay like a recording of it would.
    pub rom_filename: String,
    /// The Game Boy Color boot ROM a recording gets.
    pub bios: &'a [u8],
    pub settings: ReplayFileRecorderSettings,
    pub frames_per_keyframe: NonZeroU64,
}

/// Result of a successful import.
#[derive(Clone, Debug, PartialEq)]
pub struct LegacyImportReport {
    pub frames: u64,
    /// Old save states compared with the re-played game.
    pub checks: u64,
    /// How many of them differed (and were loaded).
    pub resyncs: u64,
}

/// Import the old file `input` into a new replay at `output`. `progress(done, total)` is called
/// now and then and returns `false` to cancel.
pub fn import_legacy_replay(
    input: &Path,
    output: &Path,
    options: &LegacyImportOptions<'_>,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<LegacyImportReport, ConvertError> {
    let header = read_legacy_header(input)?;
    if sha256(options.rom) != header.rom_sha256 {
        return Err(ConvertError::Failed("the ROM given is not the game this replay was recorded with".into()))
    }

    let total = count_frames(input, &mut || !progress(0, 0))?;
    let (_, mut packets) = open_packets(input)?;

    let clock = Arc::new(AtomicU64::new(0));
    let game = GameBoyColor::new_from_rom(options.rom, options.bios, None, Model::Cgb0);
    let mut core = SuperShuckieCore::new(Box::new(game), Box::new(FrameClock(clock.clone())));
    let mut probe = GameBoyColor::new_from_rom(options.rom, options.bios, None, Model::Cgb0);
    let mut states: HashMap<u32, Vec<u8>> = HashMap::new();

    // Everything at frame 0 (the old app's starting state and its load) happens before the
    // recording starts, so the new replay starts from that state.
    let mut pending = None;
    let mut input_mask = 0u8;
    let mut started = false;
    while let Some((frame, packet)) = packets.next()? {
        if frame > 0 {
            pending = Some((frame, packet));
            break
        }
        match packet {
            LegacyPacket::AddState { index, data } => { states.insert(index, data); }
            LegacyPacket::LoadState { index } => {
                let state = states.get(&index).ok_or_else(|| "the replay loads a state it never stored".to_owned())?;
                probe.load_save_state(state).map_err(|e| format!("the replay's first save state does not load: {e}"))?;
                core.load_save_state(state);
                started = true;
            }
            LegacyPacket::Input(mask) => input_mask = mask,
            _ => {}
        }
    }
    if !started {
        return Err(ConvertError::Failed("the replay does not start from a save state".into()))
    }
    core.enqueue_input(input_from_mask(input_mask));

    let file = File::create(output).map_err(|e| format!("cannot create {}: {e}", output.display()))?;
    core.start_recording_replay(PartialReplayRecordMetadata {
        rom_name: options.rom_filename.clone(),
        rom_filename: options.rom_filename.clone(),
        settings: options.settings.clone(),
        patch_format: ReplayPatchFormat::Unpatched,
        patch_target_checksum: Default::default(),
        patch_data: ByteVec::new(),
        frames_per_keyframe: options.frames_per_keyframe,
        final_file: BufWriter::with_capacity(8 << 20, file),
        temp_file: NullReplayFileSink,
    }).map_err(|e| format!("cannot start the new replay: {e:?}"))?;

    let mut report = LegacyImportReport { frames: 0, checks: 0, resyncs: 0 };
    let result = (|| -> Result<(), ConvertError> {
        let mut frame = 0u64;
        loop {
            // The packets of this frame boundary, in the order the old app wrote them.
            while let Some((at, packet)) = pending.take().map(Ok).or_else(|| packets.next().transpose()).transpose()? {
                if at > frame {
                    pending = Some((at, packet));
                    break
                }
                match packet {
                    LegacyPacket::AddState { index, data } => {
                        report.checks += 1;
                        probe.load_save_state(&data).map_err(|e| format!("a save state at frame {frame} does not load: {e}"))?;
                        if !same_memory(core.get_core(), &probe) {
                            report.resyncs += 1;
                            core.load_save_state(&data);
                        }
                        states.insert(index, data);
                    }
                    LegacyPacket::LoadState { index } => {
                        let state = states.get(&index).ok_or_else(|| "the replay loads a state it never stored".to_owned())?;
                        core.load_save_state(state);
                    }
                    LegacyPacket::Input(mask) => core.enqueue_input(input_from_mask(mask)),
                    LegacyPacket::WriteMemory { bank, address, byte } => {
                        // Only the banks this app addresses the same way; anything else is put
                        // right by the next save state comparison.
                        if bank <= 1 || !(0xD000..0xE000).contains(&address) {
                            core.enqueue_write(address as u32, ByteVec::from(&[byte][..]));
                        }
                    }
                    LegacyPacket::Reset => core.hard_reset(),
                    LegacyPacket::Ignored => {}
                }
            }
            if frame >= total {
                break
            }

            let before = core.total_frames();
            while core.total_frames() == before || core.is_mid_frame() {
                core.run_unlocked_hidden();
            }
            frame += 1;
            clock.store((frame as f64 * MICROS_PER_FRAME) as u64, Ordering::Relaxed);
            if frame % 2048 == 0 && !progress(frame, total) {
                return Err(ConvertError::Cancelled)
            }
        }
        report.frames = frame;
        Ok(())
    })();

    let closed = core.stop_recording_replay();
    if let Err(e) = result {
        let _ = std::fs::remove_file(output);
        return Err(e)
    }
    if closed != Some(true) {
        let _ = std::fs::remove_file(output);
        return Err(ConvertError::Failed("the new replay could not be finished".into()))
    }
    Ok(report)
}

/// SHA-256 (the old app's ROM checksum).
pub fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];

    let mut tail = data[data.len() / 64 * 64..].to_vec();
    tail.push(0x80);
    while tail.len() % 64 != 56 {
        tail.push(0);
    }
    tail.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());

    for block in data[..data.len() / 64 * 64].chunks_exact(64).chain(tail.chunks_exact(64)) {
        let mut w = [0u32; 64];
        for (i, word) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes(word.try_into().expect("4 bytes"));
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(y);
        }
    }

    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_answers() {
        let hex = |d: [u8; 32]| d.iter().map(|b| format!("{b:02x}")).collect::<String>();
        assert_eq!(hex(sha256(b"")), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(hex(sha256(b"abc")), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        let million_a = vec![b'a'; 1_000_000];
        assert_eq!(hex(sha256(&million_a)), "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0");
        // 55/56/64-byte edge cases of the padding.
        assert_eq!(hex(sha256(&[b'a'; 55])), "9f4390f8d30c2dd92ec9f095b65e2b9ae9b0a925a5258e241c9f1e910f734318");
        assert_eq!(hex(sha256(&[b'a'; 56])), "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a");
        assert_eq!(hex(sha256(&[b'a'; 64])), "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb");
    }

    /// A small old file: header, an initial state + load, inputs, a write, a reset, a second
    /// state and a speed change, compressed and not.
    fn legacy_file(compressed: bool) -> Vec<u8> {
        let mut body = Vec::new();
        let state = vec![7u8; 40];
        // frame 0: AddSaveState(0), LoadSaveState(0), ChangeGameSpeed
        body.extend_from_slice(&[0, 12]);
        body.extend_from_slice(&(state.len() as u64).to_be_bytes());
        body.extend_from_slice(&state);
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&[0, 13, 0, 0, 0, 0]);
        body.extend_from_slice(&[0, 2, 1, 0]);
        // frame 3: input 0x21; frame 5: write 0x01C123 = 9; frame 260: no-op chain + reset
        body.extend_from_slice(&[3, 4, 0x21]);
        body.extend_from_slice(&[2, 14, 0x00, 0x01, 0xC1, 0x23, 9]);
        body.extend_from_slice(&[255, 255, 0, 18]);
        // frame 300: state 1
        body.extend_from_slice(&[40, 12]);
        body.extend_from_slice(&(3u64).to_be_bytes());
        body.extend_from_slice(&[1, 2, 3]);
        body.extend_from_slice(&1u32.to_be_bytes());

        let mut header = vec![0u8; HEADER_SIZE];
        header[..8].copy_from_slice(LEGACY_MAGIC);
        header[32..41].copy_from_slice(b"TEST ROM\0");
        header[96..128].copy_from_slice(&sha256(b"rom"));
        header[160] = compressed as u8;
        if compressed {
            let mut out = vec![0u8; 1024];
            // SAFETY: buffers are valid for their lengths.
            let n = unsafe { zstd_sys::ZSTD_compress(out.as_mut_ptr() as *mut c_void, out.len(), body.as_ptr() as *const c_void, body.len(), 3) };
            assert_eq!(unsafe { zstd_sys::ZSTD_isError(n) }, 0);
            out.truncate(n);
            body = out;
        }
        header.extend_from_slice(&body);
        header
    }

    #[test]
    fn reads_headers_and_packets() {
        let dir = std::env::temp_dir().join(format!("supershuckie-legacy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for compressed in [false, true] {
            let path = dir.join(format!("old-{compressed}.replay"));
            std::fs::write(&path, legacy_file(compressed)).unwrap();
            assert!(is_legacy_replay(&path));
            let header = read_legacy_header(&path).unwrap();
            assert_eq!(header, LegacyHeader { rom_name: "TEST ROM".into(), rom_sha256: sha256(b"rom"), compressed });

            let (_, mut packets) = open_packets(&path).unwrap();
            let mut all = Vec::new();
            while let Some(p) = packets.next().unwrap() {
                all.push(p);
            }
            assert_eq!(all, vec![
                (0, LegacyPacket::AddState { index: 0, data: vec![7; 40] }),
                (0, LegacyPacket::LoadState { index: 0 }),
                (0, LegacyPacket::Ignored),
                (3, LegacyPacket::Input(0x21)),
                (5, LegacyPacket::WriteMemory { bank: 1, address: 0xC123, byte: 9 }),
                (260, LegacyPacket::Ignored),
                (260, LegacyPacket::Reset),
                (300, LegacyPacket::AddState { index: 1, data: vec![1, 2, 3] }),
            ]);
            assert_eq!(count_frames(&path, &mut || false).unwrap(), 300);

            // Cut short: an error, not a panic.
            let bytes = legacy_file(compressed);
            std::fs::write(&path, &bytes[..bytes.len() - 2]).unwrap();
            let (_, mut packets) = open_packets(&path).unwrap();
            let mut error = None;
            loop {
                match packets.next() {
                    Ok(Some(_)) => continue,
                    Ok(None) => break,
                    Err(e) => { error = Some(e); break }
                }
            }
            assert!(error.is_some(), "a truncated file must fail (compressed: {compressed})");
        }
        let not_legacy = dir.join("new.replay");
        std::fs::write(&not_legacy, b"NIDO....").unwrap();
        assert!(!is_legacy_replay(&not_legacy));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
