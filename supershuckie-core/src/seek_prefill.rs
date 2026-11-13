//! Filling a replay's seek index (see `seek_index`) ahead of the seeks that will want it.
//!
//! The index on its own only knows the stretches of a replay the emulator has already walked
//! through, so the first seek into any two seconds of a recording still emulates from the
//! recording's keyframe. A replay is deterministic, though, and nothing says the walking has to
//! be done by the emulator the user is looking at: a [`ReplaySeekPrefill`] runs a few more
//! emulators of the same game on threads of their own, each with its own player of the same
//! file, which walk the recording without drawing it, nearest the user's position first, and
//! hand every snapshot they take to the emulator that shows the replay
//! ([`SuperShuckieCore::set_replay_seek_prefill`]). That one picks them up at its next seek.
//!
//! Measured on Nintendo DS recordings (2026-11-11): one worker walks about 400 frames a second
//! with snapshots every 15 frames, so an hour of recording is filled in about nine minutes by one
//! worker and the neighbourhood of the cursor within a second or two; a snapshot is 3-6 KB, so
//! the hour costs 45-90 MB of the index's budget (a recording from before 2026-10-11, whose
//! keyframes lack what states have carried since, costs about 40 KB a snapshot instead). Once
//! the budget is nearly used the workers only walk what is close to the user's position, and
//! the index drops what was put in longest ago and never used.

use crate::emulator::EmulatorCore;
use crate::seek_index::{Point, DENSE_SPACING};
use crate::seek_store::{ReplaySeekIndexFile, TrackedFile};
use alloc::collections::BTreeMap;
use crate::{std_timestamp_provider, SuperShuckieCore};
use alloc::boxed::Box;
use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::Duration;
use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::UnsignedInteger;

/// Makes one worker's emulator and its player of the replay, on the worker's own thread (an
/// emulator can take a while to set up, and nobody should wait for it).
pub type ReplaySeekPrefillWorker = Box<dyn FnOnce() -> Result<(Box<dyn EmulatorCore>, ReplayFilePlayer), String> + Send>;

/// How many keyframe intervals in a row a worker takes at a time. Starting anywhere costs a
/// keyframe load, so a worker walks on from where it is; taking a run at once keeps two workers
/// from leapfrogging each other one interval at a time. Also how long (about two seconds of
/// work on a Nintendo DS) a worker takes to follow the user somewhere else.
const RUN_INTERVALS: usize = 8;

/// When this much of the index's budget is used (out of 8), the workers only walk intervals
/// within [`NEAR_INTERVALS`] of the user's position: each snapshot from then on pushes an older
/// one out, which is worth it next to the cursor and nowhere else.
const FULL_EIGHTHS: usize = 7;

/// See [`FULL_EIGHTHS`]: this many keyframe intervals either side of the user's position (a
/// minute of a recording with a keyframe every two seconds) are where a run may start.
const NEAR_INTERVALS: usize = 30;

/// How long a worker with nothing to do (the index is full and everything near the user is
/// walked) sleeps before looking again.
const IDLE_SLEEP: Duration = Duration::from_millis(250);

#[derive(Default)]
struct Queue {
    /// Snapshots made and not yet picked up: keyframe, frame, snapshot, and whether it was just
    /// made (rather than read from the index's file).
    ready: Vec<(UnsignedInteger, UnsignedInteger, Point, bool)>,
    /// Keyframe intervals a worker has taken (walked or being walked), or that the index's file
    /// already had.
    claimed: BTreeSet<UnsignedInteger>,
    /// How many snapshots the index's file had for each keyframe interval.
    from_file: BTreeMap<UnsignedInteger, usize>,
    errors: Vec<String>,
}

