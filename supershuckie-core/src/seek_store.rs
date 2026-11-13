//! Keeping a replay's seek index (see `seek_index`) in a file between sessions.
//!
//! The index is a cache: everything in it can be made again by emulating the replay, which is
//! what the background emulators of `seek_prefill` do. But that takes minutes for a long
//! recording, every time it is opened and in every program that opens it (the video editor runs
//! several frame servers per recording). So the index is written to a file once it is filled and
//! read back the next time, by whoever opens the same recording with the same build.
//!
//! The file belongs outside any synced folder ([`default_replay_seek_index_directory`]): it is
//! as large as the index (tens of megabytes an hour of recording), changes whenever the index
//! does, and is worth nothing on another machine's build. Its name is a fingerprint of the
//! recording and the game ([`ReplaySeekIndexFile::in_directory`]); the directory is kept under
//! a size limit by deleting the files used longest ago.
//!
//! Several programs may have the same recording open (the app, and the editor's frame servers).
//! Only one of them should spend processor time filling the index: whoever holds the file's
//! [fill lock](ReplaySeekIndexFile::try_lock_for_filling). The others read the file again when
//! it changes ([`TrackedFile::changed`]) and so get what the one filling has made so far.
//!
//! Layout (little endian): `"SSSEEKIX"`, format version `u32`, fingerprint `u64`, build string
//! (`u16` length + bytes), flags `u8` (bit 0: every keyframe interval of the replay is there),
//! snapshot count `u64`, then per snapshot: keyframe `u64`, frame `u64`, flags `u8` (bit 0:
//! stale), state length `u64`, raw length `u64`, payload length `u32`, payload.

use crate::seek_index::Point;
use alloc::string::String;
use alloc::vec::Vec;
use std::io::Write;
use std::path::{Path, PathBuf};
use supershuckie_replay_recorder::UnsignedInteger;

const MAGIC: &[u8; 8] = b"SSSEEKIX";
const FORMAT: u32 = 1;

/// Snapshots are states this build's emulator made, as differences against keyframes as this
/// build's player rebuilds them: another build's are not used.
const BUILD: &str = concat!(env!("CARGO_PKG_VERSION"), "/", env!("CARGO_PKG_NAME"));

/// Extension of an index file.
const EXTENSION: &str = "seekindex";

/// Where a replay's seek index is kept between sessions; see the module documentation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplaySeekIndexFile {
    /// The file itself.
    pub path: PathBuf,
    /// Fingerprint of the recording and the game the index belongs to; a file with another one
    /// is ignored (and overwritten).
    pub fingerprint: u64,
    /// How many bytes all index files in the file's directory may use together; the ones used
    /// longest ago are deleted after a write. `0` for no limit.
    pub directory_budget: u64,
}

impl ReplaySeekIndexFile {
    /// The index file for a recording in `directory`: named after a fingerprint of `replay`
    /// (the recording's file contents) and `rom` (the game, when the recording's keyframes are
    /// rebuilt with it).
    pub fn in_directory(directory: &Path, replay: &[u8], rom: Option<&[u8]>, directory_budget: u64) -> Self {
        let fingerprint = fingerprint(replay, rom);
        Self { path: directory.join(alloc::format!("{fingerprint:016x}.{EXTENSION}")), fingerprint, directory_budget }
    }

    /// Claim the job of filling this index among the programs that have the recording open.
    /// `None` when another one holds it (or the lock cannot be made): then do not run workers,
    /// the file will fill without them. The claim lasts until the lock is dropped.
    pub fn try_lock_for_filling(&self) -> Option<ReplaySeekIndexFillLock> {
        std::fs::create_dir_all(self.path.parent()?).ok()?;
        let path = self.path.with_extension("filling");
        let file = std::fs::File::options().create(true).truncate(false).write(true).open(&path).ok()?;
        file.try_lock().ok()?;
        Some(ReplaySeekIndexFillLock { file: Some(file), path })
    }
}

/// See [`ReplaySeekIndexFile::try_lock_for_filling`].
pub struct ReplaySeekIndexFillLock {
    file: Option<std::fs::File>,
    path: PathBuf,
}

