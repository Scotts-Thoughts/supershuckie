//! The serve loop: requests in on stdin, replies out on stdout, one core on this thread.
//!
//! A reader thread parses stdin into a channel so the emulation loop can notice a newer request
//! (or a `Cancel`) part way through a walk and abandon it. Everything else happens here.
//!
//! Stdout is the wire, so nothing else in the process may write to it. The cores' C glue prints
//! to stdout when it cannot start ("Bad BIOS", "Failed to init mGBA") and then terminates; served
//! as-is that line would corrupt the stream and never reach anyone. [`claim_stdout`] takes the
//! pipe for the protocol and points descriptor 1 at stderr before the first core is built, so
//! such a line lands in the log the client keeps, and is what the user reads.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, BufWriter};
use std::path::Path;
use std::process::ExitCode;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;

use supershuckie_core::emulator::{EmulatorCore, GameBoyAdvance, GameBoyColor, Model, NintendoDS, AUDIO_SAMPLE_RATE};
use supershuckie_core::export::{composite_screens, output_geometry};
use supershuckie_core::{std_timestamp_provider, AudioOutput, ScreenLayout, SuperShuckieCore};
use supershuckie_replay_recorder::blake3_hash;
use supershuckie_replay_recorder::replay_file::ReplayConsoleType;

use crate::protocol::{write_all_flushed, Info, Reply, Request, MAX_MEMORY_BYTES, PROTOCOL_VERSION};
use crate::source::{console_names, geometry, layout_from_byte, open_replay, ReplaySummary};

/// A `Frame` this far ahead of the current position (or nearer) is reached by stepping rather
/// than by loading a keyframe: two keyframe intervals, so a walk that has just passed a keyframe
/// does not throw the frames it already emulated away.
const STEP_LIMIT: u64 = 240;

/// The channel is checked for a superseding request every this many hidden runs during a walk.
const POLL_EVERY: u32 = 8;

/// Human-readable server name for `Hello`.
pub fn server_name() -> String {
    format!("supershuckie-frame-server {}", env!("CARGO_PKG_VERSION"))
}

/// What the reader thread delivers.
enum Inbound {
    Request(Request),
    /// A message that was framed correctly but did not decode.
    Malformed(String),
    /// Stdin ended (cleanly or not).
    End,
}

/// Why a walk stopped short of its target.
enum Stop {
    /// A newer `Frame`/`Run`, a `Cancel` for this id, or `Close` arrived.
    Superseded,
    /// The replay or the core could not reach the frame.
    Failed(String),
}

/// One opened (ROM, replay) pair.
struct Session {
    /// `None` when the ROM did not match: `Info` was still answered, but nothing can be served.
    core: Option<SuperShuckieCore>,
    layout: ScreenLayout,
    /// Pictures in the source.
    frames: u64,
    fps: (u32, u32),
    /// `total_frames()` whose picture the screens currently hold, if they hold a drawn one.
    drawn: Option<u64>,
    audio: Option<Arc<AudioOutput>>,
}

struct Server {
    rx: Receiver<Inbound>,
    out: BufWriter<File>,
    /// Requests taken off the channel while polling, still to be handled in arrival order.
    pending: VecDeque<Request>,
    session: Option<Session>,
    frame_words: Vec<u32>,
    reply_bytes: Vec<u8>,
}

