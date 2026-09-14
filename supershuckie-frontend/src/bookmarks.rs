//! Replay bookmarks: the current replay's bookmark table, the user's bookmark types, and saving
//! edits.
//!
//! While recording, every change goes to the core thread, which writes it into the recording (see
//! `ReplayFileRecorder::set_bookmark_table`). During playback, changes are written into the replay
//! file's bookmark section on a background thread shortly after they are made (see
//! [`write_bookmark_section`]); a pre-v5 replay is upgraded in place on its first change, after the
//! user confirms.

use std::collections::BTreeSet;
use std::fmt::{Display, Formatter};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use supershuckie_core::BookmarkAnchorError;
use supershuckie_frontend_webserver::{BookmarkReply, BookmarkRequest};

pub use supershuckie_frontend_webserver::BookmarkParams;
use supershuckie_replay_recorder::replay_file::bookmark_section::{write_bookmark_section, BookmarkSectionWriteError, BookmarkSectionWriteOutcome, BOOKMARK_UPGRADE_MINIMUM_VERSION};
use supershuckie_replay_recorder::replay_file::{ReplayHeaderBytes, REPLAY_VERSION_BOOKMARK_SECTION};
use supershuckie_replay_recorder::{blake3_hash, Bookmark, BookmarkTable, BookmarkTypeRecord, TimestampMillis};

use crate::settings::{hex_color, hex_type_id, BookmarkSettings, BookmarkTypeSetting};
use crate::{SuperShuckieFrontend, SuperShuckieReplayState};

/// How long after the last change a played-back replay's bookmarks are written.
const WRITE_DELAY: Duration = Duration::from_millis(250);

/// Longest bookmark or type name kept.
const MAX_NAME_CHARS: usize = 200;

/// Name used for untyped generic bookmarks.
const GENERIC_NAME: &str = "Bookmark";

/// Why a bookmark operation did not happen.
#[derive(Clone, Debug, PartialEq)]
pub enum BookmarkError {
    /// No replay is being recorded or played back.
    NoReplay,
    /// No bookmark (or type) has this id.
    NotFound(String),
    /// The request itself is wrong.
    Invalid(String),
    /// The request does not make sense right now (e.g. seeking while recording).
    WrongState(String),
    /// Saving into this pre-v5 replay would upgrade it; ask the user, then retry with the upgrade
    /// allowed.
    NeedsUpgradeConfirmation {
        /// The replay's format version.
        version: u32
    },
    /// The replay is too old to upgrade in place.
    NeedsConversion {
        /// The replay's format version.
        version: u32
    },
    /// The replay was loaded with damaged data dropped; it cannot be written to.
    Damaged,
    /// The emulator did not answer in time.
    Busy,
    /// Saving the bookmarks failed.
    WriteFailed(String)
}

impl BookmarkError {
    /// The HTTP status the REST API reports this error with.
    pub fn http_status(&self) -> u16 {
        match self {
            Self::NoReplay | Self::NotFound(_) => 404,
            Self::Invalid(_) => 400,
            Self::WrongState(_) | Self::NeedsUpgradeConfirmation { .. } | Self::NeedsConversion { .. } | Self::Damaged => 409,
            Self::Busy => 503,
            Self::WriteFailed(_) => 500
        }
    }
}

impl Display for BookmarkError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoReplay => f.write_str("Bookmarks need a replay that is recording or playing back."),
            Self::NotFound(what) => write!(f, "There is no {what}."),
            Self::Invalid(message) | Self::WrongState(message) | Self::WriteFailed(message) => f.write_str(message),
            Self::NeedsUpgradeConfirmation { version } => write!(f, "Saving bookmarks into this replay upgrades it from format v{version} to v{REPLAY_VERSION_BOOKMARK_SECTION}. Older Super Shuckie builds will not be able to open it."),
            Self::NeedsConversion { version } => write!(f, "This replay uses format v{version}. Convert it to the current format before adding bookmarks."),
            Self::Damaged => f.write_str("This replay is damaged; convert it before adding bookmarks."),
            Self::Busy => f.write_str("The emulator is busy; try again in a moment.")
        }
    }
}

impl From<BookmarkAnchorError> for BookmarkError {
    fn from(value: BookmarkAnchorError) -> Self {
        match value {
            BookmarkAnchorError::NoReplay => Self::NoReplay,
            BookmarkAnchorError::Busy => Self::Busy
        }
    }
}

/// A bookmark type as shown to the user.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BookmarkTypeView {
    /// 16 hex digits.
    pub id: String,
    pub name: String,
    /// `#RRGGBB`.
    pub color: String,
    /// Whether the type is one of the user's; `false` for a type only known from the replay.
    pub saved: bool
}

/// A bookmark as shown to the user.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BookmarkView {
    pub id: u64,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: Option<BookmarkTypeView>,
    pub in_frame: u64,
    pub in_millis: u64,
    pub out_frame: Option<u64>,
    pub out_millis: Option<u64>,
    pub keyframe: bool
}

/// The current replay's bookmarks and everything a bookmark list needs.
#[derive(Clone, Debug, Serialize)]
pub struct BookmarksView {
    /// Name of the replay, if any.
    pub replay: Option<String>,
    /// `none`, `recording` or `playback`.
    pub state: &'static str,
    /// Changes whenever anything here does.
    pub generation: u64,
    /// Whether bookmarks can be added and changed.
    pub editable: bool,
    /// Whether the first change will upgrade the replay file (and ask first, if the user wants).
    pub needs_upgrade: bool,
    /// Format version of the replay being played back.
    pub replay_version: Option<u32>,
    /// Why bookmarks cannot be changed, or why the last save failed.
    pub problem: Option<String>,
    /// The range bookmark started by the start/end range command and not ended yet.
    pub open_range: Option<u64>,
    /// Type given to new bookmarks (hex id), if any.
    pub active_type: Option<String>,
    pub bookmarks: Vec<BookmarkView>,
    /// The user's types, then any types only known from the replay.
    pub types: Vec<BookmarkTypeView>
}

/// A new or changed bookmark type.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct BookmarkTypeUpsert {
    /// Hex id of the type to change; absent to create one.
    pub id: Option<String>,
    pub name: Option<String>,
    /// `#RRGGBB`; a new type without one gets the next color of a spread-out palette.
    pub color: Option<String>
}

/// What a played-back replay is, for writing its bookmarks.
struct PlaybackTarget {
    replay: String,
    path: PathBuf,
    header: ReplayHeaderBytes,
    version: u32,
    truncated: bool,
    upgrade_confirmed: bool
}

enum Target {
    None,
    Recording { replay: String },
    Playback(PlaybackTarget)
}

/// Facts about a replay that was just loaded for playback.
pub struct LoadedReplay {
    pub name: String,
    pub path: PathBuf,
    pub header: ReplayHeaderBytes,
    pub version: u32,
    pub truncated: bool
}

struct WriteJob {
    serial: u64,
    path: PathBuf,
    header: ReplayHeaderBytes,
    table: BookmarkTable
}