impl Drop for ReplaySeekIndexFillLock {
    fn drop(&mut self) {
        drop(self.file.take());
        // Fails while someone else has taken it over, which is fine.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// An index file together with what this program last saw of it on disk, shared between
/// whoever reads it and whoever writes it.
#[derive(Clone)]
pub(crate) struct TrackedFile {
    pub file: ReplaySeekIndexFile,
    /// The file's modification time when this program last read or wrote it.
    seen: alloc::sync::Arc<std::sync::Mutex<Option<std::time::SystemTime>>>,
}

impl TrackedFile {
    pub fn new(file: ReplaySeekIndexFile) -> Self {
        Self { file, seen: Default::default() }
    }

    fn modified(&self) -> Option<std::time::SystemTime> {
        std::fs::metadata(&self.file.path).and_then(|m| m.modified()).ok()
    }

    /// Note that the file as it is now is known.
    fn note_seen(&self) {
        *self.seen.lock().unwrap_or_else(|p| p.into_inner()) = self.modified();
    }

    /// Whether the file is there and is not the one this program last read or wrote (another
    /// program wrote it).
    pub fn changed(&self) -> bool {
        let now = self.modified();
        now.is_some() && now != *self.seen.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// [`read`] it.
    pub fn read(&self, each: impl FnMut(UnsignedInteger, UnsignedInteger, Point) -> bool) -> Option<Header> {
        // Before reading: a write that lands meanwhile shows as a change afterwards.
        self.note_seen();
        let header = read(&self.file, each);
        // Used now: it should outlive the files nobody opened for longer. Only when that makes
        // a difference, since every other program with it open reads it again when it changes.
        let stale = self.modified().and_then(|m| m.elapsed().ok()).is_some_and(|age| age.as_secs() > 86_400);
        if header.is_some() && stale {
            if let Ok(opened) = std::fs::File::options().write(true).open(&self.file.path) {
                let _ = opened.set_modified(std::time::SystemTime::now());
            }
            self.note_seen();
        }
        header
    }
}

/// A fingerprint of a recording and its game: lengths plus samples spread over both (a recording
/// is tens of megabytes and a game up to gigabytes; hashing all of either every time a replay
/// opens would cost more than the index saves).
fn fingerprint(replay: &[u8], rom: Option<&[u8]>) -> u64 {
    fn mix(hash: &mut u64, bytes: &[u8]) {
        for byte in bytes {
            *hash = (*hash ^ *byte as u64).wrapping_mul(0x0000_0100_0000_01B3);
        }
    }
    fn sample(hash: &mut u64, data: &[u8]) {
        const CHUNK: usize = 4096;
        const CHUNKS: usize = 64;
        mix(hash, &(data.len() as u64).to_le_bytes());
        if data.len() <= CHUNK * CHUNKS {
            mix(hash, data);
            return
        }
        // Evenly spread chunks, the first and the last among them.
        let step = (data.len() - CHUNK) / (CHUNKS - 1);
        for i in 0..CHUNKS {
            let at = i * step;
            mix(hash, &data[at..at + CHUNK]);
        }
    }
    let mut hash = 0xCBF2_9CE4_8422_2325u64;
    sample(&mut hash, replay);
    match rom {
        Some(rom) => sample(&mut hash, rom),
        None => mix(&mut hash, &[0xFF])
    }
    hash
}

/// The directory seek index files go to unless told otherwise: the user's local (not roaming,
/// not synced) cache directory. `None` when the system does not say where that is.
pub fn default_replay_seek_index_directory() -> Option<PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    if cfg!(windows) {
        var("LOCALAPPDATA").map(|d| d.join("SuperShuckie").join("seek-index"))
    }
    else if cfg!(target_os = "macos") {
        var("HOME").map(|d| d.join("Library").join("Caches").join("SuperShuckie").join("seek-index"))
    }
    else {
        var("XDG_CACHE_HOME").or_else(|| var("HOME").map(|d| d.join(".cache"))).map(|d| d.join("supershuckie").join("seek-index"))
    }
}

/// What the start of an index file says.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) struct Header {
    /// Every keyframe interval of the replay had its snapshots when the file was written.
    pub complete: bool,
    /// Snapshots in the file.
    pub points: u64,
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let (taken, rest) = self.0.split_at_checked(len)?;
        self.0 = rest;
        Some(taken)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|b| u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self) -> Option<u64> {
        self.take(8).and_then(|b| b.try_into().ok()).map(u64::from_le_bytes)
    }
}

