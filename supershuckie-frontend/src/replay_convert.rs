//! Background conversion of replay files to the current format, driven from the UI.
//!
//! The UI first builds a [`ConversionPlan`] for a file or a folder (which lists what would be
//! converted and what is skipped), then starts a [`ConversionJob`] that works through the plan on
//! its own thread: each replay is converted into a temporary file next to it and verified, and only
//! then is the original replaced (kept as a `.bak` if asked). The UI polls
//! [`ConversionJob::status`] for progress, may [`cancel`](ConversionJob::cancel), and reads the
//! [`ConversionSummary`] when the job is [`finished`](ConversionJob::is_finished).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use supershuckie_replay_recorder::replay_file::convert::{
    convert_replay_file, human_size, replay_file_version, verify_replay_files, ConvertError, ConvertOptions,
};
pub use supershuckie_replay_recorder::replay_file::convert::ConvertPhase;
use supershuckie_replay_recorder::replay_file::record::ReplayFileRecorderSettings;
use supershuckie_replay_recorder::replay_file::REPLAY_VERSION;

/// File extension of replays.
const REPLAY_EXTENSION: &str = "replay";

/// Suffix of the temporary output written next to a replay while it is being converted.
const TEMP_SUFFIX: &str = ".v4-tmp.replay";

/// Replays written more recently than this are assumed to be recordings in progress and skipped.
const MIN_AGE: Duration = Duration::from_secs(10 * 60);

/// What a conversion would do, built from a file or folder before anything is touched.
#[derive(Clone, Debug, Default)]
pub struct ConversionPlan {
    /// The file or folder the plan was built from.
    pub root: PathBuf,
    /// Replays to convert, in path order.
    pub files: Vec<PathBuf>,
    /// Their combined size.
    pub total_bytes: u64,
    /// Replays skipped because they already are the current format.
    pub skipped_current: usize,
    /// Replays skipped because they were written in the last few minutes (probably recording).
    pub skipped_recent: usize,
    /// Replays skipped because they are excluded (the recording in progress) or unreadable.
    pub skipped_unreadable: usize,
}

impl ConversionPlan {
    /// One-line description for a confirmation dialog.
    pub fn describe(&self) -> String {
        let mut text = if self.files.len() == 1 {
            format!("1 replay ({}) to convert.", human_size(self.total_bytes))
        }
        else {
            format!("{} replays ({}) to convert.", self.files.len(), human_size(self.total_bytes))
        };

        let mut skipped = Vec::new();
        if self.skipped_current > 0 {
            skipped.push(format!("{} already format v{REPLAY_VERSION}", self.skipped_current));
        }
        if self.skipped_recent > 0 {
            skipped.push(format!("{} written in the last 10 minutes", self.skipped_recent));
        }
        if self.skipped_unreadable > 0 {
            skipped.push(format!("{} unreadable or in use", self.skipped_unreadable));
        }
        if !skipped.is_empty() {
            text.push_str(&format!(" Skipped: {}.", skipped.join(", ")));
        }
        text
    }
}

/// Classify one replay file for the plan.
fn plan_file(plan: &mut ConversionPlan, path: PathBuf, exclude: &[PathBuf]) {
    if exclude.iter().any(|e| e == &path) {
        plan.skipped_unreadable += 1;
        return;
    }

    let Ok(metadata) = std::fs::metadata(&path) else {
        plan.skipped_unreadable += 1;
        return;
    };

    if metadata.modified().ok().and_then(|m| m.elapsed().ok()).is_some_and(|age| age < MIN_AGE) {
        plan.skipped_recent += 1;
        return;
    }

    match replay_file_version(&path) {
        Ok(version) if version >= REPLAY_VERSION => plan.skipped_current += 1,
        Ok(_) => {
            plan.total_bytes += metadata.len();
            plan.files.push(path);
        }
        Err(_) => plan.skipped_unreadable += 1,
    }
}

fn is_replay_file(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == REPLAY_EXTENSION)
        && !path.to_string_lossy().ends_with(TEMP_SUFFIX)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, out);
        }
        else if path.is_file() && is_replay_file(&path) {
            out.push(path);
        }
    }
}