/// What the workers and the emulator they work for share.
pub(crate) struct Shared {
    stop: AtomicBool,
    /// The frame the user is at: intervals nearest after it are walked first.
    focus: AtomicU64,
    /// Bytes the consumer's index holds, and its budget, as of its last pick-up.
    consumer_used: AtomicUsize,
    budget: AtomicUsize,
    /// Bytes of the snapshots in `queue.ready`.
    ready_bytes: AtomicUsize,
    intervals_total: AtomicUsize,
    intervals_done: AtomicUsize,
    frames_walked: AtomicU64,
    workers_running: AtomicUsize,
    /// The index's file is still being read; the workers wait for it.
    loading: AtomicBool,
    /// The file held every interval of the replay: nothing is left to walk.
    file_complete: AtomicBool,
    queue: Mutex<Queue>,
}

impl Shared {
    fn new(workers: usize, loading: bool) -> Self {
        Self {
            stop: AtomicBool::new(false),
            focus: AtomicU64::new(0),
            consumer_used: AtomicUsize::new(0),
            budget: AtomicUsize::new(SuperShuckieCore::DEFAULT_SEEK_INDEX_BUDGET),
            ready_bytes: AtomicUsize::new(0),
            intervals_total: AtomicUsize::new(0),
            intervals_done: AtomicUsize::new(0),
            frames_walked: AtomicU64::new(0),
            workers_running: AtomicUsize::new(workers),
            loading: AtomicBool::new(loading),
            file_complete: AtomicBool::new(false),
            queue: Mutex::new(Queue::default()),
        }
    }

    /// Whether every keyframe interval of the replay has been walked (now or, going by the
    /// index's file, some time before).
    fn all_walked(&self) -> bool {
        if self.loading.load(Ordering::Relaxed) {
            return false
        }
        let total = self.intervals_total.load(Ordering::Relaxed);
        self.file_complete.load(Ordering::Relaxed) || (total > 0 && self.intervals_done.load(Ordering::Relaxed) >= total)
    }

    /// Take the intervals the index's file already covers off the list: an interval counts when
    /// the file had (all but one of) the snapshots a walk through it takes. `keyframes` and
    /// `total_frames` are the replay's. Any worker may call it, any number of times.
    fn skip_what_the_file_had(&self, keyframes: &[UnsignedInteger], total_frames: UnsignedInteger) {
        let mut queue = self.lock();
        if queue.from_file.is_empty() {
            return
        }
        let mut skipped = 0;
        for (i, keyframe) in keyframes.iter().enumerate() {
            let end = keyframes.get(i + 1).copied().unwrap_or(total_frames);
            let expected = (end.saturating_sub(*keyframe).saturating_sub(1) / DENSE_SPACING) as usize;
            let had = queue.from_file.get(keyframe).copied().unwrap_or(0);
            if had > 0 && had + 1 >= expected && queue.claimed.insert(*keyframe) {
                skipped += 1;
            }
        }
        queue.from_file.clear();
        self.intervals_done.fetch_add(skipped, Ordering::Relaxed);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn full(&self) -> bool {
        let budget = self.budget.load(Ordering::Relaxed);
        let held = self.consumer_used.load(Ordering::Relaxed).saturating_add(self.ready_bytes.load(Ordering::Relaxed));
        held >= budget / 8 * FULL_EIGHTHS
    }

    /// Take the run of unclaimed keyframe intervals that should be walked next: the interval
    /// nearest the focus (one after it before one as far before it), with the unclaimed ones
    /// that follow it directly and then the ones directly before it, up to [`RUN_INTERVALS`] in
    /// all. Returns the indices into `keyframes`. With `near_only`, only an interval within
    /// [`NEAR_INTERVALS`] of the focus starts a run.
    fn claim_run(&self, keyframes: &[UnsignedInteger], near_only: bool) -> Claim {
        let focus = self.focus.load(Ordering::Relaxed);
        let mut queue = self.lock();
        let at = keyframes.partition_point(|k| *k <= focus).saturating_sub(1);
        let cost = |i: usize| if i >= at { (i - at) * 2 } else { (at - i) * 2 + 1 };
        let Some(mut start) = (0..keyframes.len()).filter(|i| !queue.claimed.contains(&keyframes[*i])).min_by_key(|i| cost(*i)) else {
            return Claim::NothingLeft
        };
        if near_only && cost(start) > NEAR_INTERVALS * 2 {
            return Claim::NothingNear
        }
        let mut end = start;
        while end < keyframes.len() && end - start < RUN_INTERVALS && queue.claimed.insert(keyframes[end]) {
            end += 1;
        }
        while start > 0 && end - start < RUN_INTERVALS && queue.claimed.insert(keyframes[start - 1]) {
            start -= 1;
        }
        Claim::Run(start..end)
    }

    fn push(&self, keyframe: UnsignedInteger, frame: UnsignedInteger, point: Point) {
        self.ready_bytes.fetch_add(point.cost(), Ordering::Relaxed);
        self.lock().ready.push((keyframe, frame, point, true));
    }
}

/// What [`Shared::claim_run`] found.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Claim {
    Run(core::ops::Range<usize>),
    /// Unwalked intervals remain, but none near the focus.
    NothingNear,
    /// Every interval has been taken.
    NothingLeft,
}