/// The pipe replies go down, taken away from everything else in the process.
///
/// A duplicate of stdout's handle for the protocol, then descriptor 1 is made a second stderr:
/// this redirects the C runtime's descriptor 1 — where the cores' C glue's `printf` goes — so
/// that output lands in the log from here on rather than into the middle of a frame. It does not
/// move Rust's own stdout (on Windows, `_dup2` on descriptor 1 does not touch the Win32 handle
/// Rust's stdout writes through), so Rust code in the server must use `eprintln!` instead, which
/// it does.
fn claim_stdout() -> io::Result<File> {
    #[cfg(unix)]
    let protocol = {
        use std::os::fd::AsFd;
        File::from(io::stdout().as_fd().try_clone_to_owned()?)
    };
    #[cfg(windows)]
    let protocol = {
        use std::os::windows::io::AsHandle;
        File::from(io::stdout().as_handle().try_clone_to_owned()?)
    };
    // SAFETY: `dup2` on the two standard descriptors, both of which this process owns for its
    // whole life; nothing holds a borrowed handle to descriptor 1 across this call.
    if unsafe { libc::dup2(2, 1) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(protocol)
}

/// Run the server on this process's stdin/stdout until `Close` or end of input.
pub fn serve() -> ExitCode {
    let protocol = match claim_stdout() {
        Ok(file) => file,
        Err(e) => {
            eprintln!("frame-server: cannot claim stdout for the protocol: {e}");
            return ExitCode::from(1);
        }
    };
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("stdin reader".into())
        .spawn(move || {
            let stdin = io::stdin();
            let mut stdin = stdin.lock();
            loop {
                let item = match Request::read(&mut stdin) {
                    Ok(Some(Ok(request))) => Inbound::Request(request),
                    Ok(Some(Err(e))) => Inbound::Malformed(e.to_string()),
                    Ok(None) => Inbound::End,
                    Err(e) => {
                        eprintln!("frame-server: stdin: {e}");
                        Inbound::End
                    }
                };
                let end = matches!(item, Inbound::End);
                if tx.send(item).is_err() || end {
                    break;
                }
            }
        })
        .expect("spawn stdin reader");

    let mut server = Server {
        rx,
        out: BufWriter::with_capacity(1 << 20, protocol),
        pending: VecDeque::new(),
        session: None,
        frame_words: Vec::new(),
        reply_bytes: Vec::new(),
    };
    match server.run_loop() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("frame-server: stdout: {e}");
            ExitCode::from(1)
        }
    }
}

impl Server {
    fn run_loop(&mut self) -> io::Result<ExitCode> {
        loop {
            let Some(request) = self.next_request()? else {
                return Ok(ExitCode::SUCCESS);
            };
            match request {
                Request::Hello { protocol } => {
                    if protocol != PROTOCOL_VERSION {
                        self.reply(&Reply::Error {
                            id: 0,
                            message: format!("this server speaks protocol {PROTOCOL_VERSION}, the client asked for {protocol}"),
                        })?;
                        return Ok(ExitCode::from(1));
                    }
                    self.reply(&Reply::Hello { protocol: PROTOCOL_VERSION, server: server_name(), cores: console_names() })?;
                }
                Request::Open { rom, replay, layout, audio } => {
                    match self.open(Path::new(&rom), Path::new(&replay), layout, audio) {
                        Ok(info) => self.reply(&Reply::Info(info))?,
                        Err(message) => {
                            eprintln!("frame-server: open: {message}");
                            self.reply(&Reply::Error { id: 0, message })?
                        }
                    }
                }
                Request::Frame { id, index } => self.frame(id, index)?,
                Request::Run { id, from, to } => self.run(id, from, to)?,
                Request::Audio { id, first_frame, frames } => self.audio(id, first_frame, frames)?,
                Request::Memory { id, index, blocks } => self.memory(id, index, &blocks)?,
                // A `Cancel` reaching here names a request that is no longer pending.
                Request::Cancel { .. } => {}
                Request::Close => return Ok(ExitCode::SUCCESS),
            }
        }
    }

    /// The next request to handle: what polling already took off the channel first, then a
    /// blocking read. `None` once stdin has ended.
    fn next_request(&mut self) -> io::Result<Option<Request>> {
        loop {
            if let Some(request) = self.pending.pop_front() {
                return Ok(Some(request));
            }
            match self.rx.recv() {
                Ok(Inbound::Request(request)) => return Ok(Some(request)),
                Ok(Inbound::Malformed(message)) => self.reply(&Reply::Error { id: 0, message })?,
                Ok(Inbound::End) | Err(_) => return Ok(None),
            }
        }
    }

