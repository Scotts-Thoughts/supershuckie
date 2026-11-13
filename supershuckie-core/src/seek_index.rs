//! Snapshots of a replay being played, taken every few frames as the emulator walks through it,
//! so that a later seek to a frame near one already visited starts from the snapshot instead of
//! from the recording's own keyframe (which is two seconds of emulation away at worst).
//!
//! A recording's keyframes are a trade between file size and seek time: every one costs bytes in
//! the file for good. But a replay is deterministic, so any state between two keyframes can be
//! made again from the keyframe before it, and a snapshot of one is a cache entry, not data. The
//! index keeps them in memory only, each as a [region diff](supershuckie_replay_recorder::region_diff)
//! against the recorded keyframe it follows (so each loads on its own: the keyframe's state, one
//! diff), compressed, and throws the least recently used keyframe interval away when it is over
//! its budget. Measured on Nintendo DS recordings (2026-11-11): a snapshot is 3-6 KB, taking one
//! costs about 5 ms, and a seek that finds one emulates a handful of frames instead of sixty.
//!
//! Snapshots follow the recorder's rule for regenerated output buffers (the DS 3D engine's
//! geometry; see `supershuckie_replay_recorder::keyframe_masks`): one taken while they change
//! from snapshot to snapshot leaves them out and is marked stale, exactly as a recorded delta
//! keyframe is, and one taken while they stand still keeps them and is exact.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use supershuckie_replay_recorder::keyframe_masks::transient_ranges;
use supershuckie_replay_recorder::replay_file::ReplayConsoleType;
use supershuckie_replay_recorder::{apply_region_diff_in_place, compress_data, decompress_data, region_diff_resizing, UnsignedInteger};

/// Frames between snapshots for the first [`DENSE_FRAMES`] a walk runs after a load (the
/// neighbourhood of a seek, where the next seek most likely lands).
pub(crate) const DENSE_SPACING: UnsignedInteger = 15;

/// Frames between snapshots once a walk has run on for longer than that (plain playback or an
/// export passing through: a snapshot costs about two frames of emulation, so fewer are taken).
pub(crate) const SPARSE_SPACING: UnsignedInteger = 60;

/// How long after a load a walk takes snapshots at [`DENSE_SPACING`].
pub(crate) const DENSE_FRAMES: UnsignedInteger = 480;

/// How far back the regenerated output buffers are compared: unchanged since a snapshot at most
/// this many frames ago means the game is not redrawing them.
const STILL_WINDOW: UnsignedInteger = 4 * SPARSE_SPACING;

/// zstd level of a snapshot (speed matters more than the last few percent).
const LEVEL: i32 = 1;

/// Bookkeeping bytes charged per snapshot on top of its payload.
const POINT_OVERHEAD: usize = 96;

/// One snapshot as the index stores it. It only means something next to the state of the
/// recorded keyframe it follows (see [`SeekIndex::insert`]).
#[derive(Clone)]
pub(crate) struct Point {
    /// zstd frame of `[u32 control length][control][data]`. Shared, so that writing the index
    /// to a file (see `seek_store`) does not copy it.
    pub(crate) payload: alloc::sync::Arc<[u8]>,
    pub(crate) raw_len: usize,
    /// Length of the snapshot's state. A state the emulator makes now can be longer than a
    /// keyframe an older build recorded (melonDS states gained a trailing section), so the diff
    /// is a resizing one, as a Nintendo 3DS keyframe's is.
    pub(crate) state_len: usize,
    /// Whether its regenerated output buffers are the keyframe's copy rather than its own.
    pub(crate) stale: bool,
}

impl Point {
    /// Bytes the index charges for it.
    pub(crate) fn cost(&self) -> usize {
        self.payload.len() + POINT_OVERHEAD
    }
}

/// Where an index that only makes snapshots for another one hands them (see
/// [`SeekIndex::set_export`]): the recorded keyframe, the snapshot's frame, the snapshot.
pub(crate) type PointExport = alloc::boxed::Box<dyn FnMut(UnsignedInteger, UnsignedInteger, Point) + Send>;