/// What a [`ReplaySeekPrefill`] has done so far.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct ReplaySeekPrefillStats {
    /// Keyframe intervals of the replay.
    pub intervals_total: usize,
    /// How many of them have been walked.
    pub intervals_done: usize,
    /// Frames the workers have emulated.
    pub frames_walked: u64,
    /// Workers still running (none once the replay is walked or the workers were stopped; they
    /// idle, still counted, while the index is full).
    pub workers_running: usize,
}

/// Watches a [`ReplaySeekPrefill`]; see [`ReplaySeekPrefill::monitor`]. It does not keep the
/// workers running.
#[derive(Clone)]
pub struct ReplaySeekPrefillMonitor {
    shared: Arc<Shared>,
}

impl ReplaySeekPrefillMonitor {
    /// How far the workers have got.
    pub fn stats(&self) -> ReplaySeekPrefillStats {
        ReplaySeekPrefillStats {
            intervals_total: self.shared.intervals_total.load(Ordering::Relaxed),
            intervals_done: self.shared.intervals_done.load(Ordering::Relaxed),
            frames_walked: self.shared.frames_walked.load(Ordering::Relaxed),
            workers_running: self.shared.workers_running.load(Ordering::Relaxed),
        }
    }

    /// See [`ReplaySeekPrefill::take_errors`].
    pub fn take_errors(&self) -> Vec<String> {
        core::mem::take(&mut self.shared.lock().errors)
    }

    /// Whether the workers were told to stop (the replay was detached, or the prefill dropped).
    pub fn stopped(&self) -> bool {
        self.shared.stopped()
    }

    /// Whether every keyframe interval of the replay has snapshots, made now or read from the
    /// index's file.
    pub fn all_walked(&self) -> bool {
        self.shared.all_walked()
    }

    /// Whether the index's file (if there is one) is still being read.
    pub fn loading(&self) -> bool {
        self.shared.loading.load(Ordering::Relaxed)
    }
}

/// Emulators walking a replay in the background to fill the seek index of the emulator that
/// shows it; see the module documentation. Dropping it stops them (without waiting for them).
pub struct ReplaySeekPrefill {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
    file: Option<TrackedFile>,
}

impl ReplaySeekPrefill {
    /// Start one thread per worker. Each must make an emulator set up like the one the snapshots
    /// are for (same game, same options that change what it emulates) and a player of the same
    /// replay file: a snapshot is a difference against the state of a recorded keyframe as the
    /// player rebuilds it.
    pub fn start(workers: Vec<ReplaySeekPrefillWorker>) -> Self {
        Self::start_with_file(workers, None)
    }