fn read_header(reader: &mut Reader, fingerprint: u64) -> Option<Header> {
    if reader.take(MAGIC.len())? != MAGIC || reader.u32()? != FORMAT || reader.u64()? != fingerprint {
        return None
    }
    let build = reader.u16()? as usize;
    if reader.take(build)? != BUILD.as_bytes() {
        return None
    }
    let flags = reader.u8()?;
    Some(Header { complete: flags & 1 != 0, points: reader.u64()? })
}

/// The header of `file` as it is on disk, if it is an index of the same recording made by this
/// build.
pub(crate) fn peek(file: &ReplaySeekIndexFile) -> Option<Header> {
    use std::io::Read;
    let mut start = [0u8; 256];
    let mut read = 0;
    let mut opened = std::fs::File::open(&file.path).ok()?;
    while read < start.len() {
        match opened.read(&mut start[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(_) => return None
        }
    }
    read_header(&mut Reader(&start[..read]), file.fingerprint)
}

/// Read `file` and hand every snapshot in it to `each` (keyframe, frame, snapshot), which says
/// whether to go on. `None` when there is no such file or it is not an index of the same
/// recording made by this build; a file cut short yields what is whole and is not complete.
pub(crate) fn read(file: &ReplaySeekIndexFile, mut each: impl FnMut(UnsignedInteger, UnsignedInteger, Point) -> bool) -> Option<Header> {
    let bytes = std::fs::read(&file.path).ok()?;
    let mut reader = Reader(&bytes);
    let mut header = read_header(&mut reader, file.fingerprint)?;
    let point = |reader: &mut Reader| -> Option<(UnsignedInteger, UnsignedInteger, Point)> {
        let keyframe = reader.u64()?;
        let frame = reader.u64()?;
        let flags = reader.u8()?;
        let state_len = usize::try_from(reader.u64()?).ok()?;
        let raw_len = usize::try_from(reader.u64()?).ok()?;
        let payload = reader.u32()? as usize;
        let payload = reader.take(payload)?;
        Some((keyframe, frame, Point { payload: payload.into(), raw_len, state_len, stale: flags & 1 != 0 }))
    };
    for _ in 0..header.points {
        let Some((keyframe, frame, point)) = point(&mut reader) else {
            header.complete = false;
            break
        };
        if !each(keyframe, frame, point) {
            header.complete = false;
            break
        }
    }
    Some(header)
}

/// Write `points` to `file` (through a temporary file next to it, renamed over it when whole),
/// then bring the directory back under its budget. A file of the same recording that holds more
/// snapshots than `points` is left alone (another program filled it further).
pub(crate) fn write(file: &ReplaySeekIndexFile, points: &[(UnsignedInteger, UnsignedInteger, Point)], complete: bool) -> std::io::Result<bool> {
    static SERIAL: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

    if peek(file).is_some_and(|there| there.points > points.len() as u64 || (there.points == points.len() as u64 && there.complete >= complete)) {
        return Ok(false)
    }
    let Some(directory) = file.path.parent() else {
        return Err(std::io::Error::other("the index file has no directory"))
    };
    std::fs::create_dir_all(directory)?;
    let serial = SERIAL.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let temporary = file.path.with_extension(alloc::format!("{}-{serial}.tmp", std::process::id()));

    let result = (|| {
        let mut out = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(&temporary)?);
        out.write_all(MAGIC)?;
        out.write_all(&FORMAT.to_le_bytes())?;
        out.write_all(&file.fingerprint.to_le_bytes())?;
        out.write_all(&(BUILD.len() as u16).to_le_bytes())?;
        out.write_all(BUILD.as_bytes())?;
        out.write_all(&[complete as u8])?;
        out.write_all(&(points.len() as u64).to_le_bytes())?;
        for (keyframe, frame, point) in points {
            out.write_all(&keyframe.to_le_bytes())?;
            out.write_all(&frame.to_le_bytes())?;
            out.write_all(&[point.stale as u8])?;
            out.write_all(&(point.state_len as u64).to_le_bytes())?;
            out.write_all(&(point.raw_len as u64).to_le_bytes())?;
            out.write_all(&(point.payload.len() as u32).to_le_bytes())?;
            out.write_all(&point.payload)?;
        }
        out.into_inner().map_err(|e| e.into_error())?;
        std::fs::rename(&temporary, &file.path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result?;
    trim_directory(directory, file.directory_budget, &file.path);
    Ok(true)
}

/// Delete the index files of `directory` used longest ago until the rest fit `budget` bytes
/// (never `keep`), and temporary files a crashed write left more than a day ago.
fn trim_directory(directory: &Path, budget: u64, keep: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return
    };
    let now = std::time::SystemTime::now();
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue
        };
        if !metadata.is_file() {
            continue
        }
        let modified = metadata.modified().unwrap_or(std::time::UNIX_EPOCH);
        match path.extension().and_then(|e| e.to_str()) {
            Some(EXTENSION) => files.push((modified, metadata.len(), path)),
            Some("tmp") if now.duration_since(modified).is_ok_and(|age| age.as_secs() > 86_400) => {
                let _ = std::fs::remove_file(&path);
            },
            _ => {}
        }
    }
    if budget == 0 {
        return
    }
    let mut total: u64 = files.iter().map(|(_, len, _)| *len).sum();
    files.sort();
    for (_, len, path) in files {
        if total <= budget {
            break
        }
        if path != keep && std::fs::remove_file(&path).is_ok() {
            total -= len;
        }
    }
}