#[derive(Default)]
struct Segment {
    points: BTreeMap<UnsignedInteger, Point>,
    bytes: usize,
    last_used: u64,
}

/// What a seek should start from; see [`SeekIndex::start_for`].
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) struct SeekPoint {
    /// The recorded keyframe the snapshot follows.
    pub keyframe: UnsignedInteger,
    /// The frame the snapshot was taken at (that many frames had been emulated).
    pub frame: UnsignedInteger,
    pub stale: bool,
}

/// See the module documentation.
pub(crate) struct SeekIndex {
    budget: usize,
    used: usize,
    clock: u64,
    segments: BTreeMap<UnsignedInteger, Segment>,
    /// The regenerated output buffers at the most recent snapshot of the current walk that knew
    /// them, and its frame.
    last_true: Option<(UnsignedInteger, Vec<u8>)>,
    /// Frames run since the last load.
    frames_since_load: UnsignedInteger,
    scratch: Vec<u8>,
    /// Every snapshot goes here instead of into the index (see [`Self::set_export`]).
    export: Option<PointExport>,
    /// Snapshots at [`DENSE_SPACING`] however long the walk (see [`Self::set_export`]).
    always_dense: bool,
    /// Snapshots taken and snapshots loaded so far, for measurements.
    pub taken: u64,
    pub loaded: u64,
    /// Snapshots another index made that were put into this one ([`Self::adopt`]).
    pub adopted: u64,
    /// Snapshots put in that a file of the index does not have yet (see `seek_store`): counts
    /// up, never down.
    pub changes: u64,
    /// Keyframe intervals thrown away to stay within the budget.
    pub evictions: u64,
}

impl SeekIndex {
    pub(crate) fn new(budget: usize) -> Self {
        Self { budget, used: 0, clock: 0, segments: BTreeMap::new(), last_true: None, frames_since_load: 0, scratch: Vec::new(), export: None, always_dense: false, taken: 0, loaded: 0, adopted: 0, changes: 0, evictions: 0 }
    }

    /// Make this the index of an emulator that walks a replay only to fill another emulator's
    /// index (see `seek_prefill`): every snapshot is handed to `export` instead of kept, and
    /// they are taken at [`DENSE_SPACING`] throughout, since the walk is not a seek whose
    /// neighbourhood matters most but the whole point.
    pub(crate) fn set_export(&mut self, export: PointExport) {
        self.export = Some(export);
        self.always_dense = true;
    }

    /// Put a snapshot another index made (of the same replay, with the same build) into this
    /// one. One already there is kept: both are the same frame of the same replay. `new` is
    /// whether it was just made (rather than read back from the index's file).
    pub(crate) fn adopt(&mut self, keyframe: UnsignedInteger, frame: UnsignedInteger, point: Point, new: bool) {
        if self.budget == 0 {
            return;
        }
        let clock = self.clock;
        let segment = self.segments.entry(keyframe).or_default();
        if segment.points.contains_key(&frame) {
            return;
        }
        let bytes = point.cost();
        // As recent as the last seek, not more: when the index is full, what was put in longest
        // ago and never used goes first, and anything a seek used since stays.
        segment.last_used = segment.last_used.max(clock);
        segment.points.insert(frame, point);
        segment.bytes += bytes;
        self.used += bytes;
        self.adopted += 1;
        self.changes += new as u64;
        self.evict(keyframe);
    }

    /// Every snapshot held (the payloads are shared, not copied): keyframe, frame, snapshot.
    pub(crate) fn all_points(&self) -> Vec<(UnsignedInteger, UnsignedInteger, Point)> {
        self.segments.iter().flat_map(|(keyframe, segment)| segment.points.iter().map(|(frame, point)| (*keyframe, *frame, point.clone()))).collect()
    }

    pub(crate) fn budget(&self) -> usize {
        self.budget
    }