struct WriteResult {
    serial: u64,
    path: PathBuf,
    result: Result<BookmarkSectionWriteOutcome, BookmarkSectionWriteError>
}

struct Writer {
    jobs: Sender<WriteJob>,
    results: Receiver<WriteResult>
}

impl Writer {
    fn spawn() -> Option<Self> {
        let (jobs, job_receiver) = channel::<WriteJob>();
        let (result_sender, results) = channel();
        std::thread::Builder::new()
            .name("BookmarkWriter".to_owned())
            .spawn(move || {
                while let Ok(job) = job_receiver.recv() {
                    let result = write_bookmark_section(&job.path, Some(&job.header), &job.table);
                    if result_sender.send(WriteResult { serial: job.serial, path: job.path, result }).is_err() {
                        break
                    }
                }
            })
            .ok()?;
        Some(Self { jobs, results })
    }
}

/// The current replay's bookmarks.
pub struct ReplayBookmarks {
    table: BookmarkTable,
    target: Target,
    generation: u64,
    open_range: Option<u64>,

    /// Recording: the core has not been given the latest table.
    core_dirty: bool,

    /// Playback: serial of the latest change, and of the latest change on disk.
    edit_serial: u64,
    saved_serial: u64,
    /// When the oldest unsaved change was made (cleared once a write is sent).
    dirty_since: Option<Instant>,
    in_flight: Option<u64>,
    write_error: Option<String>,
    writer: Option<Writer>,

    /// An error to show the user once (a failed save, or edits lost when switching replays).
    report: Option<String>
}

impl Default for ReplayBookmarks {
    fn default() -> Self {
        Self::new()
    }
}

impl ReplayBookmarks {
    pub fn new() -> Self {
        Self {
            table: BookmarkTable::new(),
            target: Target::None,
            generation: 1,
            open_range: None,
            core_dirty: false,
            edit_serial: 0,
            saved_serial: 0,
            dirty_since: None,
            in_flight: None,
            write_error: None,
            writer: None,
            report: None
        }
    }

    /// The current replay's bookmarks.
    pub fn table(&self) -> &BookmarkTable {
        &self.table
    }

    /// Changes whenever the table or the replay does.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn changed(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    fn reset(&mut self, target: Target, table: BookmarkTable) {
        self.table = table;
        self.target = target;
        self.open_range = None;
        self.core_dirty = false;
        self.edit_serial = 0;
        self.saved_serial = 0;
        self.dirty_since = None;
        self.write_error = None;
        self.changed();
    }

    /// A recording started with `table` (empty, or a resumed replay's bookmarks); the core already
    /// has it.
    pub fn set_recording(&mut self, replay: String, table: BookmarkTable) {
        self.reset(Target::Recording { replay }, table);
    }

    /// A replay was loaded for playback with `table`.
    pub fn set_playback(&mut self, replay: LoadedReplay, table: BookmarkTable) {
        self.reset(Target::Playback(PlaybackTarget {
            replay: replay.name,
            path: replay.path,
            header: replay.header,
            version: replay.version,
            truncated: replay.truncated,
            upgrade_confirmed: replay.version >= REPLAY_VERSION_BOOKMARK_SECTION
        }), table);
    }

    /// No replay any more. Unsaved playback edits must have been flushed first (see
    /// [`Self::flush`]).
    pub fn clear(&mut self) {
        if matches!(self.target, Target::None) {
            return
        }
        self.reset(Target::None, BookmarkTable::new());
    }

    fn state_name(&self) -> &'static str {
        match self.target {
            Target::None => "none",
            Target::Recording { .. } => "recording",
            Target::Playback(_) => "playback"
        }
    }

    /// Why the bookmarks cannot be changed at all, if they cannot.
    fn read_only_reason(&self) -> Option<BookmarkError> {
        match &self.target {
            Target::None => Some(BookmarkError::NoReplay),
            Target::Recording { .. } => None,
            Target::Playback(p) if p.truncated => Some(BookmarkError::Damaged),
            Target::Playback(p) if p.version < BOOKMARK_UPGRADE_MINIMUM_VERSION => Some(BookmarkError::NeedsConversion { version: p.version }),
            Target::Playback(_) => None
        }
    }

    /// Check that the bookmarks may be changed now. A pre-v5 replay needs the upgrade allowed (or
    /// the user not wanting to be asked); once allowed, it stays allowed for this replay.
    pub fn check_editable(&mut self, settings: &BookmarkSettings, allow_upgrade: bool) -> Result<(), BookmarkError> {
        if let Some(reason) = self.read_only_reason() {
            return Err(reason)
        }
        if let Target::Playback(p) = &mut self.target && !p.upgrade_confirmed {
            if settings.confirm_replay_upgrade && !allow_upgrade {
                return Err(BookmarkError::NeedsUpgradeConfirmation { version: p.version })
            }
            p.upgrade_confirmed = true;
        }
        Ok(())
    }

    /// Change the table with `edit`; on success the types are refreshed from `settings` and the
    /// change is queued for the core (recording) or the file (playback).
    fn edit<R>(&mut self, settings: &BookmarkSettings, edit: impl FnOnce(&mut BookmarkTable) -> Result<R, BookmarkError>) -> Result<R, BookmarkError> {
        let mut table = self.table.clone();
        let result = edit(&mut table)?;
        table.sort();
        refresh_type_records(&mut table, settings);
        if table == self.table {
            return Ok(result)
        }
        self.table = table;
        self.changed();

        if let Some(id) = self.open_range && self.table.get(id).is_none() {
            self.open_range = None;
        }

        match self.target {
            Target::None => {}
            Target::Recording { .. } => self.core_dirty = true,
            Target::Playback(_) => {
                self.edit_serial += 1;
                self.dirty_since.get_or_insert_with(Instant::now);
            }
        }
        Ok(result)
    }

    /// The table to hand to the core, if it changed since it was last handed over (recording).
    pub fn take_core_update(&mut self) -> Option<BookmarkTable> {
        (matches!(self.target, Target::Recording { .. }) && core::mem::take(&mut self.core_dirty)).then(|| self.table.clone())
    }

    fn handle_write_result(&mut self, result: WriteResult) {
        self.in_flight = None;
        let Target::Playback(p) = &mut self.target else {
            return
        };
        if result.path != p.path {
            return
        }
        match result.result {
            Ok(outcome) => {
                p.header = outcome.header;
                if outcome.upgraded_from.is_some() {
                    p.version = REPLAY_VERSION_BOOKMARK_SECTION;
                }
                self.saved_serial = self.saved_serial.max(result.serial);
                if self.write_error.take().is_some() {
                    self.changed();
                }
            }
            Err(e) => {
                let message = format!("Bookmarks could not be saved into {}: {e}", p.replay);
                if self.write_error.as_ref() != Some(&message) {
                    self.report = Some(message.clone());
                }
                self.write_error = Some(message);
                self.changed();
            }
        }
    }

    /// Collect finished writes and start the next one when a change has settled (playback).
    pub fn poll_writes(&mut self, now: Instant) {
        while let Some(result) = self.writer.as_ref().and_then(|w| w.results.try_recv().ok()) {
            self.handle_write_result(result);
        }

        let Target::Playback(p) = &self.target else {
            return
        };
        if self.in_flight.is_some() || self.saved_serial >= self.edit_serial || !self.dirty_since.is_some_and(|at| now.duration_since(at) >= WRITE_DELAY) {
            return
        }

        if self.writer.is_none() {
            self.writer = Writer::spawn();
        }
        let job = WriteJob { serial: self.edit_serial, path: p.path.clone(), header: p.header, table: self.table.clone() };
        match self.writer.as_ref().map(|w| w.jobs.send(job)) {
            Some(Ok(())) => {
                self.in_flight = Some(self.edit_serial);
                self.dirty_since = None;
            }
            _ => {
                // No writer thread; save synchronously instead.
                self.dirty_since = None;
                let _ = self.flush();
            }
        }
    }

    /// Write any unsaved playback edits now.
    pub fn flush(&mut self) -> Result<(), BookmarkError> {
        if self.in_flight.is_some() {
            let received = self.writer.as_ref().map(|w| w.results.recv_timeout(Duration::from_secs(10)));
            match received {
                Some(Ok(result)) => self.handle_write_result(result),
                Some(Err(RecvTimeoutError::Timeout)) => return Err(BookmarkError::WriteFailed("Saving bookmarks is taking too long.".to_owned())),
                _ => self.in_flight = None
            }
        }

        let Target::Playback(p) = &self.target else {
            return Ok(())
        };
        if self.saved_serial >= self.edit_serial {
            return Ok(())
        }

        let serial = self.edit_serial;
        let path = p.path.clone();
        let result = write_bookmark_section(&path, Some(&p.header), &self.table);
        let failed = result.as_ref().err().map(|e| e.to_string());
        self.dirty_since = None;
        self.handle_write_result(WriteResult { serial, path, result });
        match failed {
            None => Ok(()),
            Some(e) => Err(BookmarkError::WriteFailed(e))
        }
    }

    /// An error to show the user, once.
    pub fn take_report(&mut self) -> Option<String> {
        self.report.take()
    }
}