/// A write of the index going on in the background. Dropping it waits for the write, so that
/// closing the program right after closing a replay does not lose the file.
#[derive(Default)]
pub(crate) struct SeekIndexSaver {
    writing: Option<std::thread::JoinHandle<()>>,
    /// [`crate::seek_index::SeekIndex::changes`] as of the last write started.
    pub saved_changes: u64,
    /// When the last write started.
    pub last: Option<std::time::Instant>,
    /// When the file was last looked at for another program's changes.
    pub last_look: Option<std::time::Instant>,
    /// Whether a write has started since the background emulators finished.
    pub saved_finished: bool,
}

impl SeekIndexSaver {
    /// Whether a write is still going on.
    pub fn busy(&self) -> bool {
        self.writing.as_ref().is_some_and(|w| !w.is_finished())
    }

    /// Wait for the write going on, if any.
    pub fn wait(&mut self) {
        if let Some(writing) = self.writing.take() {
            let _ = writing.join();
        }
    }

    /// Write `points` to `file` on a thread of its own (after the write going on, if any).
    pub fn save(&mut self, file: TrackedFile, points: Vec<(UnsignedInteger, UnsignedInteger, Point)>, complete: bool) {
        let before = self.writing.take();
        self.last = Some(std::time::Instant::now());
        self.writing = std::thread::Builder::new().name(String::from("seek-index-save")).spawn(move || {
            if let Some(before) = before {
                let _ = before.join();
            }
            crate::thread::mark_thread_background();
            if let Ok(true) = write(&file.file, &points, complete) {
                file.note_seen();
            }
        }).ok();
    }

    /// Nothing written counts any more (another replay's index is in memory now).
    pub fn reset(&mut self) {
        self.saved_changes = 0;
        self.last = None;
        self.last_look = None;
        self.saved_finished = false;
    }
}