    /// Forget every snapshot (the replay they belong to is gone).
    pub(crate) fn clear(&mut self) {
        self.segments.clear();
        self.used = 0;
        self.last_true = None;
        self.frames_since_load = 0;
        self.taken = 0;
        self.loaded = 0;
        self.adopted = 0;
        self.changes = 0;
        self.evictions = 0;
    }

    /// Bytes held.
    pub(crate) fn used(&self) -> usize {
        self.used
    }

    /// Snapshots held.
    pub(crate) fn len(&self) -> usize {
        self.segments.values().map(|s| s.points.len()).sum()
    }

    /// The emulator's state was just replaced (a keyframe or snapshot load, or anything else
    /// that breaks the walk): nothing is known about the frames before it any more.
    pub(crate) fn walk_restarted(&mut self) {
        self.last_true = None;
        self.frames_since_load = 0;
    }

    /// One more frame was emulated.
    pub(crate) fn frame_run(&mut self, frames: UnsignedInteger) {
        self.frames_since_load = self.frames_since_load.saturating_add(frames);
    }

    /// Whether a snapshot should be taken now: `frame` frames have been emulated, the last
    /// recorded keyframe passed is `keyframe` and the next one is at `next_keyframe`.
    pub(crate) fn wants(&self, keyframe: UnsignedInteger, frame: UnsignedInteger, next_keyframe: Option<UnsignedInteger>) -> bool {
        if self.budget == 0 || frame <= keyframe || next_keyframe.is_some_and(|next| frame >= next) {
            return false;
        }
        let spacing = if self.always_dense || self.frames_since_load <= DENSE_FRAMES { DENSE_SPACING } else { SPARSE_SPACING };
        (frame - keyframe) % spacing == 0 && !self.segments.get(&keyframe).is_some_and(|s| s.points.contains_key(&frame))
    }

    /// Take a snapshot: `state` is the emulator's state after `frame` frames (it is modified),
    /// `base` the state of recorded keyframe `keyframe` as the player materialises it, and
    /// `buffers_known` whether the emulator's regenerated output buffers are its own (false while
    /// it still shows what a stale load left out).
    pub(crate) fn insert(&mut self, console: ReplayConsoleType, keyframe: UnsignedInteger, frame: UnsignedInteger, base: &[u8], state: &mut [u8], buffers_known: bool) {
        // Only ever a little longer or shorter (see `Point::state_len`); anything else is not a
        // state of the same console.
        if base.is_empty() || base.len().abs_diff(state.len()) > base.len() / 8 {
            return;
        }

        let ranges = transient_ranges(console, state);
        let true_now: Option<Vec<u8>> = (buffers_known && !ranges.is_empty()).then(|| {
            let mut bytes = Vec::with_capacity(ranges.iter().map(|r| r.len()).sum());
            for range in &ranges {
                bytes.extend_from_slice(&state[range.clone()]);
            }
            bytes
        });
        // A console without such buffers has nothing to leave out; otherwise they are kept only
        // when the game has not redrawn them since a recent snapshot.
        let keep = ranges.is_empty() || match (&true_now, &self.last_true) {
            (Some(now), Some((at, before))) => *at < frame && frame - *at <= STILL_WINDOW && now == before,
            _ => false
        };
        let mut stale = false;
        if !keep {
            // Leave them out: the keyframe's copy in their place, as the recorder's masks do.
            if ranges == transient_ranges(console, base) {
                for range in &ranges {
                    state[range.clone()].copy_from_slice(&base[range.clone()]);
                }
                stale = true;
            }
            else if !buffers_known {
                // The layouts differ (it cannot be masked) and the buffers are not known to be
                // good: not worth keeping.
                self.last_true = None;
                return;
            }
        }
        self.last_true = true_now.map(|bytes| (frame, bytes));

        let diff = region_diff_resizing(base, state);
        self.scratch.clear();
        self.scratch.extend_from_slice(&(diff.control.len() as u32).to_le_bytes());
        self.scratch.extend_from_slice(&diff.control);
        self.scratch.extend_from_slice(&diff.data);
        let Ok(payload) = compress_data(&self.scratch, LEVEL) else {
            return;
        };

        let point = Point { payload: payload.into(), raw_len: self.scratch.len(), state_len: state.len(), stale };
        if let Some(export) = self.export.as_mut() {
            self.taken += 1;
            export(keyframe, frame, point);
            return;
        }
        let bytes = point.cost();
        self.clock += 1;
        let segment = self.segments.entry(keyframe).or_default();
        segment.last_used = self.clock;
        if let Some(old) = segment.points.insert(frame, point) {
            let old_bytes = old.payload.len() + POINT_OVERHEAD;
            segment.bytes -= old_bytes;
            self.used -= old_bytes;
        }
        segment.bytes += bytes;
        self.used += bytes;
        self.taken += 1;
        self.changes += 1;
        self.evict(keyframe);
    }