/// Build the plan for `path`: a single replay, or every `*.replay` under a folder (recursively).
///
/// `exclude` lists files that must never be converted (the recording in progress).
pub fn plan_conversion(path: &Path, exclude: &[PathBuf]) -> Result<ConversionPlan, String> {
    let mut plan = ConversionPlan { root: path.to_path_buf(), ..Default::default() };

    if path.is_file() {
        if !is_replay_file(path) {
            return Err(format!("{} is not a .replay file", path.display()));
        }
        plan_file(&mut plan, path.to_path_buf(), exclude);
        if plan.files.is_empty() {
            return match (plan.skipped_current, plan.skipped_recent) {
                (1, _) => Err(format!("{} is already format v{REPLAY_VERSION}.", path.display())),
                (_, 1) => Err(format!("{} was written in the last 10 minutes; if it is being recorded, stop the recording first.", path.display())),
                _ => Err(format!("{} could not be read as a replay.", path.display())),
            };
        }
    }
    else if path.is_dir() {
        let mut files = Vec::new();
        walk(path, &mut files);
        files.sort();
        for file in files {
            plan_file(&mut plan, file, exclude);
        }
    }
    else {
        return Err(format!("{} does not exist", path.display()));
    }

    Ok(plan)
}

/// Progress of a running [`ConversionJob`].
#[derive(Clone, Debug, PartialEq)]
pub struct ConversionStatus {
    /// Index of the replay being worked on (0-based).
    pub file_index: usize,
    /// Number of replays in the plan.
    pub file_count: usize,
    /// File name of the replay being worked on.
    pub current_name: String,
    /// What is being done to it.
    pub phase: ConvertPhase,
    /// Frames done in the current phase.
    pub done: u64,
    /// Frames total in the current phase.
    pub total: u64,
}

/// One converted replay.
#[derive(Clone, Debug, PartialEq)]
pub struct ConvertedReplay {
    pub path: PathBuf,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

/// Outcome of a [`ConversionJob`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConversionSummary {
    pub converted: Vec<ConvertedReplay>,
    pub failed: Vec<(PathBuf, String)>,
    pub cancelled: bool,
    pub planned: usize,
    pub kept_backups: bool,
    pub elapsed: Duration,
}

impl ConversionSummary {
    /// Multi-line summary for a message box.
    pub fn describe(&self) -> String {
        let before: u64 = self.converted.iter().map(|c| c.bytes_before).sum();
        let after: u64 = self.converted.iter().map(|c| c.bytes_after).sum();
        let seconds = self.elapsed.as_secs();

        let mut text = String::new();
        if self.cancelled {
            text.push_str("Cancelled. ");
        }
        text.push_str(&format!(
            "Converted {} of {} replay{}: {} -> {}",
            self.converted.len(),
            self.planned,
            if self.planned == 1 { "" } else { "s" },
            human_size(before),
            human_size(after)
        ));
        if after > 0 {
            text.push_str(&format!(" ({:.1}x smaller)", before as f64 / after as f64));
        }
        text.push_str(&format!(" in {}:{:02}:{:02}.", seconds / 3600, (seconds / 60) % 60, seconds % 60));

        if self.kept_backups && !self.converted.is_empty() {
            text.push_str("\n\nThe originals were kept next to the converted files as .replay.bak files; delete them once you are happy with the results.");
        }

        if !self.failed.is_empty() {
            text.push_str(&format!("\n\n{} FAILED (originals untouched):", self.failed.len()));
            for (path, error) in self.failed.iter().take(10) {
                text.push_str(&format!("\n  {}: {}", path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), error));
            }
            if self.failed.len() > 10 {
                text.push_str(&format!("\n  ... and {} more", self.failed.len() - 10));
            }
        }

        text
    }
}

struct Shared {
    cancel: AtomicBool,
    finished: AtomicBool,
    file_index: AtomicUsize,
    file_count: usize,
    phase: AtomicU8,
    done: AtomicU64,
    total: AtomicU64,
    current_name: Mutex<String>,
}

/// A conversion running on its own thread.
pub struct ConversionJob {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<ConversionSummary>>,
}

impl ConversionJob {
    /// Start converting every file in `plan` with `settings`, replacing each original after its
    /// verification passes (renamed to `<name>.replay.bak` when `keep_backups`).
    pub fn start(plan: ConversionPlan, settings: ReplayFileRecorderSettings, keep_backups: bool) -> Self {
        let shared = Arc::new(Shared {
            cancel: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            file_index: AtomicUsize::new(0),
            file_count: plan.files.len(),
            phase: AtomicU8::new(0),
            done: AtomicU64::new(0),
            total: AtomicU64::new(0),
            current_name: Mutex::new(String::new()),
        });

        let worker = shared.clone();
        let thread = std::thread::Builder::new()
            .name("replay-conversion".to_owned())
            .spawn(move || run_job(worker, plan, settings, keep_backups))
            .expect("failed to spawn the replay conversion thread");

        Self { shared, thread: Some(thread) }
    }