impl Drop for SeekIndexSaver {
    fn drop(&mut self) {
        self.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(seed: u8, stale: bool) -> Point {
        Point { payload: (0..50u8).map(|i| i.wrapping_mul(seed)).collect::<Vec<u8>>().into(), raw_len: 400 + seed as usize, state_len: 4096, stale }
    }

    fn scratch_directory(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(alloc::format!("supershuckie-seek-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        directory
    }

    fn read_all(file: &ReplaySeekIndexFile) -> (Option<Header>, Vec<(u64, u64, Vec<u8>, usize, usize, bool)>) {
        let mut all = Vec::new();
        let header = read(file, |keyframe, frame, p| {
            all.push((keyframe, frame, p.payload.to_vec(), p.raw_len, p.state_len, p.stale));
            true
        });
        (header, all)
    }

    #[test]
    fn an_index_comes_back_as_it_was_written_and_only_for_the_same_recording() {
        let directory = scratch_directory("round-trip");
        let replay = alloc::vec![7u8; 1000];
        let file = ReplaySeekIndexFile::in_directory(&directory, &replay, Some(&[1, 2, 3]), 0);
        assert_eq!(read_all(&file), (None, Vec::new()), "no file yet");

        let points = alloc::vec![(0u64, 15u64, point(3, false)), (0, 30, point(5, true)), (120, 135, point(9, false))];
        assert!(write(&file, &points, true).expect("write"));
        let (header, all) = read_all(&file);
        assert_eq!(header, Some(Header { complete: true, points: 3 }));
        assert_eq!(all.len(), 3);
        for ((keyframe, frame, p), read) in points.iter().zip(&all) {
            assert_eq!((*keyframe, *frame, p.payload.to_vec(), p.raw_len, p.state_len, p.stale), *read);
        }

        // Fewer snapshots do not replace more; as many but complete replace incomplete.
        assert!(!write(&file, &points[..2], true).expect("write"));
        assert_eq!(peek(&file), Some(Header { complete: true, points: 3 }));

        // Another game, another recording or another fingerprint in the same file: not ours.
        let other = ReplaySeekIndexFile::in_directory(&directory, &replay, Some(&[1, 2, 4]), 0);
        assert_ne!(other.path, file.path);
        assert_ne!(ReplaySeekIndexFile::in_directory(&directory, &replay[1..], Some(&[1, 2, 3]), 0).path, file.path);
        assert_ne!(ReplaySeekIndexFile::in_directory(&directory, &replay, None, 0).path, file.path);
        let renamed = ReplaySeekIndexFile { fingerprint: file.fingerprint ^ 1, ..file.clone() };
        assert_eq!(read_all(&renamed), (None, Vec::new()));

        // A file cut short gives what is whole.
        let bytes = std::fs::read(&file.path).unwrap();
        std::fs::write(&file.path, &bytes[..bytes.len() - 10]).unwrap();
        let (header, all) = read_all(&file);
        assert_eq!((header, all.len()), (Some(Header { complete: false, points: 3 }), 2));
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn one_program_fills_and_the_others_see_what_it_writes() {
        let directory = scratch_directory("shared");
        let file = ReplaySeekIndexFile::in_directory(&directory, &[1, 2], None, 0);
        let lock = file.try_lock_for_filling().expect("the first one gets it");
        assert!(file.try_lock_for_filling().is_none(), "the second does not");

        let (ours, theirs) = (TrackedFile::new(file.clone()), TrackedFile::new(file.clone()));
        assert!(!theirs.changed(), "nothing there yet");
        let points = alloc::vec![(0u64, 15u64, point(3, false))];
        assert!(write(&ours.file, &points, false).expect("write"));
        ours.note_seen();
        assert!(!ours.changed(), "our own write");
        assert!(theirs.changed());
        assert_eq!(theirs.read(|_, _, _| true), Some(Header { complete: false, points: 1 }));
        assert!(!theirs.changed(), "read now");

        drop(lock);
        assert!(file.try_lock_for_filling().is_some(), "free again");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn the_directory_stays_under_its_budget_by_dropping_the_oldest() {
        let directory = scratch_directory("budget");
        let points = alloc::vec![(0u64, 15u64, point(3, false))];
        let size = {
            let file = ReplaySeekIndexFile::in_directory(&directory, &[0], None, 0);
            write(&file, &points, false).expect("write");
            std::fs::metadata(&file.path).unwrap().len()
        };
        let mut paths = Vec::new();
        for i in 0..4u8 {
            let file = ReplaySeekIndexFile::in_directory(&directory, &[i], None, size * 2 + 1);
            write(&file, &points, false).expect("write");
            // Older than the next one, whatever the file system's clock resolution.
            let age = std::time::Duration::from_secs(1000 - i as u64 * 100);
            std::fs::File::options().write(true).open(&file.path).unwrap().set_modified(std::time::SystemTime::now() - age).unwrap();
            paths.push(file.path);
        }
        let file = ReplaySeekIndexFile::in_directory(&directory, &[9], None, size * 2 + 1);
        write(&file, &points, false).expect("write");
        let left: Vec<bool> = paths.iter().map(|p| p.exists()).collect();
        assert_eq!(left, [false, false, false, true], "the newest of the old ones and the one just written stay");
        assert!(file.path.exists());
        let _ = std::fs::remove_dir_all(&directory);
    }
}