    /// Throw the least recently used keyframe intervals away until the index fits its budget
    /// (never the one being walked).
    fn evict(&mut self, keep: UnsignedInteger) {
        while self.used > self.budget {
            let Some((&victim, _)) = self.segments.iter().filter(|(k, _)| **k != keep).min_by_key(|(_, s)| s.last_used) else {
                break;
            };
            if let Some(segment) = self.segments.remove(&victim) {
                self.used -= segment.bytes;
                self.evictions += 1;
            }
        }
    }

    /// The snapshot a seek to `target` (the frame to be shown) should start from, among those
    /// that follow recorded keyframe `keyframe`: the latest one far enough before the target,
    /// which is `exact_lead` frames for an exact snapshot and `stale_lead` for a stale one (the
    /// game needs that long to rebuild what it left out).
    pub(crate) fn start_for(&self, keyframe: UnsignedInteger, target: UnsignedInteger, exact_lead: UnsignedInteger, stale_lead: UnsignedInteger) -> Option<SeekPoint> {
        let segment = self.segments.get(&keyframe)?;
        let latest = target.checked_sub(exact_lead.min(stale_lead))?;
        segment.points.range(..=latest).rev()
            .find(|(frame, point)| **frame + if point.stale { stale_lead } else { exact_lead } <= target)
            .map(|(frame, point)| SeekPoint { keyframe, frame: *frame, stale: point.stale })
    }