    /// Current progress.
    pub fn status(&self) -> ConversionStatus {
        ConversionStatus {
            file_index: self.shared.file_index.load(Ordering::Relaxed),
            file_count: self.shared.file_count,
            current_name: self.shared.current_name.lock().map(|n| n.clone()).unwrap_or_default(),
            phase: if self.shared.phase.load(Ordering::Relaxed) == 1 { ConvertPhase::Verifying } else { ConvertPhase::Converting },
            done: self.shared.done.load(Ordering::Relaxed),
            total: self.shared.total.load(Ordering::Relaxed),
        }
    }

    /// Ask the job to stop after cleaning up the replay it is working on.
    pub fn cancel(&self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
    }

    /// Whether the worker thread has finished (successfully, with failures, or cancelled).
    pub fn is_finished(&self) -> bool {
        self.shared.finished.load(Ordering::Acquire)
    }

    /// Wait for the job and return its summary.
    pub fn finish(mut self) -> ConversionSummary {
        match self.thread.take().map(|t| t.join()) {
            Some(Ok(summary)) => summary,
            _ => ConversionSummary { failed: vec![(PathBuf::new(), "the conversion thread panicked".to_owned())], ..Default::default() },
        }
    }
}

impl Drop for ConversionJob {
    fn drop(&mut self) {
        // Never leave the worker replacing files behind our back.
        self.cancel();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run_job(shared: Arc<Shared>, plan: ConversionPlan, settings: ReplayFileRecorderSettings, keep_backups: bool) -> ConversionSummary {
    let started = Instant::now();
    let options = ConvertOptions { settings, allow_corruption: false };
    let mut summary = ConversionSummary { planned: plan.files.len(), kept_backups: keep_backups, ..Default::default() };

    for (index, path) in plan.files.iter().enumerate() {
        if shared.cancel.load(Ordering::Relaxed) {
            summary.cancelled = true;
            break;
        }

        shared.file_index.store(index, Ordering::Relaxed);
        shared.phase.store(0, Ordering::Relaxed);
        shared.done.store(0, Ordering::Relaxed);
        shared.total.store(0, Ordering::Relaxed);
        if let Ok(mut name) = shared.current_name.lock() {
            *name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        }

        match convert_one(&shared, path, &options, keep_backups) {
            Ok(converted) => summary.converted.push(converted),
            Err(ConvertError::Cancelled) => {
                summary.cancelled = true;
                break;
            }
            Err(ConvertError::Failed(message)) => summary.failed.push((path.clone(), message)),
        }
    }

    summary.elapsed = started.elapsed();
    shared.finished.store(true, Ordering::Release);
    summary
}

/// Convert, verify and replace one replay.
fn convert_one(shared: &Shared, path: &Path, options: &ConvertOptions, keep_backup: bool) -> Result<ConvertedReplay, ConvertError> {
    let temp = temp_path(path);
    let _ = std::fs::remove_file(&temp);

    let mut progress = |phase: ConvertPhase, done: u64, total: u64| -> bool {
        shared.phase.store(if phase == ConvertPhase::Verifying { 1 } else { 0 }, Ordering::Relaxed);
        shared.done.store(done, Ordering::Relaxed);
        shared.total.store(total, Ordering::Relaxed);
        !shared.cancel.load(Ordering::Relaxed)
    };

    let report = convert_replay_file(path, &temp, options, true, &mut progress)?;

    if let Err(error) = verify_replay_files(path, &temp, options.settings.mask_transient_buffers, false, &mut progress) {
        let _ = std::fs::remove_file(&temp);
        return Err(match error {
            ConvertError::Cancelled => ConvertError::Cancelled,
            ConvertError::Failed(message) => ConvertError::Failed(format!("verification failed: {message}")),
        });
    }

    if let Err(message) = replace_original(path, &temp, keep_backup) {
        let _ = std::fs::remove_file(&temp);
        return Err(ConvertError::Failed(message));
    }

    Ok(ConvertedReplay { path: path.to_path_buf(), bytes_before: report.source.size, bytes_after: report.output_size })
}

fn temp_path(path: &Path) -> PathBuf {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!("{stem}{TEMP_SUFFIX}"))
}

/// First free `<name>.replay.bak`, `<name>.replay.1.bak`, ...
fn backup_path(path: &Path) -> PathBuf {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut candidate = path.with_file_name(format!("{name}.bak"));
    let mut n = 1;
    while candidate.exists() {
        candidate = path.with_file_name(format!("{name}.{n}.bak"));
        n += 1;
    }
    candidate
}

/// Rename with a few retries: Dropbox and antivirus scanners briefly hold freshly written files.
fn rename_with_retry(from: &Path, to: &Path) -> Result<(), String> {
    let mut last_error = None;
    for attempt in 0..5 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_secs(attempt));
        }
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) => last_error = Some(e),
        }
    }
    Err(format!("cannot rename {} to {}: {}", from.display(), to.display(), last_error.map(|e| e.to_string()).unwrap_or_default()))
}