    /// Take everything that has arrived off the channel, and say whether the request `id` that
    /// is being worked on has been superseded by any of it.
    fn poll(&mut self, id: u32) -> io::Result<bool> {
        loop {
            match self.rx.try_recv() {
                Ok(Inbound::Request(request)) => self.pending.push_back(request),
                Ok(Inbound::Malformed(message)) => self.reply(&Reply::Error { id: 0, message })?,
                Ok(Inbound::End) => {
                    // Treat the end of input like a Close so the walk stops and the loop exits.
                    self.pending.push_back(Request::Close);
                    break;
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        Ok(self.pending.iter().any(|r| match r {
            Request::Frame { .. } | Request::Run { .. } | Request::Memory { .. } | Request::Close => true,
            Request::Cancel { id: cancelled } => *cancelled == id,
            _ => false,
        }))
    }

    fn reply(&mut self, reply: &Reply) -> io::Result<()> {
        self.reply_bytes.clear();
        reply.encode(&mut self.reply_bytes);
        write_all_flushed(&mut self.out, &self.reply_bytes)
    }

    // -----------------------------------------------------------------------------------------
    // Open

    fn open(&mut self, rom: &Path, replay: &Path, layout: u8, audio: bool) -> Result<Info, String> {
        self.session = None;

        let rom_bytes = std::fs::read(rom).map_err(|e| format!("cannot read ROM {}: {e}", rom.display()))?;
        if rom_bytes.is_empty() {
            return Err(format!("ROM {} is empty", rom.display()));
        }
        let mut player = open_replay(replay)?;
        let summary = ReplaySummary::of(&player);
        let layout = layout_from_byte(layout);
        let rom_ok = blake3_hash(&rom_bytes) == summary.rom_checksum;

        let mut info = Info {
            console: summary.console.name().to_string(),
            frames: summary.frames,
            sample_rate: AUDIO_SAMPLE_RATE,
            rom_ok,
            core_recorded: summary.core_recorded.clone(),
            keyframes: summary.keyframes.clone(),
            bookmarks: summary.bookmarks.clone(),
            crop: summary.crop,
            counters: summary.counters.clone(),
            ..Info::default()
        };
        (info.fps_num, info.fps_den) = crate::source::frame_rate(summary.console);
        (info.width, info.height) = geometry(summary.console, layout);

        let core = if rom_ok {
            let emulator = build_core(summary.console, &rom_bytes)?;
            info.core_running = emulator.core_name().to_string();
            info.fps_num = emulator.frame_rate().0;
            info.fps_den = emulator.frame_rate().1;
            let mut core = SuperShuckieCore::new(emulator, std_timestamp_provider());
            core.set_audio_enabled(audio);
            // Samples are wanted per emulated frame at 1x whatever speed the recording was made
            // at; the recorded speed only ever paced the original session.
            core.set_audio_mute_when_sped_up(false);
            core.set_ignore_speed_changes_in_replays(true);
            player.enable_threading();
            core.attach_replay_player(player, true).map_err(|e| format!("cannot attach the replay: {e:?}"))?;
            if let Some((w, h)) = output_geometry(core.get_core().get_screens(), layout) {
                (info.width, info.height) = (w, h);
            }
            Some(core)
        } else {
            eprintln!(
                "frame-server: ROM {} does not match the recording ({} was recorded with blake3 {})",
                rom.display(),
                summary.rom_filename,
                summary.rom_hash_hex()
            );
            None
        };

        // Attaching runs the first frame, so the screens hold frame 1 (which is also frame 0).
        let drawn = core.as_ref().map(|c| c.total_frames());
        self.session = Some(Session {
            core,
            layout,
            frames: summary.frames,
            fps: (info.fps_num, info.fps_den),
            drawn,
            audio: None,
        });
        Ok(info)
    }

    // -----------------------------------------------------------------------------------------
    // Frame / Run / Audio

    /// The open session with a usable core, or the message to refuse the request with.
    fn usable(&self) -> Result<(), String> {
        match &self.session {
            None => Err("no source is open (send Open first)".into()),
            Some(Session { core: None, .. }) => Err("the ROM does not match the recording".into()),
            Some(_) => Ok(()),
        }
    }

    fn frame(&mut self, id: u32, index: u64) -> io::Result<()> {
        if let Err(message) = self.usable() {
            return self.reply(&Reply::Error { id, message });
        }
        if self.poll(id)? {
            return self.reply(&Reply::Cancelled { id });
        }
        let frames = self.session.as_ref().map_or(0, |s| s.frames);
        if index >= frames {
            return self.reply(&Reply::Error { id, message: format!("frame {index} is past the end ({frames} frames)") });
        }
        match self.position_drawn(id, picture_of(index)) {
            Ok(()) => self.send_frame(id, index),
            Err(Stop::Superseded) => self.reply(&Reply::Cancelled { id }),
            Err(Stop::Failed(message)) => self.reply(&Reply::Error { id, message }),
        }
    }

    fn run(&mut self, id: u32, from: u64, to: u64) -> io::Result<()> {
        if let Err(message) = self.usable() {
            return self.reply(&Reply::Error { id, message });
        }
        if self.poll(id)? {
            return self.reply(&Reply::Cancelled { id });
        }
        let frames = self.session.as_ref().map_or(0, |s| s.frames);
        if to > frames || from > to {
            return self.reply(&Reply::Error { id, message: format!("range {from}..{to} is not inside the {frames} frames") });
        }
        for index in from..to {
            match self.position_drawn(id, picture_of(index)) {
                Ok(()) => self.send_frame(id, index)?,
                Err(Stop::Superseded) => return self.reply(&Reply::Cancelled { id }),
                Err(Stop::Failed(message)) => return self.reply(&Reply::Error { id, message }),
            }
            if self.poll(id)? {
                return self.reply(&Reply::Cancelled { id });
            }
        }
        self.reply(&Reply::Done { id })
    }

    fn audio(&mut self, id: u32, first_frame: u64, frames: u32) -> io::Result<()> {
        if let Err(message) = self.usable() {
            return self.reply(&Reply::Error { id, message });
        }
        if self.poll(id)? {
            return self.reply(&Reply::Cancelled { id });
        }
        let total = self.session.as_ref().map_or(0, |s| s.frames);
        if first_frame >= total || u64::from(frames) > total - first_frame {
            return self.reply(&Reply::Error {
                id,
                message: format!("audio range {first_frame}+{frames} is not inside the {total} frames"),
            });
        }

        // The sound of picture `i` is what the run that produced picture `i` emitted, so start
        // from the position just before it.
        let start = picture_of(first_frame) - 1;
        if let Err(stop) = self.position_hidden(id, start) {
            return match stop {
                Stop::Superseded => self.reply(&Reply::Cancelled { id }),
                Stop::Failed(message) => self.reply(&Reply::Error { id, message }),
            };
        }

        let session = self.session.as_mut().expect("checked by usable()");
        let core = session.core.as_mut().expect("checked by usable()");
        if !core.audio_enabled() {
            core.set_audio_enabled(true);
        }
        let ring = match &session.audio {
            Some(ring) => ring.clone(),
            None => {
                let ring = Arc::new(AudioOutput::new(1000));
                core.set_audio_output(Some(ring.clone()));
                session.audio = Some(ring.clone());
                ring
            }
        };
        // Room for the whole stretch plus a second of slack; the ring drops the oldest samples
        // when it is full, which must never happen here.
        let (fps_num, fps_den) = session.fps;
        let millis = (u64::from(frames) * 1000 * u64::from(fps_den)).div_ceil(u64::from(fps_num.max(1))) + 1000;
        ring.set_max_latency_ms(u32::try_from(millis).unwrap_or(u32::MAX));
        ring.clear();

        let end = start + u64::from(frames);
        let mut runs = 0u32;
        let result: Result<(), Stop> = loop {
            let session = self.session.as_mut().expect("checked by usable()");
            let core = session.core.as_mut().expect("checked by usable()");
            if core.total_frames() >= end {
                break Ok(());
            }
            if core.is_replay_stalled() {
                break Err(Stop::Failed(format!("the replay ended at frame {}", core.total_frames())));
            }
            core.run_unlocked_audible();
            session.drawn = core.last_frame_presented().then(|| core.total_frames());
            runs += 1;
            if runs % POLL_EVERY == 0 && self.poll(id)? {
                break Err(Stop::Superseded);
            }
        };

        match result {
            Ok(()) => {
                let mut samples = vec![0i16; ring.queued_frames() * 2];
                let got = ring.read(&mut samples);
                samples.truncate(got * 2);
                self.reply(&Reply::Audio { id, first_frame, frames, samples })
            }
            Err(Stop::Superseded) => self.reply(&Reply::Cancelled { id }),
            Err(Stop::Failed(message)) => self.reply(&Reply::Error { id, message }),
        }
    }

    /// The state behind picture `index`: each block read from the core's memory once the picture
    /// has been produced, concatenated in the order asked. Positioning is exactly a `Frame`'s, so
    /// a `Frame` for the same picture afterwards costs nothing; a block the core cannot read is
    /// answered with zeros, as the live tool integration leaves it, since a tool's block list is
    /// written for the game and not for the emulator.
    fn memory(&mut self, id: u32, index: u64, blocks: &[(u32, u32)]) -> io::Result<()> {
        if let Err(message) = self.usable() {
            return self.reply(&Reply::Error { id, message });
        }
        if self.poll(id)? {
            return self.reply(&Reply::Cancelled { id });
        }
        let frames = self.session.as_ref().map_or(0, |s| s.frames);
        if index >= frames {
            return self.reply(&Reply::Error { id, message: format!("frame {index} is past the end ({frames} frames)") });
        }
        let total: usize = blocks.iter().map(|(_, len)| *len as usize).sum();
        if total > MAX_MEMORY_BYTES {
            return self.reply(&Reply::Error { id, message: format!("memory request of {total} bytes is more than {MAX_MEMORY_BYTES}") });
        }
        match self.position_drawn(id, picture_of(index)) {
            Ok(()) => {
                let core = self.core().get_core();
                let mut data = Vec::with_capacity(total);
                for (address, length) in blocks {
                    let start = data.len();
                    data.resize(start + *length as usize, 0);
                    if core.read_ram(*address, &mut data[start..]).is_err() {
                        data[start..].fill(0);
                    }
                }
                self.reply(&Reply::Memory { id, index, data })
            }
            Err(Stop::Superseded) => self.reply(&Reply::Cancelled { id }),
            Err(Stop::Failed(message)) => self.reply(&Reply::Error { id, message }),
        }
    }

    /// Composite the screens and send them as picture `index` of request `id`.
    fn send_frame(&mut self, id: u32, index: u64) -> io::Result<()> {
        let session = self.session.as_ref().expect("checked by usable()");
        let core = session.core.as_ref().expect("checked by usable()");
        composite_screens(core.get_core().get_screens(), session.layout, &mut self.frame_words);
        let mut pixels = Vec::with_capacity(self.frame_words.len() * 4);
        for word in &self.frame_words {
            pixels.extend_from_slice(&word.to_le_bytes());
        }
        self.reply(&Reply::Frame { id, index, pixels })
    }

    // -----------------------------------------------------------------------------------------
    // Positioning

    /// Make the screens hold the picture after `target` emulated frames (`target >= 1`), doing as
    /// little as the seek policy allows: nothing if they already do, hidden steps when the target
    /// is a short way ahead, otherwise a keyframe load and hidden steps.
    fn position_drawn(&mut self, id: u32, target: u64) -> Result<(), Stop> {
        debug_assert!(target >= 1);
        let (cur, drawn) = {
            let session = self.session.as_ref().expect("checked by usable()");
            (session.core.as_ref().expect("checked by usable()").total_frames(), session.drawn)
        };
        if cur == target && drawn == Some(target) {
            return Ok(());
        }
        if !(cur < target && target - cur <= STEP_LIMIT) {
            self.load_keyframe_before(target)?;
        }
        self.step_hidden_to(id, target - 1)?;

        let session = self.session.as_mut().expect("checked by usable()");
        let core = session.core.as_mut().expect("checked by usable()");
        session.drawn = None;
        while core.total_frames() < target {
            if core.is_replay_stalled() {
                return Err(Stop::Failed(format!("the replay ended at frame {}", core.total_frames())));
            }
            core.run_unlocked();
        }
        if !core.last_frame_presented() {
            return Err(Stop::Failed(format!("the core did not draw frame {target}")));
        }
        session.drawn = Some(target);
        Ok(())
    }

    /// Bring the core to exactly `target` emulated frames without drawing anything.
    fn position_hidden(&mut self, id: u32, target: u64) -> Result<(), Stop> {
        let cur = self.core().total_frames();
        if cur == target {
            return Ok(());
        }
        if !(cur < target && target - cur <= STEP_LIMIT) {
            self.load_keyframe_before(target)?;
        }
        self.step_hidden_to(id, target)
    }

    /// Load the keyframe `go_to_replay_frame` would for a seek to `target`: at least
    /// `POST_LOAD_FRAMES` before it so regenerated buffers are rebuilt before anything is shown.
    fn load_keyframe_before(&mut self, target: u64) -> Result<(), Stop> {
        let session = self.session.as_mut().expect("checked by usable()");
        session.drawn = None;
        let core = session.core.as_mut().expect("checked by usable()");
        core.go_to_replay_keyframe(target.saturating_sub(SuperShuckieCore::POST_LOAD_FRAMES))
            .map(|_| ())
            .map_err(Stop::Failed)
    }

    /// Run hidden frames until `total_frames() == target` (a no-op when already past it), checking
    /// for a superseding request every few runs.
    fn step_hidden_to(&mut self, id: u32, target: u64) -> Result<(), Stop> {
        let mut runs = 0u32;
        loop {
            let session = self.session.as_mut().expect("checked by usable()");
            let core = session.core.as_mut().expect("checked by usable()");
            if core.total_frames() >= target {
                return Ok(());
            }
            if core.is_replay_stalled() {
                return Err(Stop::Failed(format!("the replay ended at frame {}", core.total_frames())));
            }
            session.drawn = None;
            core.run_unlocked_hidden();
            runs += 1;
            if runs % POLL_EVERY == 0 && self.poll(id).map_err(|e| Stop::Failed(e.to_string()))? {
                return Err(Stop::Superseded);
            }
        }
    }

    fn core(&self) -> &SuperShuckieCore {
        self.session.as_ref().and_then(|s| s.core.as_ref()).expect("checked by usable()")
    }
}

/// How many frames must have been emulated for the screens to hold picture `index`.
///
/// Picture 0 is the first frame ever drawn, which is the picture after one run: the same one the
/// emulator's own export produces for a range starting at 0 (`go_to_replay_frame(0)` runs one
/// frame). Every later picture is the one after exactly that many runs.
fn picture_of(index: u64) -> u64 {
    index.max(1)
}

/// The core the frontend would build for `console`, with the embedded boot ROMs and the settings
/// a replay needs (no SRAM, no JIT).
fn build_core(console: ReplayConsoleType, rom: &[u8]) -> Result<Box<dyn EmulatorCore>, String> {
    const DMG_BOOT: &[u8] = include_bytes!("../../bootrom/dmg/dmg.bin");
    const CGB_BOOT: &[u8] = include_bytes!("../../bootrom/cgb/cgb_boot/cgb_boot_fast.bin");
    const GBA_BIOS: &[u8] = include_bytes!("../../bootrom/agb/gba_bios.bin");

    Ok(match console {
        ReplayConsoleType::GameBoy => Box::new(GameBoyColor::new_from_rom(rom, DMG_BOOT, None, Model::DmgB)),
        ReplayConsoleType::SuperGameBoy2 => Box::new(GameBoyColor::new_from_rom(rom, DMG_BOOT, None, Model::Sgb2)),
        ReplayConsoleType::GameBoyColor => Box::new(GameBoyColor::new_from_rom(rom, CGB_BOOT, None, Model::Cgb0)),
        ReplayConsoleType::GameBoyAdvance => Box::new(GameBoyAdvance::new_from_rom(rom, None, GBA_BIOS, std_timestamp_provider())?),
        // The JIT is not reproducible; recordings are only bit-exact under the interpreter.
        ReplayConsoleType::NintendoDS => Box::new(NintendoDS::new_from_rom(rom, None, std_timestamp_provider(), false)?),
        ReplayConsoleType::Unknown => return Err("the recording's console type is unknown".into()),
    })
}