    /// Rebuild the state of `point` in `out`, which holds the state of its keyframe (as
    /// [`Self::insert`] was given it). `false` when the snapshot is gone or does not fit.
    pub(crate) fn materialise(&mut self, console: ReplayConsoleType, point: SeekPoint, out: &mut Vec<u8>) -> bool {
        self.clock += 1;
        let clock = self.clock;
        let Some(segment) = self.segments.get_mut(&point.keyframe) else {
            return false;
        };
        segment.last_used = clock;
        let Some(stored) = segment.points.get(&point.frame) else {
            return false;
        };
        let Ok(raw) = decompress_data(&stored.payload, stored.raw_len) else {
            return false;
        };
        let Some((len, rest)) = raw.split_at_checked(4) else {
            return false;
        };
        let len = u32::from_le_bytes([len[0], len[1], len[2], len[3]]) as usize;
        let Some((control, data)) = rest.split_at_checked(len) else {
            return false;
        };
        out.resize(stored.state_len, 0);
        if !apply_region_diff_in_place(out.as_mut_slice(), control, data) {
            return false;
        }
        self.loaded += 1;
        // What the walk from here knows about the output buffers: an exact snapshot's own.
        self.frames_since_load = 0;
        self.last_true = (!point.stale).then(|| {
            let mut bytes = Vec::new();
            for range in transient_ranges(console, out) {
                bytes.extend_from_slice(&out[range]);
            }
            (point.frame, bytes)
        }).filter(|(_, bytes)| !bytes.is_empty());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(seed: u8, len: usize) -> Vec<u8> {
        (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    #[test]
    fn snapshots_round_trip_and_the_latest_far_enough_back_is_chosen() {
        let base = state(1, 4096);
        let mut index = SeekIndex::new(1 << 20);
        assert!(!index.wants(100, 100, Some(220)), "not on the keyframe itself");
        assert!(index.wants(100, 115, Some(220)));
        assert!(!index.wants(100, 116, Some(220)));
        assert!(!index.wants(100, 220, Some(220)), "the next keyframe covers its own frame");

        for frame in [115u64, 130, 145] {
            let mut s = base.clone();
            s[frame as usize] ^= 0xFF;
            s[10] = frame as u8;
            index.insert(ReplayConsoleType::GameBoyColor, 100, frame, &base, &mut s, true);
        }
        assert_eq!(index.len(), 3);
        assert!(!index.wants(100, 130, Some(220)), "already there");

        // A console without regenerated buffers has only exact snapshots.
        assert_eq!(index.start_for(100, 150, 3, 6), Some(SeekPoint { keyframe: 100, frame: 145, stale: false }));
        assert_eq!(index.start_for(100, 147, 3, 6), Some(SeekPoint { keyframe: 100, frame: 130, stale: false }));
        assert_eq!(index.start_for(100, 117, 3, 6), None);
        assert_eq!(index.start_for(220, 300, 3, 6), None);

        let point = index.start_for(100, 150, 3, 6).unwrap();
        let mut out = base.clone();
        assert!(index.materialise(ReplayConsoleType::GameBoyColor, point, &mut out));
        let mut expected = base.clone();
        expected[145] ^= 0xFF;
        expected[10] = 145;
        assert_eq!(out, expected);
    }

    /// An index that exports keeps nothing, takes snapshots densely however long it walks, and
    /// what it hands over works in another index as if that one had taken it.
    #[test]
    fn exported_snapshots_work_in_the_index_that_adopts_them() {
        let base = state(3, 4096);
        let made = alloc::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut maker = SeekIndex::new(usize::MAX);
        let sink = made.clone();
        maker.set_export(alloc::boxed::Box::new(move |keyframe, frame, point| sink.lock().unwrap().push((keyframe, frame, point))));
        maker.frame_run(10 * DENSE_FRAMES);
        assert!(maker.wants(100, 115, Some(220)), "dense however long the walk");
        let mut s = base.clone();
        s[9] = 77;
        maker.insert(ReplayConsoleType::GameBoyColor, 100, 115, &base, &mut s, true);
        assert_eq!((maker.len(), maker.used(), maker.taken), (0, 0, 1));
        assert!(maker.wants(100, 115, Some(220)), "nothing is kept");

        let mut index = SeekIndex::new(1 << 20);
        for (keyframe, frame, point) in made.lock().unwrap().drain(..) {
            index.adopt(keyframe, frame, point, true);
        }
        assert_eq!((index.len(), index.adopted, index.taken, index.changes), (1, 1, 0, 1));
        let point = index.start_for(100, 120, 3, 6).unwrap();
        let mut out = base.clone();
        assert!(index.materialise(ReplayConsoleType::GameBoyColor, point, &mut out));
        assert_eq!(out, s);
    }

    #[test]
    fn the_least_recently_used_interval_goes_first() {
        let base = state(2, 4096);
        let mut index = SeekIndex::new(3 * (POINT_OVERHEAD + 40));
        for keyframe in [0u64, 120, 240, 360] {
            let mut s = base.clone();
            s[7] = keyframe as u8 + 1;
            index.insert(ReplayConsoleType::GameBoyColor, keyframe, keyframe + 15, &base, &mut s, true);
            // Interval 0 keeps being used.
            if let Some(point) = index.start_for(0, 100, 3, 6) {
                let mut out = base.clone();
                assert!(index.materialise(ReplayConsoleType::GameBoyColor, point, &mut out));
            }
        }
        assert!(index.used() <= index.budget());
        assert!(index.start_for(0, 100, 3, 6).is_some(), "the interval in use survives");
        assert!(index.start_for(360, 400, 3, 6).is_some(), "so does the newest");
        assert!(index.start_for(120, 200, 3, 6).is_none(), "the oldest unused one went");
    }
}