/// Put the verified `temp` in place of `original` (moving the original to a `.bak` first, or
/// deleting it). If the final rename fails, the original is restored.
fn replace_original(original: &Path, temp: &Path, keep_backup: bool) -> Result<(), String> {
    if keep_backup {
        let backup = backup_path(original);
        rename_with_retry(original, &backup)?;
        if let Err(error) = rename_with_retry(temp, original) {
            let _ = rename_with_retry(&backup, original);
            return Err(error);
        }
    }
    else {
        std::fs::remove_file(original).map_err(|e| format!("cannot delete {}: {e}", original.display()))?;
        rename_with_retry(temp, original)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const V3_SMALL_CLOSED: &[u8] = include_bytes!("../../supershuckie-replay-recorder/tests/fixtures/v3-small-closed.replay");
    const V3_SMALL: &[u8] = include_bytes!("../../supershuckie-replay-recorder/tests/fixtures/v3-small.replay");

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("supershuckie-frontend-convert-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Set a file's modified time back so it does not count as "recent".
    fn age(path: &Path) {
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_modified(std::time::SystemTime::now() - Duration::from_secs(3600)).unwrap();
    }

    fn wait(job: ConversionJob) -> ConversionSummary {
        let started = Instant::now();
        while !job.is_finished() {
            assert!(started.elapsed() < Duration::from_secs(60), "conversion job hung");
            std::thread::sleep(Duration::from_millis(10));
        }
        job.finish()
    }

    #[test]
    fn folder_plan_skips_current_recent_excluded_and_junk() {
        let dir = temp_dir("plan");
        let replays = dir.join("TEST.gb-data").join("replays");
        std::fs::create_dir_all(&replays).unwrap();

        let old = replays.join("old.replay");
        std::fs::write(&old, V3_SMALL_CLOSED).unwrap();
        age(&old);
        let temp_layout = replays.join("temp-layout.replay");
        std::fs::write(&temp_layout, V3_SMALL).unwrap();
        age(&temp_layout);
        let recent = replays.join("recent.replay");
        std::fs::write(&recent, V3_SMALL_CLOSED).unwrap();
        let excluded = replays.join("recording.replay");
        std::fs::write(&excluded, V3_SMALL_CLOSED).unwrap();
        age(&excluded);
        let junk = replays.join("junk.replay");
        std::fs::write(&junk, b"nope").unwrap();
        age(&junk);
        std::fs::write(replays.join("notes.txt"), b"ignored").unwrap();
        std::fs::write(replays.join("leftover.v4-tmp.replay"), V3_SMALL_CLOSED).unwrap();

        // A v4 file: convert one first.
        let current = replays.join("current.replay");
        let job = ConversionJob::start(plan_conversion(&old, &[]).unwrap(), ReplayFileRecorderSettings::default(), false);
        let summary = wait(job);
        assert_eq!(summary.converted.len(), 1);
        std::fs::copy(&old, &current).unwrap();
        age(&current);
        age(&old);

        let plan = plan_conversion(&dir, &[excluded.clone()]).unwrap();
        assert_eq!(plan.files, vec![temp_layout.clone()]);
        assert_eq!(plan.skipped_current, 2, "old (now converted) and current");
        assert_eq!(plan.skipped_recent, 1);
        assert_eq!(plan.skipped_unreadable, 2, "excluded + junk");
        assert!(plan.describe().starts_with("1 replay ("));

        // Single-file plans explain why nothing would be done.
        assert!(plan_conversion(&current, &[]).unwrap_err().contains("already format v4"));
        assert!(plan_conversion(&recent, &[]).unwrap_err().contains("last 10 minutes"));
        assert!(plan_conversion(&junk, &[]).unwrap_err().contains("could not be read"));
        assert!(plan_conversion(&replays.join("notes.txt"), &[]).is_err());
        assert!(plan_conversion(&replays.join("missing.replay"), &[]).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn job_replaces_originals_and_keeps_backups() {
        let dir = temp_dir("job");
        let a = dir.join("a.replay");
        let b = dir.join("b.replay");
        let bad = dir.join("bad.replay");
        std::fs::write(&a, V3_SMALL_CLOSED).unwrap();
        std::fs::write(&b, V3_SMALL).unwrap();
        // Parses as a replay header but is cut inside a blob, so conversion fails.
        std::fs::write(&bad, &V3_SMALL_CLOSED[..V3_SMALL_CLOSED.len() * 2 / 3]).unwrap();
        for p in [&a, &b, &bad] {
            age(p);
        }
        // An existing .bak must not be overwritten.
        std::fs::write(dir.join("a.replay.bak"), b"older backup").unwrap();

        let plan = plan_conversion(&dir, &[]).unwrap();
        assert_eq!(plan.files.len(), 3);

        let job = ConversionJob::start(plan, ReplayFileRecorderSettings { max_frames_per_blob: 60, ..Default::default() }, true);
        let summary = wait(job);

        assert_eq!(summary.converted.len(), 2, "{summary:?}");
        assert_eq!(summary.failed.len(), 1);
        assert_eq!(summary.failed[0].0, bad);
        assert!(!summary.cancelled);
        assert!(summary.describe().contains("Converted 2 of 3 replays"));
        assert!(summary.describe().contains("FAILED"));

        // Converted files are v4 and smaller; originals sit in .bak files; the bad one is untouched.
        assert_eq!(replay_file_version(&a).unwrap(), REPLAY_VERSION);
        assert_eq!(replay_file_version(&b).unwrap(), REPLAY_VERSION);
        assert!(std::fs::metadata(&a).unwrap().len() < V3_SMALL_CLOSED.len() as u64);
        assert_eq!(std::fs::read(dir.join("a.replay.bak")).unwrap(), b"older backup");
        assert_eq!(std::fs::read(dir.join("a.replay.1.bak")).unwrap(), V3_SMALL_CLOSED);
        assert_eq!(std::fs::read(dir.join("b.replay.bak")).unwrap(), V3_SMALL);
        assert_eq!(std::fs::read(&bad).unwrap().len(), V3_SMALL_CLOSED.len() * 2 / 3);
        assert!(!dir.join("bad.v4-tmp.replay").exists());
        assert!(!dir.join("a.v4-tmp.replay").exists());

        // Without backups the original is simply replaced.
        let c = dir.join("c.replay");
        std::fs::write(&c, V3_SMALL_CLOSED).unwrap();
        age(&c);
        let summary = wait(ConversionJob::start(plan_conversion(&c, &[]).unwrap(), ReplayFileRecorderSettings::default(), false));
        assert_eq!(summary.converted.len(), 1);
        assert!(!dir.join("c.replay.bak").exists());
        assert_eq!(replay_file_version(&c).unwrap(), REPLAY_VERSION);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancelling_leaves_the_current_file_untouched() {
        let dir = temp_dir("cancel");
        let files: Vec<PathBuf> = (0..3).map(|i| dir.join(format!("{i}.replay"))).collect();
        for f in &files {
            std::fs::write(f, V3_SMALL_CLOSED).unwrap();
            age(f);
        }

        let plan = plan_conversion(&dir, &[]).unwrap();
        let job = ConversionJob::start(plan, ReplayFileRecorderSettings::default(), true);
        job.cancel();
        let summary = wait(job);
        assert!(summary.cancelled);

        // Whatever was not converted is still the original v3 file, and nothing temporary remains.
        for f in &files {
            let version = replay_file_version(f).unwrap();
            if version < REPLAY_VERSION {
                assert_eq!(std::fs::read(f).unwrap(), V3_SMALL_CLOSED);
            }
        }
        assert!(std::fs::read_dir(&dir).unwrap().flatten().all(|e| !e.path().to_string_lossy().ends_with(TEMP_SUFFIX)));
        assert!(summary.converted.len() + summary.failed.len() < 3 || summary.cancelled);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