/// A new, non-zero bookmark type id that `taken` does not know.
fn new_type_id(taken: impl Fn(u64) -> bool) -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    loop {
        let mut seed = Vec::with_capacity(32);
        seed.extend_from_slice(&SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0).to_le_bytes());
        seed.extend_from_slice(&std::process::id().to_le_bytes());
        seed.extend_from_slice(&COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
        let id = u64::from_le_bytes(blake3_hash(&seed)[..8].try_into().expect("8 bytes"));
        if id != 0 && !taken(id) {
            return id
        }
    }
}

/// The `index`-th color of a palette whose hues are spread by the golden angle.
fn palette_color(index: usize) -> u32 {
    let hue = (index as f64 * 137.507_764 + 18.0) % 360.0;
    let (saturation, value) = (0.65, 0.9);
    let chroma = value * saturation;
    let x = chroma * (1.0 - ((hue / 60.0) % 2.0 - 1.0).abs());
    let m = value - chroma;
    let (r, g, b) = match hue as u32 / 60 {
        0 => (chroma, x, 0.0),
        1 => (x, chroma, 0.0),
        2 => (0.0, chroma, x),
        3 => (0.0, x, chroma),
        4 => (x, 0.0, chroma),
        _ => (chroma, 0.0, x)
    };
    let channel = |c: f64| ((c + m) * 255.0).round().clamp(0.0, 255.0) as u32;
    channel(r) << 16 | channel(g) << 8 | channel(b)
}

/// A cleaned-up name: one line, trimmed, at most [`MAX_NAME_CHARS`]; `None` if nothing is left.
fn clean_name(name: &str) -> Option<String> {
    let name: String = name.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let name = name.trim();
    (!name.is_empty()).then(|| name.chars().take(MAX_NAME_CHARS).collect::<String>().trim_end().to_owned())
}