    /// [`Self::start`], with the index kept in `file` between sessions: what the file holds is
    /// read first (on a thread of its own) and handed over like the workers' snapshots, the
    /// workers walk only what it lacks, and the emulator the snapshots are for writes the index
    /// back to it (see [`SuperShuckieCore::set_replay_seek_prefill`]). `workers` may be empty:
    /// the file is then only read, and written with what that emulator's own walks add.
    pub fn start_with_file(workers: Vec<ReplaySeekPrefillWorker>, file: Option<ReplaySeekIndexFile>) -> Self {
        let shared = Arc::new(Shared::new(workers.len(), file.is_some()));
        let file = file.map(TrackedFile::new);
        if let Some(file) = file.as_ref() {
            Self::read_file(&shared, file, true);
        }
        let threads = workers.into_iter().enumerate().filter_map(|(i, make)| {
            let for_thread = shared.clone();
            let spawned = std::thread::Builder::new().name(alloc::format!("seek-prefill-{i}")).spawn(move || {
                crate::thread::mark_thread_background();
                if let Err(e) = work(&for_thread, make) {
                    for_thread.lock().errors.push(e);
                }
                for_thread.workers_running.fetch_sub(1, Ordering::Relaxed);
            });
            if spawned.is_err() {
                shared.workers_running.fetch_sub(1, Ordering::Relaxed);
            }
            spawned.ok()
        }).collect();
        Self { shared, threads, file }
    }

    /// Read the index's file on a thread of its own and queue what it holds. `first` is the
    /// read the workers wait for; a later one (another program wrote the file) only adds.
    fn read_file(shared: &Arc<Shared>, file: &TrackedFile, first: bool) {
        let (for_thread, file) = (shared.clone(), file.clone());
        let spawned = std::thread::Builder::new().name(String::from("seek-index-load")).spawn(move || {
            crate::thread::mark_thread_background();
            let header = file.read(|keyframe, frame, point| {
                for_thread.ready_bytes.fetch_add(point.cost(), Ordering::Relaxed);
                let mut queue = for_thread.lock();
                if first {
                    *queue.from_file.entry(keyframe).or_default() += 1;
                }
                queue.ready.push((keyframe, frame, point, false));
                !for_thread.stopped()
            });
            if header.is_some_and(|h| h.complete) {
                for_thread.file_complete.store(true, Ordering::Relaxed);
            }
            for_thread.loading.store(false, Ordering::Relaxed);
        });
        if spawned.is_err() {
            shared.loading.store(false, Ordering::Relaxed);
        }
    }

    /// The file the index is kept in between sessions, if any.
    pub(crate) fn file(&self) -> Option<&TrackedFile> {
        self.file.as_ref()
    }

    /// Read the index's file again if another program has written it since this one last read
    /// or wrote it. Whether a read was started.
    pub(crate) fn read_file_if_changed(&self) -> bool {
        let Some(file) = self.file.as_ref() else {
            return false
        };
        if self.shared.stopped() || !file.changed() || self.shared.loading.swap(true, Ordering::Relaxed) {
            return false
        }
        Self::read_file(&self.shared, file, false);
        true
    }

    /// Whether the workers are done for good: none is running and the file has been read.
    pub(crate) fn finished(&self) -> bool {
        !self.shared.loading.load(Ordering::Relaxed) && self.shared.workers_running.load(Ordering::Relaxed) == 0
    }

    /// See [`ReplaySeekPrefillMonitor::all_walked`].
    pub(crate) fn all_walked(&self) -> bool {
        self.shared.all_walked()
    }

    /// How far the workers have got.
    pub fn stats(&self) -> ReplaySeekPrefillStats {
        self.monitor().stats()
    }

    /// Why workers gave up, if any did (their emulator or player could not be made, or the
    /// replay would not attach). Each is reported once.
    pub fn take_errors(&self) -> Vec<String> {
        self.monitor().take_errors()
    }

    /// A handle that can ask how the workers are doing after this has been handed to the
    /// emulator, without asking the emulator's thread.
    pub fn monitor(&self) -> ReplaySeekPrefillMonitor {
        ReplaySeekPrefillMonitor { shared: self.shared.clone() }
    }

    /// Stop the workers and wait until they have.
    pub fn stop_and_join(mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }

    /// The user is at `frame` now.
    pub(crate) fn set_focus(&self, frame: UnsignedInteger) {
        self.shared.focus.store(frame, Ordering::Relaxed);
    }