/// `"<prefix> N"`, with N one more than the highest such number in use.
fn generic_name(table: &BookmarkTable, prefix: &str) -> String {
    let highest = table.bookmarks.iter()
        .filter_map(|b| b.name.strip_prefix(prefix)?.strip_prefix(' ')?.parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    format!("{prefix} {}", highest + 1)
}

/// Make the table's type records match the user's types for every type it uses.
fn refresh_type_records(table: &mut BookmarkTable, settings: &BookmarkSettings) {
    let used: BTreeSet<u64> = table.bookmarks.iter().map(|b| b.type_id).filter(|&id| id != 0).collect();
    for id in used {
        if let Some(t) = settings.get_type(id) {
            table.set_type_record(BookmarkTypeRecord { id, name: t.name.clone(), color: t.color });
        }
    }
    table.prune_types();
}

fn type_view(settings: &BookmarkSettings, table: &BookmarkTable, id: u64) -> Option<BookmarkTypeView> {
    if let Some(t) = settings.get_type(id) {
        return Some(BookmarkTypeView { id: hex_type_id::format(id), name: t.name.clone(), color: hex_color::format(t.color), saved: true })
    }
    table.type_record(id).map(|t| BookmarkTypeView { id: hex_type_id::format(id), name: t.name.clone(), color: hex_color::format(t.color), saved: false })
}

/// Which type a request asks for.
#[derive(Clone, Debug, PartialEq)]
enum TypeChoice {
    Untyped,
    Id(u64),
    Name(String)
}

/// The type a request names, if it names one.
fn requested_type(params: &BookmarkParams) -> Result<Option<TypeChoice>, BookmarkError> {
    if let Some(text) = params.type_id.as_deref() {
        return match hex_type_id::parse(text) {
            Some(0) => Ok(Some(TypeChoice::Untyped)),
            Some(id) => Ok(Some(TypeChoice::Id(id))),
            None => Err(BookmarkError::Invalid(format!("{text:?} is not a bookmark type id.")))
        }
    }
    Ok(match params.type_name.as_deref().map(str::trim) {
        None => None,
        Some("") | Some("none") => Some(TypeChoice::Untyped),
        Some(name) => Some(TypeChoice::Name(name.to_owned()))
    })
}

/// Resolve a type choice to an id, adding the type to the user's types if it is new (by name) or
/// only known from the replay. Returns the id and whether the settings changed.
fn resolve_type(settings: &mut BookmarkSettings, table: &BookmarkTable, choice: TypeChoice) -> Result<(u64, bool), BookmarkError> {
    match choice {
        TypeChoice::Untyped => Ok((0, false)),
        TypeChoice::Id(id) => {
            if settings.get_type(id).is_some() {
                return Ok((id, false))
            }
            let record = table.type_record(id).ok_or_else(|| BookmarkError::NotFound(format!("bookmark type with id {}", hex_type_id::format(id))))?;
            settings.types.push(BookmarkTypeSetting { id, name: record.name.clone(), color: record.color });
            Ok((id, true))
        }
        TypeChoice::Name(name) => {
            let name = clean_name(&name).ok_or_else(|| BookmarkError::Invalid("A bookmark type needs a name.".to_owned()))?;
            if let Some(t) = settings.types.iter().find(|t| t.name.to_lowercase() == name.to_lowercase()) {
                return Ok((t.id, false))
            }
            if let Some(record) = table.types.iter().find(|t| t.name.to_lowercase() == name.to_lowercase()) && settings.get_type(record.id).is_none() {
                settings.types.push(BookmarkTypeSetting { id: record.id, name: record.name.clone(), color: record.color });
                return Ok((record.id, true))
            }
            let id = new_type_id(|id| settings.get_type(id).is_some() || table.type_record(id).is_some());
            let color = palette_color(settings.types.len());
            settings.types.push(BookmarkTypeSetting { id, name, color });
            Ok((id, true))
        }
    }
}

/// What a request's `out` asks for.
#[derive(Copy, Clone, Debug, PartialEq)]
enum OutChoice {
    Clear,
    Now,
    Frame(u64)
}

fn requested_out(params: &BookmarkParams) -> Result<Option<OutChoice>, BookmarkError> {
    Ok(match params.out.as_deref().map(str::trim) {
        None => None,
        Some("none") | Some("") => Some(OutChoice::Clear),
        Some("now") => Some(OutChoice::Now),
        Some(frame) => Some(OutChoice::Frame(frame.parse().map_err(|_| BookmarkError::Invalid(format!("{frame:?} is not a frame (use a frame number, now or none).")))?))
    })
}

/// End the range bookmark `id` at `now`, swapping its ends if `now` is before its start.
fn end_range(table: &mut BookmarkTable, id: u64, now: (u64, TimestampMillis)) -> Result<(), BookmarkError> {
    let bookmark = table.get_mut(id).ok_or_else(|| BookmarkError::NotFound(format!("bookmark {id}")))?;
    if now.0 >= bookmark.in_frame {
        bookmark.out = Some(now);
    }
    else {
        bookmark.out = Some((bookmark.in_frame, bookmark.in_millis));
        bookmark.in_frame = now.0;
        bookmark.in_millis = now.1;
        bookmark.keyframe = false;
    }
    Ok(())
}

impl SuperShuckieFrontend {
    /// Changes whenever the current replay's bookmarks, the replay, or the bookmark types change.
    pub fn bookmark_generation(&self) -> u64 {
        self.bookmarks.generation().wrapping_add(self.bookmark_types_generation)
    }

    fn bookmark_view(&self, bookmark: &Bookmark) -> BookmarkView {
        BookmarkView {
            id: bookmark.id,
            name: bookmark.name.clone(),
            kind: type_view(&self.settings.bookmarks, self.bookmarks.table(), bookmark.type_id),
            in_frame: bookmark.in_frame,
            in_millis: bookmark.in_millis.0,
            out_frame: bookmark.out.map(|o| o.0),
            out_millis: bookmark.out.map(|o| o.1.0),
            keyframe: bookmark.keyframe
        }
    }

    fn bookmark_view_by_id(&self, id: u64) -> Result<BookmarkView, BookmarkError> {
        self.bookmarks.table().get(id).map(|b| self.bookmark_view(b)).ok_or_else(|| BookmarkError::NotFound(format!("bookmark {id}")))
    }

    /// The user's bookmark types, then any types only known from the current replay.
    pub fn bookmark_types(&self) -> Vec<BookmarkTypeView> {
        let settings = &self.settings.bookmarks;
        let mut types: Vec<BookmarkTypeView> = settings.types.iter()
            .map(|t| BookmarkTypeView { id: hex_type_id::format(t.id), name: t.name.clone(), color: hex_color::format(t.color), saved: true })
            .collect();
        types.extend(self.bookmarks.table().types.iter()
            .filter(|t| settings.get_type(t.id).is_none())
            .map(|t| BookmarkTypeView { id: hex_type_id::format(t.id), name: t.name.clone(), color: hex_color::format(t.color), saved: false }));
        types
    }

    /// Everything about the current replay's bookmarks.
    pub fn bookmarks_view(&self) -> BookmarksView {
        let (replay, version, needs_upgrade) = match &self.bookmarks.target {
            Target::None => (None, None, false),
            Target::Recording { replay } => (Some(replay.clone()), None, false),
            Target::Playback(p) => (Some(p.replay.clone()), Some(p.version), p.version < REPLAY_VERSION_BOOKMARK_SECTION && p.version >= BOOKMARK_UPGRADE_MINIMUM_VERSION)
        };
        let read_only = self.bookmarks.read_only_reason();

        BookmarksView {
            replay,
            state: self.bookmarks.state_name(),
            generation: self.bookmark_generation(),
            editable: read_only.is_none(),
            needs_upgrade,
            replay_version: version,
            problem: read_only.filter(|r| *r != BookmarkError::NoReplay).map(|r| r.to_string()).or_else(|| self.bookmarks.write_error.clone()),
            open_range: self.bookmarks.open_range,
            active_type: self.settings.bookmarks.get_type(self.settings.bookmarks.active_type).map(|t| hex_type_id::format(t.id)),
            bookmarks: self.bookmarks.table().bookmarks.iter().map(|b| self.bookmark_view(b)).collect(),
            types: self.bookmark_types()
        }
    }

    /// The frame counter now, and the replay time then.
    fn bookmark_now(&self) -> Result<(u64, TimestampMillis), BookmarkError> {
        let anchor = self.core.bookmark_anchor(false)?;
        Ok((anchor.in_frame, anchor.in_millis))
    }

    /// Check that `frame` exists in the replay (so far, when recording).
    fn check_bookmark_frame(&self, frame: u64) -> Result<(), BookmarkError> {
        let (limit, what) = match self.get_replay_state() {
            SuperShuckieReplayState::Recording => (self.core.get_elapsed_time().frames as u64, "recorded so far"),
            SuperShuckieReplayState::Playback => (self.core.get_playback_total_frames() as u64, "in the replay"),
            SuperShuckieReplayState::NoReplay => return Err(BookmarkError::NoReplay)
        };
        if frame > limit {
            return Err(BookmarkError::Invalid(format!("Frame {frame} is past the last frame {what} ({limit}).")))
        }
        Ok(())
    }

    fn bookmark_point_at(&self, frame: u64) -> Result<(u64, TimestampMillis), BookmarkError> {
        self.check_bookmark_frame(frame)?;
        let millis = self.core.estimate_millis_at(frame)?.ok_or(BookmarkError::NoReplay)?;
        Ok((frame, millis))
    }

    fn resolve_out(&self, choice: OutChoice) -> Result<Option<(u64, TimestampMillis)>, BookmarkError> {
        match choice {
            OutChoice::Clear => Ok(None),
            OutChoice::Now => self.bookmark_now().map(Some),
            OutChoice::Frame(frame) => self.bookmark_point_at(frame).map(Some)
        }
    }

    fn resolve_bookmark_type(&mut self, choice: TypeChoice) -> Result<u64, BookmarkError> {
        let (id, changed) = resolve_type(&mut self.settings.bookmarks, self.bookmarks.table(), choice)?;
        if changed {
            self.bookmark_types_changed();
        }
        Ok(id)
    }

    fn bookmark_types_changed(&mut self) {
        self.bookmark_types_generation = self.bookmark_types_generation.wrapping_add(1);
        self.write_config();
    }

    fn generic_bookmark_prefix(&self, type_id: u64) -> String {
        type_view(&self.settings.bookmarks, self.bookmarks.table(), type_id).map(|t| t.name).unwrap_or_else(|| GENERIC_NAME.to_owned())
    }

    /// Add a bookmark to the current replay.
    ///
    /// Without a frame it goes on the current frame (a keyframe bookmark: see
    /// `SuperShuckieCore::bookmark_anchor`); without a name it gets a generic one; without a type it
    /// gets the active type. A type named but unknown is added to the user's types.
    pub fn add_bookmark(&mut self, params: BookmarkParams, allow_upgrade: bool) -> Result<BookmarkView, BookmarkError> {
        self.bookmarks.check_editable(&self.settings.bookmarks, allow_upgrade)?;

        // Check the whole request before placing anything: a keyframe bookmark writes a keyframe.
        let out_choice = requested_out(&params)?;
        let type_choice = requested_type(&params)?;

        let keyframe = params.keyframe.unwrap_or(false);
        let (in_frame, in_millis, keyframe) = match params.frame {
            None => {
                let anchor = self.core.bookmark_anchor(keyframe)?;
                (anchor.in_frame, anchor.in_millis, anchor.keyframe)
            }
            Some(_) if keyframe => return Err(BookmarkError::Invalid("A keyframe bookmark is always placed at the current frame; leave out the frame.".to_owned())),
            Some(frame) => {
                let (frame, millis) = self.bookmark_point_at(frame)?;
                (frame, millis, false)
            }
        };

        let out = match out_choice {
            None => None,
            Some(choice) => self.resolve_out(choice)?
        };
        if let Some((out_frame, _)) = out && out_frame < in_frame {
            return Err(BookmarkError::Invalid("The out frame must not be before the in frame.".to_owned()))
        }

        let type_id = match type_choice {
            Some(choice) => self.resolve_bookmark_type(choice)?,
            None => self.settings.bookmarks.get_type(self.settings.bookmarks.active_type).map(|t| t.id).unwrap_or(0)
        };
        let name = params.name.as_deref().and_then(clean_name);
        let prefix = self.generic_bookmark_prefix(type_id);

        let id = self.bookmarks.edit(&self.settings.bookmarks, |table| {
            let name = name.unwrap_or_else(|| generic_name(table, &prefix));
            Ok(table.insert(Bookmark { id: 0, name, type_id, in_frame, in_millis, out, keyframe }))
        })?;
        self.bookmark_view_by_id(id)
    }

    /// Change a bookmark. Moving its in frame makes a keyframe bookmark an ordinary one.
    pub fn update_bookmark(&mut self, id: u64, params: BookmarkParams, allow_upgrade: bool) -> Result<BookmarkView, BookmarkError> {
        self.bookmarks.check_editable(&self.settings.bookmarks, allow_upgrade)?;
        let mut bookmark = self.bookmarks.table().get(id).cloned().ok_or_else(|| BookmarkError::NotFound(format!("bookmark {id}")))?;

        if let Some(name) = params.name.as_deref() {
            bookmark.name = clean_name(name).ok_or_else(|| BookmarkError::Invalid("A bookmark needs a name.".to_owned()))?;
        }

        match params.keyframe {
            Some(true) if !bookmark.keyframe => return Err(BookmarkError::Invalid("Only a new bookmark at the current frame can be a keyframe bookmark.".to_owned())),
            Some(false) => bookmark.keyframe = false,
            _ => {}
        }

        if let Some(frame) = params.frame && frame != bookmark.in_frame {
            (bookmark.in_frame, bookmark.in_millis) = self.bookmark_point_at(frame)?;
            bookmark.keyframe = false;
        }

        if let Some(choice) = requested_out(&params)? {
            bookmark.out = self.resolve_out(choice)?;
        }
        if let Some((out_frame, _)) = bookmark.out && out_frame < bookmark.in_frame {
            return Err(BookmarkError::Invalid("The out frame must not be before the in frame.".to_owned()))
        }

        if let Some(choice) = requested_type(&params)? {
            bookmark.type_id = self.resolve_bookmark_type(choice)?;
        }

        self.bookmarks.edit(&self.settings.bookmarks, |table| {
            table.insert(bookmark);
            Ok(())
        })?;
        self.bookmark_view_by_id(id)
    }

    /// Delete a bookmark.
    pub fn delete_bookmark(&mut self, id: u64, allow_upgrade: bool) -> Result<(), BookmarkError> {
        self.bookmarks.check_editable(&self.settings.bookmarks, allow_upgrade)?;
        self.bookmarks.edit(&self.settings.bookmarks, |table| {
            table.remove(id).map(|_| ()).ok_or_else(|| BookmarkError::NotFound(format!("bookmark {id}")))
        })
    }

    /// Start a range bookmark at the current frame, or end the one started last at the current frame
    /// (swapping its ends if playback went back before its start). Returns the bookmark and whether
    /// it was started.
    pub fn toggle_range_bookmark(&mut self, params: BookmarkParams, allow_upgrade: bool) -> Result<(BookmarkView, bool), BookmarkError> {
        self.bookmarks.check_editable(&self.settings.bookmarks, allow_upgrade)?;

        if let Some(id) = self.bookmarks.open_range && self.bookmarks.table().get(id).is_some() {
            let now = self.bookmark_now()?;
            self.bookmarks.edit(&self.settings.bookmarks, |table| end_range(table, id, now))?;
            self.bookmarks.open_range = None;
            self.bookmarks.changed();
            return Ok((self.bookmark_view_by_id(id)?, false))
        }

        let params = BookmarkParams { frame: None, out: None, ..params };
        let view = self.add_bookmark(params, allow_upgrade)?;
        self.bookmarks.open_range = Some(view.id);
        self.bookmarks.changed();
        Ok((view, true))
    }

    /// Seek playback to a bookmark's in frame, or its out frame.
    pub fn go_to_bookmark(&mut self, id: u64, out_point: bool) -> Result<(), BookmarkError> {
        let bookmark = self.bookmarks.table().get(id).ok_or_else(|| BookmarkError::NotFound(format!("bookmark {id}")))?;
        if self.get_replay_state() != SuperShuckieReplayState::Playback {
            return Err(BookmarkError::WrongState("Seeking to bookmarks works during playback.".to_owned()))
        }
        let frame = if out_point {
            bookmark.out_frame().ok_or_else(|| BookmarkError::Invalid(format!("{} has no out frame.", bookmark.name)))?
        }
        else {
            bookmark.in_frame
        };
        self.go_to_replay_frame(frame.min(u32::MAX as u64) as u32);
        Ok(())
    }

    /// Save unsaved bookmark changes of the replay being played back now.
    pub fn flush_bookmarks(&mut self) -> Result<(), BookmarkError> {
        self.bookmarks.flush()
    }

    /// Hand the latest bookmarks to a recording that is about to stop.
    pub(crate) fn push_bookmarks_to_core(&mut self) {
        if let Some(table) = self.bookmarks.take_core_update() {
            self.core.set_replay_bookmarks(table);
        }
    }

    /// Show `message` to the user on the next tick.
    pub(crate) fn bookmarks_report(&mut self, message: String) {
        self.bookmarks.report = Some(message);
    }

    /// Save pending playback edits before the replay goes away; a failure is reported on the next
    /// tick rather than blocking what the user asked for.
    pub(crate) fn finish_replay_bookmarks(&mut self) {
        self.push_bookmarks_to_core();
        if let Err(e) = self.bookmarks.flush() {
            self.bookmarks.report = Some(format!("Bookmark changes were lost: {e}"));
        }
        self.bookmarks.clear();
    }

    /// Per-tick bookmark work: hand changes to the recording, save playback edits. Returns an error
    /// to show, once.
    pub(crate) fn tick_bookmarks(&mut self) -> Option<String> {
        self.push_bookmarks_to_core();
        self.bookmarks.poll_writes(Instant::now());
        self.bookmarks.take_report()
    }

    /// Add or change a bookmark type (saved to the settings right away).
    pub fn upsert_bookmark_type(&mut self, upsert: BookmarkTypeUpsert) -> Result<BookmarkTypeView, BookmarkError> {
        let name = upsert.name.as_deref().map(|n| clean_name(n).ok_or_else(|| BookmarkError::Invalid("A bookmark type needs a name.".to_owned()))).transpose()?;
        let color = upsert.color.as_deref().map(|c| hex_color::parse(c).ok_or_else(|| BookmarkError::Invalid(format!("{c:?} is not a color (use #RRGGBB).")))).transpose()?;
        let settings = &mut self.settings.bookmarks;

        if let Some(name) = name.as_ref() {
            let editing = upsert.id.as_deref().and_then(hex_type_id::parse);
            if settings.types.iter().any(|t| t.name.to_lowercase() == name.to_lowercase() && Some(t.id) != editing) {
                return Err(BookmarkError::Invalid(format!("There is already a bookmark type named {name}.")))
            }
        }

        let id = match upsert.id.as_deref() {
            Some(text) => {
                let id = hex_type_id::parse(text).filter(|&id| id != 0).ok_or_else(|| BookmarkError::Invalid(format!("{text:?} is not a bookmark type id.")))?;
                match settings.types.iter_mut().find(|t| t.id == id) {
                    Some(t) => {
                        if let Some(name) = name {
                            t.name = name;
                        }
                        if let Some(color) = color {
                            t.color = color;
                        }
                    }
                    None => {
                        // Adopting a type only known from the replay.
                        let record = self.bookmarks.table().type_record(id).ok_or_else(|| BookmarkError::NotFound(format!("bookmark type with id {text}")))?;
                        settings.types.push(BookmarkTypeSetting { id, name: name.unwrap_or_else(|| record.name.clone()), color: color.unwrap_or(record.color) });
                    }
                }
                id
            }
            None => {
                let name = name.ok_or_else(|| BookmarkError::Invalid("A bookmark type needs a name.".to_owned()))?;
                let table = self.bookmarks.table();
                let id = new_type_id(|id| settings.get_type(id).is_some() || table.type_record(id).is_some());
                let color = color.unwrap_or_else(|| palette_color(settings.types.len()));
                settings.types.push(BookmarkTypeSetting { id, name, color });
                id
            }
        };

        self.bookmark_types_changed();
        type_view(&self.settings.bookmarks, self.bookmarks.table(), id).ok_or_else(|| BookmarkError::NotFound("bookmark type".to_owned()))
    }

    /// Remove a bookmark type from the user's types. Bookmarks of that type keep it, and show the
    /// name and color recorded in their replay.
    pub fn delete_bookmark_type(&mut self, id: &str) -> Result<(), BookmarkError> {
        let id = hex_type_id::parse(id).filter(|&id| id != 0).ok_or_else(|| BookmarkError::Invalid(format!("{id:?} is not a bookmark type id.")))?;
        let settings = &mut self.settings.bookmarks;
        let before = settings.types.len();
        settings.types.retain(|t| t.id != id);
        if settings.types.len() == before {
            return Err(BookmarkError::NotFound(format!("bookmark type with id {}", hex_type_id::format(id))))
        }
        if settings.active_type == id {
            settings.active_type = 0;
        }
        self.bookmark_types_changed();
        Ok(())
    }

    /// The type new bookmarks get (hex id), if any.
    pub fn get_active_bookmark_type(&self) -> Option<String> {
        self.settings.bookmarks.get_type(self.settings.bookmarks.active_type).map(|t| hex_type_id::format(t.id))
    }

    /// Set the type new bookmarks get (`None` or `""` for untyped).
    pub fn set_active_bookmark_type(&mut self, id: Option<&str>) -> Result<(), BookmarkError> {
        let id = match id.map(str::trim) {
            None | Some("") => 0,
            Some(text) => hex_type_id::parse(text).ok_or_else(|| BookmarkError::Invalid(format!("{text:?} is not a bookmark type id.")))?
        };
        if id != 0 && self.settings.bookmarks.get_type(id).is_none() {
            return Err(BookmarkError::NotFound(format!("bookmark type with id {}", hex_type_id::format(id))))
        }
        if self.settings.bookmarks.active_type != id {
            self.settings.bookmarks.active_type = id;
            self.bookmark_types_changed();
        }
        Ok(())
    }

    /// Whether to ask before a bookmark change upgrades a pre-v5 replay.
    pub fn get_confirm_replay_upgrade(&self) -> bool {
        self.settings.bookmarks.confirm_replay_upgrade
    }

    /// Set whether to ask before a bookmark change upgrades a pre-v5 replay.
    pub fn set_confirm_replay_upgrade(&mut self, confirm: bool) {
        if self.settings.bookmarks.confirm_replay_upgrade != confirm {
            self.settings.bookmarks.confirm_replay_upgrade = confirm;
            self.write_config();
        }
    }

    /// Serve a bookmark route of the REST API. The API never asks before upgrading a replay.
    pub(crate) fn handle_bookmark_request(&mut self, request: BookmarkRequest) -> BookmarkReply {
        fn json<T: Serialize>(value: &T) -> BookmarkReply {
            serde_json::to_string(value).map(Some).map_err(|e| (500, e.to_string()))
        }
        fn failure(error: BookmarkError) -> (u16, String) {
            (error.http_status(), error.to_string())
        }

        match request {
            BookmarkRequest::List => json(&self.bookmarks_view()),
            BookmarkRequest::Add(params) => json(&self.add_bookmark(params, true).map_err(failure)?),
            BookmarkRequest::Update(id, params) => json(&self.update_bookmark(id, params, true).map_err(failure)?),
            BookmarkRequest::Delete(id) => self.delete_bookmark(id, true).map(|()| None).map_err(failure),
            BookmarkRequest::ToggleRange(params) => {
                #[derive(Serialize)]
                struct Toggled {
                    bookmark: BookmarkView,
                    started: bool
                }
                let (bookmark, started) = self.toggle_range_bookmark(params, true).map_err(failure)?;
                json(&Toggled { bookmark, started })
            }
            BookmarkRequest::GoTo(id, out_point) => self.go_to_bookmark(id, out_point).map(|()| None).map_err(failure)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
    use supershuckie_replay_recorder::replay_file::record::{ReplayFileRecorder, ReplayFileRecorderSettings};
    use supershuckie_replay_recorder::replay_file::{ReplayConsoleType, ReplayFileMetadata, ReplayHeaderRaw};
    use supershuckie_replay_recorder::{ByteVec, InputBuffer, Speed};

    fn bookmark(name: &str, in_frame: u64) -> Bookmark {
        Bookmark { name: name.to_owned(), in_frame, in_millis: (in_frame * 16).into(), ..Default::default() }
    }

    fn settings_with(types: &[(u64, &str, u32)]) -> BookmarkSettings {
        BookmarkSettings { types: types.iter().map(|&(id, name, color)| BookmarkTypeSetting { id, name: name.to_owned(), color }).collect(), ..Default::default() }
    }

    #[test]
    fn generic_names_count_up_per_prefix() {
        let mut table = BookmarkTable::new();
        assert_eq!(generic_name(&table, "Bookmark"), "Bookmark 1");
        table.insert(bookmark("Bookmark 1", 1));
        table.insert(bookmark("Bookmark 7", 2));
        table.insert(bookmark("Bookmarks 9", 3));
        table.insert(bookmark("Death 2", 4));
        assert_eq!(generic_name(&table, "Bookmark"), "Bookmark 8");
        assert_eq!(generic_name(&table, "Death"), "Death 3");
        assert_eq!(generic_name(&table, "Split"), "Split 1");
    }

    #[test]
    fn names_are_cleaned() {
        assert_eq!(clean_name("  Moon\nStone  "), Some("Moon Stone".to_owned()));
        assert_eq!(clean_name(" \t "), None);
        assert_eq!(clean_name(&"x".repeat(500)).unwrap().len(), MAX_NAME_CHARS);
    }

    #[test]
    fn types_resolve_by_id_name_or_replay_record() {
        let mut table = BookmarkTable::new();
        table.set_type_record(BookmarkTypeRecord { id: 0x77, name: "Route".into(), color: 0x123456 });
        table.insert(Bookmark { type_id: 0x77, ..bookmark("r", 5) });
        let mut settings = settings_with(&[(0x55, "Death", 0xE53935)]);

        assert_eq!(resolve_type(&mut settings, &table, TypeChoice::Untyped), Ok((0, false)));
        assert_eq!(resolve_type(&mut settings, &table, TypeChoice::Id(0x55)), Ok((0x55, false)));
        assert_eq!(resolve_type(&mut settings, &table, TypeChoice::Name("  death ".into())), Ok((0x55, false)), "names match case-insensitively");
        assert!(matches!(resolve_type(&mut settings, &table, TypeChoice::Id(0x99)), Err(BookmarkError::NotFound(_))));

        // A type known only from the replay is adopted with its recorded name and color.
        assert_eq!(resolve_type(&mut settings, &table, TypeChoice::Name("route".into())), Ok((0x77, true)));
        assert_eq!(settings.get_type(0x77).map(|t| (t.name.as_str(), t.color)), Some(("Route", 0x123456)));

        // A new name creates a type with a palette color.
        let (id, changed) = resolve_type(&mut settings, &table, TypeChoice::Name("Split".into())).unwrap();
        assert!(changed && id != 0 && id != 0x55 && id != 0x77);
        assert_eq!(settings.get_type(id).unwrap().color, palette_color(2));
    }

    #[test]
    fn type_records_follow_the_settings() {
        let mut table = BookmarkTable::new();
        table.insert(Bookmark { type_id: 0x55, ..bookmark("a", 1) });
        table.insert(Bookmark { type_id: 0x66, ..bookmark("b", 2) });
        table.set_type_record(BookmarkTypeRecord { id: 0x55, name: "Old name".into(), color: 1 });
        table.set_type_record(BookmarkTypeRecord { id: 0x66, name: "Foreign".into(), color: 2 });
        table.set_type_record(BookmarkTypeRecord { id: 0x88, name: "Unused".into(), color: 3 });

        refresh_type_records(&mut table, &settings_with(&[(0x55, "Death", 0xE53935)]));
        assert_eq!(table.types, vec![
            BookmarkTypeRecord { id: 0x55, name: "Death".into(), color: 0xE53935 },
            BookmarkTypeRecord { id: 0x66, name: "Foreign".into(), color: 2 },
        ]);
    }

    #[test]
    fn palette_colors_are_distinct_and_in_range() {
        let colors: BTreeSet<u32> = (0..24).map(palette_color).collect();
        assert_eq!(colors.len(), 24);
        assert!(colors.iter().all(|&c| c <= 0xFF_FFFF));
    }

    #[test]
    fn out_parameters_parse() {
        let out = |text: &str| requested_out(&BookmarkParams { out: Some(text.to_owned()), ..Default::default() });
        assert_eq!(out("now"), Ok(Some(OutChoice::Now)));
        assert_eq!(out("none"), Ok(Some(OutChoice::Clear)));
        assert_eq!(out(" 1200 "), Ok(Some(OutChoice::Frame(1200))));
        assert!(matches!(out("later"), Err(BookmarkError::Invalid(_))));
        assert_eq!(requested_out(&BookmarkParams::default()), Ok(None));
    }

    #[test]
    fn ending_a_range_before_its_start_swaps_the_ends() {
        let mut table = BookmarkTable::new();
        let id = table.insert(Bookmark { keyframe: true, ..bookmark("r", 100) });
        end_range(&mut table, id, (150, 2400.into())).unwrap();
        assert_eq!(table.get(id).unwrap().out, Some((150, 2400.into())));

        let id = table.insert(Bookmark { keyframe: true, ..bookmark("back", 100) });
        end_range(&mut table, id, (40, 640.into())).unwrap();
        let b = table.get(id).unwrap();
        assert_eq!((b.in_frame, b.out, b.keyframe), (40, Some((100, 1600.into())), false));
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("supershuckie-frontend-bookmarks-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A closed replay of a few frames, written with `version` in its header (a v3/v4 layout has
    /// no stream end and no section).
    fn replay_file(dir: &std::path::Path, name: &str, version: u32) -> (PathBuf, LoadedReplay) {
        let metadata = ReplayFileMetadata { console_type: ReplayConsoleType::GameBoy, rom_name: "TEST".into(), emulator_core_name: "test".into(), ..Default::default() };
        let mut recorder = ReplayFileRecorder::new_with_metadata(metadata, ByteVec::new(), ReplayFileRecorderSettings::default(), 0.into(), InputBuffer::new(), Speed::default(), ByteVec::from(&[1u8; 64][..]), Vec::<u8>::new(), Vec::<u8>::new()).unwrap();
        for frame in 1..=30u64 {
            recorder.next_frame((frame * 16).into()).unwrap();
        }
        let (mut bytes, _) = recorder.close().unwrap();
        if version < REPLAY_VERSION_BOOKMARK_SECTION {
            let stream_end = ReplayHeaderRaw::from_bytes(bytes[..2048].try_into().unwrap()).packet_stream_end().unwrap() as usize;
            bytes.truncate(stream_end);
            bytes[4..8].copy_from_slice(&version.to_le_bytes());
            bytes[0x3A8..0x3B0].fill(0);
        }
        let path = dir.join(format!("{name}.replay"));
        std::fs::write(&path, &bytes).unwrap();
        let player = ReplayFilePlayer::new(&bytes, false).unwrap();
        let loaded = LoadedReplay { name: name.to_owned(), path: path.clone(), header: player.raw_header_bytes(), version: player.get_replay_version(), truncated: player.stream_truncated() };
        (path, loaded)
    }

    fn table_on_disk(path: &std::path::Path) -> BookmarkTable {
        ReplayFilePlayer::new(std::fs::read(path).unwrap(), false).unwrap().bookmark_table().clone()
    }

    #[test]
    fn playback_edits_are_written_after_a_pause() {
        let dir = temp_dir("write");
        let (path, loaded) = replay_file(&dir, "run", REPLAY_VERSION_BOOKMARK_SECTION);
        let settings = BookmarkSettings::default();
        let mut bookmarks = ReplayBookmarks::new();
        bookmarks.set_playback(loaded, BookmarkTable::new());

        bookmarks.check_editable(&settings, false).unwrap();
        bookmarks.edit(&settings, |t| Ok(t.insert(bookmark("first", 10)))).unwrap();
        bookmarks.edit(&settings, |t| Ok(t.insert(bookmark("second", 20)))).unwrap();

        // Nothing is written until the changes settle.
        let start = Instant::now();
        bookmarks.poll_writes(start);
        assert!(bookmarks.in_flight.is_none());
        assert!(table_on_disk(&path).is_empty());

        bookmarks.poll_writes(start + WRITE_DELAY * 2);
        assert!(bookmarks.in_flight.is_some());
        bookmarks.flush().unwrap();
        assert_eq!(table_on_disk(&path), *bookmarks.table());
        assert_eq!(bookmarks.saved_serial, bookmarks.edit_serial);

        // An edit then a flush without waiting also lands.
        bookmarks.edit(&settings, |t| t.remove(1).map(|_| ()).ok_or(BookmarkError::NotFound("1".into()))).unwrap();
        bookmarks.flush().unwrap();
        assert_eq!(table_on_disk(&path).bookmarks.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(), ["second"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_replays_ask_before_the_upgrade() {
        let dir = temp_dir("upgrade");
        let (path, loaded) = replay_file(&dir, "old", 4);
        let mut settings = BookmarkSettings::default();
        let mut bookmarks = ReplayBookmarks::new();
        bookmarks.set_playback(loaded, BookmarkTable::new());

        assert_eq!(bookmarks.check_editable(&settings, false), Err(BookmarkError::NeedsUpgradeConfirmation { version: 4 }));
        bookmarks.check_editable(&settings, true).unwrap();
        bookmarks.check_editable(&settings, false).expect("confirmed once for this replay");

        bookmarks.edit(&settings, |t| Ok(t.insert(bookmark("mark", 3)))).unwrap();
        bookmarks.flush().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let version = ReplayHeaderRaw::from_bytes(bytes[..2048].try_into().unwrap()).replay_version;
        assert_eq!(version, REPLAY_VERSION_BOOKMARK_SECTION);
        assert!(matches!(&bookmarks.target, Target::Playback(p) if p.version == REPLAY_VERSION_BOOKMARK_SECTION));

        // The next write uses the upgraded header.
        bookmarks.edit(&settings, |t| Ok(t.insert(bookmark("again", 4)))).unwrap();
        bookmarks.flush().unwrap();
        assert_eq!(table_on_disk(&path).bookmarks.len(), 2);

        // Not asking at all when the user turned the question off.
        let (_, loaded) = replay_file(&dir, "old2", 3);
        settings.confirm_replay_upgrade = false;
        bookmarks.set_playback(loaded, BookmarkTable::new());
        bookmarks.check_editable(&settings, false).unwrap();

        // v2 and damaged replays are read-only.
        let (_, mut loaded) = replay_file(&dir, "older", 3);
        loaded.version = 2;
        bookmarks.set_playback(loaded, BookmarkTable::new());
        assert_eq!(bookmarks.check_editable(&settings, true), Err(BookmarkError::NeedsConversion { version: 2 }));
        let (_, mut loaded) = replay_file(&dir, "damaged", REPLAY_VERSION_BOOKMARK_SECTION);
        loaded.truncated = true;
        bookmarks.set_playback(loaded, BookmarkTable::new());
        assert_eq!(bookmarks.check_editable(&settings, true), Err(BookmarkError::Damaged));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_writes_are_reported_once_and_retried_on_the_next_change() {
        let dir = temp_dir("fail");
        let (path, loaded) = replay_file(&dir, "gone", REPLAY_VERSION_BOOKMARK_SECTION);
        let settings = BookmarkSettings::default();
        let mut bookmarks = ReplayBookmarks::new();
        bookmarks.set_playback(loaded, BookmarkTable::new());
        std::fs::remove_file(&path).unwrap();

        bookmarks.edit(&settings, |t| Ok(t.insert(bookmark("x", 1)))).unwrap();
        assert!(matches!(bookmarks.flush(), Err(BookmarkError::WriteFailed(_))));
        assert!(bookmarks.take_report().is_some());
        assert!(bookmarks.write_error.is_some());
        assert!(matches!(bookmarks.flush(), Err(BookmarkError::WriteFailed(_))), "still unsaved");
        assert!(bookmarks.take_report().is_none(), "the same failure is reported once");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recording_edits_go_to_the_core_once() {
        let settings = BookmarkSettings::default();
        let mut bookmarks = ReplayBookmarks::new();
        bookmarks.set_recording("live".into(), BookmarkTable::new());
        assert!(bookmarks.take_core_update().is_none());

        let generation = bookmarks.generation();
        bookmarks.edit(&settings, |t| Ok(t.insert(bookmark("x", 1)))).unwrap();
        bookmarks.edit(&settings, |t| Ok(t.insert(bookmark("y", 2)))).unwrap();
        assert!(bookmarks.generation() > generation);
        assert_eq!(bookmarks.take_core_update().map(|t| t.bookmarks.len()), Some(2));
        assert!(bookmarks.take_core_update().is_none());

        // A no-op edit changes nothing.
        let generation = bookmarks.generation();
        bookmarks.edit(&settings, |_| Ok(())).unwrap();
        assert_eq!(bookmarks.generation(), generation);
        assert!(bookmarks.take_core_update().is_none());
    }
}