    /// Take what the workers have made since the last time.
    pub(crate) fn take_ready(&self) -> Vec<(UnsignedInteger, UnsignedInteger, Point, bool)> {
        if self.shared.ready_bytes.load(Ordering::Relaxed) == 0 {
            return Vec::new()
        }
        let ready = core::mem::take(&mut self.shared.lock().ready);
        self.shared.ready_bytes.fetch_sub(ready.iter().map(|(_, _, p, _)| p.cost()).sum(), Ordering::Relaxed);
        ready
    }

    /// Tell the workers how full the index they fill is.
    pub(crate) fn report_index(&self, used: usize, budget: usize) {
        self.shared.consumer_used.store(used, Ordering::Relaxed);
        self.shared.budget.store(budget, Ordering::Relaxed);
    }
}

impl Drop for ReplaySeekPrefill {
    fn drop(&mut self) {
        // Not joined: a worker may be in the middle of loading a keyframe, and whoever drops
        // this (the emulator thread, detaching a replay) should not wait for that.
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

fn work(shared: &Arc<Shared>, make: ReplaySeekPrefillWorker) -> Result<(), String> {
    // What the index's file already has is not walked again, and when it has everything there
    // is no need for an emulator at all.
    while shared.loading.load(Ordering::Relaxed) && !shared.stopped() {
        std::thread::sleep(Duration::from_millis(5));
    }
    if shared.stopped() || shared.file_complete.load(Ordering::Relaxed) {
        return Ok(())
    }
    let (emulator, player) = make()?;
    if shared.stopped() {
        return Ok(())
    }
    let mut core = SuperShuckieCore::new(emulator, std_timestamp_provider());
    core.set_audio_enabled(false);
    // The state every interval starts from is then the recording's own, as it is for a seek
    // that loads the keyframe, whatever this emulator made of the interval before.
    core.set_auto_resync_keyframes_in_replays(true);
    core.set_ignore_speed_changes_in_replays(true);
    let sink = shared.clone();
    core.export_seek_snapshots(Box::new(move |keyframe, frame, point| sink.push(keyframe, frame, point)));
    core.attach_replay_player(player, true).map_err(|e| alloc::format!("the replay would not attach: {e:?}"))?;
    if !core.seek_index_applies() {
        return Ok(())
    }

    let keyframes: Vec<UnsignedInteger> = core.replay_player.as_ref().map(|p| p.all_keyframes().keys().copied().collect()).unwrap_or_default();
    shared.intervals_total.store(keyframes.len(), Ordering::Relaxed);
    let total_frames = core.replay_player.as_ref().map(|p| p.get_total_frames()).unwrap_or(0);
    shared.skip_what_the_file_had(&keyframes, total_frames);

    while !shared.stopped() {
        let run = match shared.claim_run(&keyframes, shared.full()) {
            Claim::Run(run) => run,
            Claim::NothingNear => {
                std::thread::sleep(IDLE_SLEEP);
                continue
            },
            Claim::NothingLeft => break
        };
        // Where the run ends: the keyframe after its last interval, or the end of the replay.
        let end = keyframes.get(run.end).copied();
        if core.go_to_replay_keyframe(keyframes[run.start]).is_err() {
            // Unreadable here; a seek into it will say so. The intervals stay claimed.
            shared.intervals_done.fetch_add(run.len(), Ordering::Relaxed);
            continue
        }
        let mut done = 0;
        let mut frame = core.total_frames();
        while !shared.stopped() && !core.is_replay_stalled() && end.is_none_or(|end| core.total_frames() < end) {
            core.run_unlocked_hidden();
            let now = core.total_frames();
            if now != frame {
                shared.frames_walked.fetch_add(now.saturating_sub(frame), Ordering::Relaxed);
                frame = now;
                // Intervals of the run the cursor has left behind.
                let passed = keyframes[run.clone()].partition_point(|k| *k <= now).saturating_sub(1);
                if passed > done {
                    shared.intervals_done.fetch_add(passed - done, Ordering::Relaxed);
                    done = passed;
                }
            }
        }
        if !shared.stopped() {
            shared.intervals_done.fetch_add(run.len() - done, Ordering::Relaxed);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared(focus: u64) -> Shared {
        let shared = Shared::new(0, false);
        shared.focus.store(focus, Ordering::Relaxed);
        shared.budget.store(800, Ordering::Relaxed);
        shared
    }

    #[test]
    fn intervals_the_file_had_are_not_walked_again() {
        let keyframes: Vec<u64> = (0..6).map(|i| i * 120).collect();
        let shared = shared(0);
        {
            let mut queue = shared.lock();
            // Seven snapshots fit an interval of 120 frames (15, 30, ..., 105).
            queue.from_file.insert(0, 7);
            queue.from_file.insert(120, 6);
            queue.from_file.insert(240, 3);
            // The last interval is 40 frames long: two snapshots.
            queue.from_file.insert(600, 2);
        }
        shared.skip_what_the_file_had(&keyframes, 640);
        assert_eq!(shared.intervals_done.load(Ordering::Relaxed), 3);
        assert_eq!(shared.claim_run(&keyframes, false), Claim::Run(2..5));
        assert_eq!(shared.claim_run(&keyframes, false), Claim::NothingLeft);
        // A second worker asking changes nothing.
        shared.skip_what_the_file_had(&keyframes, 640);
        assert_eq!(shared.intervals_done.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn runs_start_at_the_focus_and_never_overlap() {
        let keyframes: Vec<u64> = (0..30).map(|i| i * 120).collect();
        let shared = shared(10 * 120 + 50);
        // The interval the focus is in and the seven after it.
        assert_eq!(shared.claim_run(&keyframes, false), Claim::Run(10..18));
        // Then what lies just before the focus (nearer than what follows the first run), as a
        // run that ends where the first began.
        assert_eq!(shared.claim_run(&keyframes, false), Claim::Run(2..10));
        // The user went back: nearest now are the two intervals left before them.
        shared.focus.store(8 * 120, Ordering::Relaxed);
        assert_eq!(shared.claim_run(&keyframes, false), Claim::Run(0..2));
        assert_eq!(shared.claim_run(&keyframes, false), Claim::Run(18..26));
        assert_eq!(shared.claim_run(&keyframes, false), Claim::Run(26..30));
        assert_eq!(shared.claim_run(&keyframes, false), Claim::NothingLeft);
        assert_eq!(shared.lock().claimed.len(), 30);
    }

    #[test]
    fn a_full_index_only_gets_what_is_near_the_focus() {
        let keyframes: Vec<u64> = (0..200).map(|i| i * 120).collect();
        let shared = shared(100 * 120);
        assert_eq!(shared.claim_run(&keyframes, true), Claim::Run(100..108));
        let mut runs = 1;
        while let Claim::Run(_) = shared.claim_run(&keyframes, true) {
            runs += 1;
            assert!(runs < 50);
        }
        // About a minute either side is taken (a run may reach a little further); the rest is
        // too far away to be worth pushing other snapshots out for.
        assert_eq!(shared.claim_run(&keyframes, true), Claim::NothingNear);
        let claimed = shared.lock().claimed.clone();
        let near = |i: u64| i * 120;
        assert!(claimed.iter().all(|k| (near(70 - 8)..=near(130 + 8)).contains(k)), "{claimed:?}");
        assert!((near(71)..=near(130)).step_by(120).all(|k| claimed.contains(&k)), "{claimed:?}");
        assert!(matches!(shared.claim_run(&keyframes, false), Claim::Run(_)));
    }

    #[test]
    fn workers_idle_once_the_index_is_nearly_full() {
        let shared = shared(0);
        assert!(!shared.full());
        shared.consumer_used.store(500, Ordering::Relaxed);
        assert!(!shared.full());
        shared.ready_bytes.store(200, Ordering::Relaxed);
        assert!(shared.full(), "what is waiting to be picked up counts");
    }
}
