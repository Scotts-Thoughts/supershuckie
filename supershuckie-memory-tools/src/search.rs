//! The RAM search engine.
//!
//! A search starts with a scan over a snapshot of memory and narrows its candidates with further
//! scans, each against a newer snapshot. While there are many candidates they are kept as a bitset
//! per region alongside the snapshots they were found in; once a sparse list of addresses and values
//! takes less memory than that, the search switches to one for good. Every step can be undone.

use crate::region::RegionInfo;
use crate::value::{decode_number, Number, PatternByte, ValueFormat, ValueType};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

/// Undo steps kept.
pub const MAX_HISTORY_STEPS: usize = 16;

/// Memory the undo history may use.
pub const MAX_HISTORY_BYTES: usize = 128 * 1024 * 1024;

/// Progress and cancellation are checked this often.
const CHUNK_SLOTS: usize = 64 * 1024;

/// A copy of memory taken between two frames.
#[derive(Clone, Debug)]
pub struct MemorySnapshot {
    /// Emulated frame it was taken after.
    pub frame: u64,
    /// State epoch it was taken in (changes when memory was replaced wholesale).
    pub epoch: u64,
    /// Where each region's bytes are in `bytes`.
    pub layout: Vec<SnapshotRegion>,
    /// Every region's bytes, concatenated.
    pub bytes: Vec<u8>
}

/// A region's place in a [`MemorySnapshot`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotRegion {
    pub region: RegionInfo,
    /// Start of the region's bytes.
    pub offset: usize,
    /// Mapped bytes (the rest of the region's `len` are unmapped).
    pub available: usize
}

impl MemorySnapshot {
    /// The mapped bytes of region `index`.
    #[inline]
    pub fn region_bytes(&self, index: usize) -> &[u8] {
        let layout = &self.layout[index];
        &self.bytes[layout.offset..layout.offset + layout.available]
    }

    fn same_layout(&self, other: &MemorySnapshot) -> bool {
        self.layout.len() == other.layout.len() && self.layout.iter().zip(&other.layout).all(|(a, b)| a.region == b.region)
    }

    /// Region index and offset of an address whose `len` bytes are all mapped.
    fn locate(&self, address: u32, len: usize) -> Option<(usize, usize)> {
        self.layout.iter().enumerate().find_map(|(index, layout)| {
            let offset = layout.region.offset_of(address, len)? as usize;
            (offset + len <= layout.available).then_some((index, offset))
        })
    }
}

/// What to compare each candidate's value with.
#[derive(Clone, Debug, PartialEq)]
pub enum Comparison {
    /// Every aligned address is a candidate (initial scans only).
    Unknown,
    Equal(Number),
    NotEqual(Number),
    Less(Number),
    LessOrEqual(Number),
    Greater(Number),
    GreaterOrEqual(Number),
    /// Inclusive.
    Between(Number, Number),
    InSet(Vec<Number>),
    /// Bytes matching a pattern (bytes and text types).
    Pattern(Vec<PatternByte>),
    Changed,
    Unchanged,
    Increased,
    Decreased,
    IncreasedBy(Number),
    DecreasedBy(Number),
    /// Changed by exactly this much, up or down.
    ChangedBy(Number),
    /// Changed by at least this much, up or down.
    ChangedByAtLeast(Number),
    EqualToFirst,
    NotEqualToFirst,
    IncreasedSinceFirst,
    DecreasedSinceFirst
}

impl Comparison {
    /// Whether this compares with the previous scan's values.
    pub fn needs_previous(&self) -> bool {
        matches!(self, Comparison::Changed | Comparison::Unchanged | Comparison::Increased | Comparison::Decreased | Comparison::IncreasedBy(_) | Comparison::DecreasedBy(_) | Comparison::ChangedBy(_) | Comparison::ChangedByAtLeast(_))
    }

    /// Whether this compares with the first scan's values.
    pub fn needs_first(&self) -> bool {
        matches!(self, Comparison::EqualToFirst | Comparison::NotEqualToFirst | Comparison::IncreasedSinceFirst | Comparison::DecreasedSinceFirst)
    }

    /// Whether this can start a search.
    pub fn is_initial(&self) -> bool {
        !self.needs_previous() && !self.needs_first()
    }

    /// Whether this works with values that are not numbers (bytes, text).
    pub fn works_on_bytes(&self) -> bool {
        matches!(self, Comparison::Unknown | Comparison::Pattern(_) | Comparison::Changed | Comparison::Unchanged | Comparison::EqualToFirst | Comparison::NotEqualToFirst)
    }
}

/// How a search reads memory.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchSettings {
    pub format: ValueFormat,
    /// Candidates are at multiples of this from each region's start (1, 2 or 4).
    pub alignment: u8,
    /// Regions to search (indices into the snapshot's layout); empty means all.
    pub regions: Vec<usize>,
    /// Only addresses in `[start, end)`.
    pub range: Option<(u32, u32)>,
    /// Floats within this of each other are equal.
    pub epsilon: f64
}

/// Why a scan did not happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchError {
    Cancelled,
    /// The snapshot's regions differ from the ones the search started with.
    LayoutChanged,
    /// The comparison cannot be used here.
    BadComparison(String)
}

impl core::fmt::Display for SearchError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SearchError::Cancelled => f.write_str("cancelled"),
            SearchError::LayoutChanged => f.write_str("the game's memory layout changed; start a new search"),
            SearchError::BadComparison(e) => f.write_str(e)
        }
    }
}

/// Cancellation and progress for a running scan.
#[derive(Default)]
pub struct ScanControl {
    pub cancel: AtomicBool,
    /// 0-1000.
    pub progress: AtomicU32
}

/// A candidate, for display.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchRow {
    pub address: u32,
    pub region: usize,
    /// The value when last scanned.
    pub previous: Vec<u8>,
    /// The value at the first scan.
    pub first: Vec<u8>
}

#[derive(Clone, Debug)]
struct DenseRegion {
    /// One bit per aligned slot.
    words: Vec<u64>,
    count: u64,
    /// Candidates before each 64-word block, for paging.
    rank: Vec<u64>
}

impl DenseRegion {
    fn build_rank(&mut self) {
        self.rank.clear();
        let mut total = 0u64;
        for block in self.words.chunks(64) {
            self.rank.push(total);
            total += block.iter().map(|w| w.count_ones() as u64).sum::<u64>();
        }
        self.count = total;
    }

    /// Slot of the `n`th candidate.
    fn nth_slot(&self, n: u64) -> Option<usize> {
        if n >= self.count {
            return None
        }
        let block = self.rank.partition_point(|&before| before <= n) - 1;
        let mut remaining = n - self.rank[block];
        for (w, word) in self.words.iter().enumerate().skip(block * 64) {
            let ones = word.count_ones() as u64;
            if remaining < ones {
                let mut word = *word;
                for _ in 0..remaining {
                    word &= word - 1;
                }
                return Some(w * 64 + word.trailing_zeros() as usize)
            }
            remaining -= ones;
        }
        None
    }
}

#[derive(Clone, Debug)]
enum Candidates {
    Dense {
        regions: Vec<DenseRegion>,
        /// The snapshot the candidates' current values are in.
        previous: Arc<MemorySnapshot>,
        /// The first scan's snapshot.
        first: Arc<MemorySnapshot>
    },
    Sparse {
        addresses: Vec<u32>,
        regions: Vec<u16>,
        previous: Vec<u8>,
        first: Vec<u8>
    }
}

impl Candidates {
    fn count(&self) -> u64 {
        match self {
            Candidates::Dense { regions, .. } => regions.iter().map(|r| r.count).sum(),
            Candidates::Sparse { addresses, .. } => addresses.len() as u64
        }
    }

    /// Memory this state holds, not counting snapshots shared with the current state.
    fn history_bytes(&self) -> usize {
        match self {
            Candidates::Dense { regions, previous, .. } => regions.iter().map(|r| r.words.len() * 8 + r.rank.len() * 8).sum::<usize>() + previous.bytes.len(),
            Candidates::Sparse { addresses, regions, previous, first } => addresses.len() * 4 + regions.len() * 2 + previous.len() + first.len()
        }
    }
}

/// Reads the value at an offset of a byte slice.
#[derive(Copy, Clone)]
struct Reader {
    format: ValueFormat,
    len: usize,
    epsilon: f64
}

impl Reader {
    #[inline(always)]
    fn number(&self, bytes: &[u8], offset: usize) -> Option<Number> {
        let b = &bytes[offset..offset + self.len];
        // The common fixed-size cases without the general decoder's checks.
        Some(match (self.format.ty, self.format.big_endian) {
            (ValueType::U8, _) => Number::Int(b[0] as i64),
            (ValueType::I8, _) => Number::Int(b[0] as i8 as i64),
            (ValueType::U16, false) => Number::Int(u16::from_le_bytes([b[0], b[1]]) as i64),
            (ValueType::U16, true) => Number::Int(u16::from_be_bytes([b[0], b[1]]) as i64),
            (ValueType::I16, false) => Number::Int(i16::from_le_bytes([b[0], b[1]]) as i64),
            (ValueType::I16, true) => Number::Int(i16::from_be_bytes([b[0], b[1]]) as i64),
            (ValueType::U32, false) => Number::Int(u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as i64),
            (ValueType::U32, true) => Number::Int(u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as i64),
            (ValueType::I32, false) => Number::Int(i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as i64),
            (ValueType::I32, true) => Number::Int(i32::from_be_bytes([b[0], b[1], b[2], b[3]]) as i64),
            _ => return decode_number(&self.format, b)
        })
    }

    #[inline(always)]
    fn equal(&self, a: Number, b: Number) -> bool {
        match (a, b) {
            (Number::Int(a), Number::Int(b)) => a == b,
            (a, b) => (a.as_f64() - b.as_f64()).abs() <= self.epsilon
        }
    }

    #[inline(always)]
    fn less(&self, a: Number, b: Number) -> bool {
        match (a, b) {
            (Number::Int(a), Number::Int(b)) => a < b,
            (a, b) => a.as_f64() < b.as_f64() && !self.equal(a, b)
        }
    }

    #[inline(always)]
    fn difference(&self, new: Number, old: Number) -> Number {
        match (new, old) {
            (Number::Int(n), Number::Int(o)) => Number::Int(n.wrapping_sub(o)),
            (n, o) => Number::Float(n.as_f64() - o.as_f64())
        }
    }

    #[inline(always)]
    fn abs(&self, n: Number) -> Number {
        match n {
            Number::Int(i) => Number::Int(i.wrapping_abs()),
            Number::Float(f) => Number::Float(f.abs())
        }
    }

    /// Whether a candidate passes. `previous` and `first` are its bytes at those scans.
    #[inline(always)]
    fn passes(&self, comparison: &Comparison, current: &[u8], previous: Option<&[u8]>, first: Option<&[u8]>) -> bool {
        match comparison {
            Comparison::Unknown => true,
            Comparison::Pattern(pattern) => pattern.iter().zip(current).all(|(p, b)| b & p.mask == p.value),
            Comparison::Changed => previous.is_some_and(|p| p != current),
            Comparison::Unchanged => previous.is_some_and(|p| p == current),
            Comparison::EqualToFirst => first.is_some_and(|f| f == current),
            Comparison::NotEqualToFirst => first.is_some_and(|f| f != current),
            _ => {
                let Some(value) = self.number(current, 0) else { return false };
                let number_of = |bytes: Option<&[u8]>| bytes.and_then(|b| self.number(b, 0));
                match comparison {
                    Comparison::Equal(v) => self.equal(value, *v),
                    Comparison::NotEqual(v) => !self.equal(value, *v),
                    Comparison::Less(v) => self.less(value, *v),
                    Comparison::LessOrEqual(v) => !self.less(*v, value),
                    Comparison::Greater(v) => self.less(*v, value),
                    Comparison::GreaterOrEqual(v) => !self.less(value, *v),
                    Comparison::Between(a, b) => !self.less(value, *a) && !self.less(*b, value),
                    Comparison::InSet(set) => set.iter().any(|v| self.equal(value, *v)),
                    Comparison::Increased => number_of(previous).is_some_and(|p| self.less(p, value)),
                    Comparison::Decreased => number_of(previous).is_some_and(|p| self.less(value, p)),
                    Comparison::IncreasedBy(d) => number_of(previous).is_some_and(|p| self.equal(self.difference(value, p), *d)),
                    Comparison::DecreasedBy(d) => number_of(previous).is_some_and(|p| self.equal(self.difference(p, value), *d)),
                    Comparison::ChangedBy(d) => number_of(previous).is_some_and(|p| self.equal(self.abs(self.difference(value, p)), self.abs(*d))),
                    Comparison::ChangedByAtLeast(d) => number_of(previous).is_some_and(|p| {
                        let change = self.abs(self.difference(value, p));
                        !self.less(change, self.abs(*d)) && !(change == Number::Int(0))
                    }),
                    Comparison::IncreasedSinceFirst => number_of(first).is_some_and(|f| self.less(f, value)),
                    Comparison::DecreasedSinceFirst => number_of(first).is_some_and(|f| self.less(value, f)),
                    Comparison::Unknown | Comparison::Pattern(_) | Comparison::Changed | Comparison::Unchanged | Comparison::EqualToFirst | Comparison::NotEqualToFirst => unreachable!()
                }
            }
        }
    }
}

/// A RAM search.
pub struct Search {
    settings: SearchSettings,
    candidates: Candidates,
    history: Vec<Candidates>,
    redo: Vec<Candidates>,
    steps: u32,
    last_frame: u64,
    last_epoch: u64
}

fn check_comparison(settings: &SearchSettings, comparison: &Comparison) -> Result<(), SearchError> {
    if !settings.format.ty.is_numeric() && !comparison.works_on_bytes() {
        return Err(SearchError::BadComparison(format!("{} values can only be compared as bytes", settings.format.ty.name())))
    }
    if let Comparison::Pattern(p) = comparison {
        if p.len() != settings.format.len() {
            return Err(SearchError::BadComparison(format!("the pattern is {} bytes but the value is {}", p.len(), settings.format.len())))
        }
    }
    Ok(())
}

impl Search {
    /// Start a search by scanning `snapshot`.
    pub fn new(settings: SearchSettings, comparison: &Comparison, snapshot: MemorySnapshot, control: &ScanControl) -> Result<Search, SearchError> {
        check_comparison(&settings, comparison)?;
        if !comparison.is_initial() {
            return Err(SearchError::BadComparison("a new search cannot compare with earlier scans".to_owned()))
        }

        let reader = Reader { format: settings.format, len: settings.format.len(), epsilon: settings.epsilon };
        let alignment = settings.alignment.max(1) as usize;
        let total_slots: usize = snapshot.layout.iter().map(|l| l.available / alignment).sum::<usize>().max(1);
        let mut done_slots = 0usize;

        let mut regions = Vec::with_capacity(snapshot.layout.len());
        for (index, layout) in snapshot.layout.iter().enumerate() {
            let bytes = snapshot.region_bytes(index);
            let slots = layout.region.len as usize / alignment;
            let mut dense = DenseRegion { words: vec![0u64; slots.div_ceil(64)], count: 0, rank: Vec::new() };

            let included = settings.regions.is_empty() || settings.regions.contains(&index);
            if included && bytes.len() >= reader.len {
                // Slots whose value is fully mapped, limited to the address range.
                let mut first_slot = 0usize;
                let mut end_slot = (bytes.len() - reader.len) / alignment + 1;
                if let Some((start, end)) = settings.range {
                    let base = layout.region.base as u64;
                    first_slot = (start as u64).saturating_sub(base).div_ceil(alignment as u64) as usize;
                    // The last slot whose whole value ends by `end`.
                    let range_end_slot = match (end as u64).checked_sub(base + reader.len as u64) {
                        Some(last_offset) => (last_offset / alignment as u64 + 1) as usize,
                        None => 0
                    };
                    end_slot = end_slot.min(range_end_slot);
                }

                let mut slot = first_slot;
                while slot < end_slot {
                    if control.cancel.load(Ordering::Relaxed) {
                        return Err(SearchError::Cancelled)
                    }
                    let chunk_end = (slot + CHUNK_SLOTS).min(end_slot);
                    match comparison {
                        Comparison::Unknown => {
                            for s in slot..chunk_end {
                                dense.words[s / 64] |= 1 << (s % 64);
                            }
                        }
                        _ => {
                            for s in slot..chunk_end {
                                let offset = s * alignment;
                                if reader.passes(comparison, &bytes[offset..offset + reader.len], None, None) {
                                    dense.words[s / 64] |= 1 << (s % 64);
                                }
                            }
                        }
                    }
                    done_slots += chunk_end - slot;
                    control.progress.store((done_slots * 1000 / total_slots).min(1000) as u32, Ordering::Relaxed);
                    slot = chunk_end;
                }
            }
            dense.build_rank();
            regions.push(dense);
        }

        let frame = snapshot.frame;
        let epoch = snapshot.epoch;
        let snapshot = Arc::new(snapshot);
        let mut search = Search {
            settings,
            candidates: Candidates::Dense { regions, previous: snapshot.clone(), first: snapshot },
            history: Vec::new(),
            redo: Vec::new(),
            steps: 1,
            last_frame: frame,
            last_epoch: epoch
        };
        search.maybe_make_sparse();
        control.progress.store(1000, Ordering::Relaxed);
        Ok(search)
    }

    /// Narrow the candidates by scanning `snapshot`. Returns snapshots no longer needed (for their
    /// buffers) whether or not the scan succeeds.
    pub fn refine(&mut self, comparison: &Comparison, snapshot: MemorySnapshot, control: &ScanControl) -> Result<Vec<MemorySnapshot>, (SearchError, MemorySnapshot)> {
        if let Err(e) = check_comparison(&self.settings, comparison) {
            return Err((e, snapshot))
        }
        if matches!(comparison, Comparison::Unknown) {
            return Err((SearchError::BadComparison("\"unknown value\" only starts a search".to_owned()), snapshot))
        }

        let reader = Reader { format: self.settings.format, len: self.settings.format.len(), epsilon: self.settings.epsilon };
        let alignment = self.settings.alignment.max(1) as usize;
        let frame = snapshot.frame;
        let epoch = snapshot.epoch;

        let next = match &self.candidates {
            Candidates::Dense { regions, previous, first } => {
                if !snapshot.same_layout(previous) {
                    return Err((SearchError::LayoutChanged, snapshot))
                }
                let total: u64 = regions.iter().map(|r| r.words.len() as u64 * 64).sum::<u64>().max(1);
                let mut done = 0u64;
                let mut new_regions = Vec::with_capacity(regions.len());
                for (index, dense) in regions.iter().enumerate() {
                    let current = snapshot.region_bytes(index);
                    let prev = previous.region_bytes(index);
                    let firsts = first.region_bytes(index);
                    let mut words = vec![0u64; dense.words.len()];

                    for (chunk_index, chunk) in dense.words.chunks(CHUNK_SLOTS / 64).enumerate() {
                        if control.cancel.load(Ordering::Relaxed) {
                            return Err((SearchError::Cancelled, snapshot))
                        }
                        for (w, &word) in chunk.iter().enumerate() {
                            if word == 0 {
                                continue
                            }
                            let word_index = chunk_index * (CHUNK_SLOTS / 64) + w;
                            let start_offset = word_index * 64 * alignment;

                            // Whole words whose bytes did not move need no per-slot work for these.
                            if matches!(comparison, Comparison::Changed | Comparison::Unchanged) {
                                let end_offset = (start_offset + 64 * alignment + reader.len - 1).min(current.len()).min(prev.len());
                                if start_offset < end_offset && current[start_offset..end_offset] == prev[start_offset..end_offset] {
                                    if matches!(comparison, Comparison::Unchanged) {
                                        words[word_index] = word;
                                    }
                                    continue
                                }
                            }

                            let mut bits = word;
                            let mut kept = 0u64;
                            while bits != 0 {
                                let bit = bits.trailing_zeros() as usize;
                                bits &= bits - 1;
                                let offset = start_offset + bit * alignment;
                                let end = offset + reader.len;
                                if end > current.len() {
                                    continue
                                }
                                let previous_bytes = prev.get(offset..end);
                                let first_bytes = firsts.get(offset..end);
                                if reader.passes(comparison, &current[offset..end], previous_bytes, first_bytes) {
                                    kept |= 1 << bit;
                                }
                            }
                            words[word_index] = kept;
                        }
                        done += chunk.len() as u64 * 64;
                        control.progress.store((done * 1000 / total).min(1000) as u32, Ordering::Relaxed);
                    }
                    let mut new_dense = DenseRegion { words, count: 0, rank: Vec::new() };
                    new_dense.build_rank();
                    new_regions.push(new_dense);
                }
                Candidates::Dense { regions: new_regions, previous: Arc::new(snapshot), first: first.clone() }
            }
            Candidates::Sparse { addresses, regions, previous, first } => {
                let len = reader.len;
                let mut new = Candidates::Sparse { addresses: Vec::new(), regions: Vec::new(), previous: Vec::new(), first: Vec::new() };
                let Candidates::Sparse { addresses: na, regions: nr, previous: np, first: nf } = &mut new else { unreachable!() };
                for (i, (&address, &region)) in addresses.iter().zip(regions).enumerate() {
                    if i % CHUNK_SLOTS == 0 {
                        if control.cancel.load(Ordering::Relaxed) {
                            return Err((SearchError::Cancelled, snapshot))
                        }
                        control.progress.store((i as u64 * 1000 / addresses.len().max(1) as u64) as u32, Ordering::Relaxed);
                    }
                    let Some((index, offset)) = snapshot.locate(address, len) else { continue };
                    if index != region as usize {
                        return Err((SearchError::LayoutChanged, snapshot))
                    }
                    let current = &snapshot.region_bytes(index)[offset..offset + len];
                    let prev = &previous[i * len..(i + 1) * len];
                    let firsts = &first[i * len..(i + 1) * len];
                    if reader.passes(comparison, current, Some(prev), Some(firsts)) {
                        na.push(address);
                        nr.push(region);
                        np.extend_from_slice(current);
                        nf.extend_from_slice(firsts);
                    }
                }
                new
            }
        };

        let old = core::mem::replace(&mut self.candidates, next);
        self.history.push(old);
        let mut released: Vec<MemorySnapshot> = self.redo.drain(..).filter_map(Self::into_snapshot).collect();
        released.extend(self.trim_history());
        self.steps += 1;
        self.last_frame = frame;
        self.last_epoch = epoch;
        released.extend(self.maybe_make_sparse());
        control.progress.store(1000, Ordering::Relaxed);
        Ok(released)
    }

    /// A state's previous snapshot, if nothing else still holds it.
    fn into_snapshot(candidates: Candidates) -> Option<MemorySnapshot> {
        match candidates {
            Candidates::Dense { previous, .. } => Arc::try_unwrap(previous).ok(),
            Candidates::Sparse { .. } => None
        }
    }

    fn trim_history(&mut self) -> Vec<MemorySnapshot> {
        let mut released = Vec::new();
        loop {
            let bytes: usize = self.history.iter().map(Candidates::history_bytes).sum();
            if self.history.len() <= MAX_HISTORY_STEPS && bytes <= MAX_HISTORY_BYTES {
                break
            }
            if self.history.is_empty() {
                break
            }
            let oldest = self.history.remove(0);
            released.extend(Self::into_snapshot(oldest));
        }
        released
    }

    /// Switch to a sparse list once it is smaller than the bitsets and snapshots.
    fn maybe_make_sparse(&mut self) -> Vec<MemorySnapshot> {
        let Candidates::Dense { regions, previous, first } = &self.candidates else {
            return Vec::new()
        };
        let len = self.settings.format.len();
        let alignment = self.settings.alignment.max(1) as usize;
        let count = self.candidates.count() as usize;
        let dense_bytes = regions.iter().map(|r| r.words.len() * 8).sum::<usize>() + previous.bytes.len() + if Arc::ptr_eq(previous, first) { 0 } else { first.bytes.len() };
        let sparse_bytes = count * (4 + 2 + len * 2);
        if sparse_bytes >= dense_bytes {
            return Vec::new()
        }

        let mut addresses = Vec::with_capacity(count);
        let mut region_indices = Vec::with_capacity(count);
        let mut previous_values = Vec::with_capacity(count * len);
        let mut first_values = Vec::with_capacity(count * len);
        for (index, dense) in regions.iter().enumerate() {
            let base = previous.layout[index].region.base;
            let prev = previous.region_bytes(index);
            let firsts = first.region_bytes(index);
            for (w, &word) in dense.words.iter().enumerate() {
                let mut bits = word;
                while bits != 0 {
                    let slot = w * 64 + bits.trailing_zeros() as usize;
                    bits &= bits - 1;
                    let offset = slot * alignment;
                    addresses.push(base + offset as u32);
                    region_indices.push(index as u16);
                    previous_values.extend_from_slice(&prev[offset..offset + len]);
                    first_values.extend_from_slice(firsts.get(offset..offset + len).unwrap_or(&prev[offset..offset + len]));
                }
            }
        }

        let old = core::mem::replace(&mut self.candidates, Candidates::Sparse { addresses, regions: region_indices, previous: previous_values, first: first_values });
        let mut released = Vec::new();
        if let Candidates::Dense { previous, first, .. } = old {
            let same = Arc::ptr_eq(&previous, &first);
            if let Ok(s) = Arc::try_unwrap(previous) {
                released.push(s);
            }
            if !same && let Ok(s) = Arc::try_unwrap(first) {
                released.push(s);
            }
        }
        released
    }

    /// Go back one scan.
    pub fn undo(&mut self) -> bool {
        let Some(previous) = self.history.pop() else {
            return false
        };
        let current = core::mem::replace(&mut self.candidates, previous);
        self.redo.push(current);
        self.steps -= 1;
        true
    }

    /// Go forward one undone scan.
    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else {
            return false
        };
        let current = core::mem::replace(&mut self.candidates, next);
        self.history.push(current);
        self.steps += 1;
        true
    }

    pub fn can_undo(&self) -> bool {
        !self.history.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Scans in the current state (1 after the first scan).
    pub fn steps(&self) -> u32 {
        self.steps
    }

    pub fn settings(&self) -> &SearchSettings {
        &self.settings
    }

    /// Candidates left.
    pub fn count(&self) -> u64 {
        self.candidates.count()
    }

    /// Frame and state epoch of the last scan.
    pub fn last_scan(&self) -> (u64, u64) {
        (self.last_frame, self.last_epoch)
    }

    /// Whether candidates are kept as a sparse list.
    pub fn is_sparse(&self) -> bool {
        matches!(self.candidates, Candidates::Sparse { .. })
    }

    /// Up to `count` candidates starting with the `offset`th, in address order within each region.
    pub fn results(&self, offset: u64, count: usize) -> Vec<SearchRow> {
        let len = self.settings.format.len();
        let alignment = self.settings.alignment.max(1) as usize;
        let mut rows = Vec::with_capacity(count.min(4096));
        match &self.candidates {
            Candidates::Sparse { addresses, regions, previous, first } => {
                let start = offset.min(addresses.len() as u64) as usize;
                let end = (start + count).min(addresses.len());
                for i in start..end {
                    rows.push(SearchRow {
                        address: addresses[i],
                        region: regions[i] as usize,
                        previous: previous[i * len..(i + 1) * len].to_vec(),
                        first: first[i * len..(i + 1) * len].to_vec()
                    });
                }
            }
            Candidates::Dense { regions, previous, first } => {
                let mut skip = offset;
                for (index, dense) in regions.iter().enumerate() {
                    if rows.len() >= count {
                        break
                    }
                    if skip >= dense.count {
                        skip -= dense.count;
                        continue
                    }
                    let Some(start_slot) = dense.nth_slot(skip) else { continue };
                    skip = 0;
                    let base = previous.layout[index].region.base;
                    let prev = previous.region_bytes(index);
                    let firsts = first.region_bytes(index);
                    let mut w = start_slot / 64;
                    let mut bits = dense.words[w] & (!0u64 << (start_slot % 64));
                    loop {
                        while bits != 0 && rows.len() < count {
                            let slot = w * 64 + bits.trailing_zeros() as usize;
                            bits &= bits - 1;
                            let o = slot * alignment;
                            rows.push(SearchRow {
                                address: base + o as u32,
                                region: index,
                                previous: prev[o..o + len].to_vec(),
                                first: firsts.get(o..o + len).unwrap_or(&prev[o..o + len]).to_vec()
                            });
                        }
                        w += 1;
                        if rows.len() >= count || w >= dense.words.len() {
                            break
                        }
                        bits = dense.words[w];
                    }
                }
            }
        }
        rows
    }

    /// Give back every snapshot the search holds (for their buffers).
    pub fn into_snapshots(self) -> Vec<MemorySnapshot> {
        let mut out = Vec::new();
        let mut firsts = Vec::new();
        for state in self.history.into_iter().chain(self.redo).chain(std::iter::once(self.candidates)) {
            if let Candidates::Dense { previous, first, .. } = state {
                firsts.push(first);
                if let Ok(s) = Arc::try_unwrap(previous) {
                    out.push(s);
                }
            }
        }
        for first in firsts {
            if let Ok(s) = Arc::try_unwrap(first) {
                out.push(s);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::parse_pattern;

    fn region(name: &str, base: u32, len: u32) -> RegionInfo {
        RegionInfo { name: name.into(), short_name: name.into(), base, len, big_endian: false, writable: true }
    }

    fn snapshot(frame: u64, regions: &[(RegionInfo, Vec<u8>)]) -> MemorySnapshot {
        let mut bytes = Vec::new();
        let mut layout = Vec::new();
        for (info, data) in regions {
            let offset = bytes.len();
            bytes.extend_from_slice(data);
            bytes.resize(offset + info.len as usize, 0);
            layout.push(SnapshotRegion { region: info.clone(), offset, available: data.len() });
        }
        MemorySnapshot { frame, epoch: 0, layout, bytes }
    }

    fn settings(ty: ValueType, size: u8, alignment: u8) -> SearchSettings {
        SearchSettings { format: ValueFormat::new(ty, size, false), alignment, regions: Vec::new(), range: None, epsilon: 0.01 }
    }

    fn addresses(search: &Search) -> Vec<u32> {
        search.results(0, usize::MAX).iter().map(|r| r.address).collect()
    }

    #[test]
    fn exact_value_scan_and_refine() {
        let r = region("RAM", 0x1000, 16);
        let mut data = vec![0u8; 16];
        data[2] = 5;
        data[9] = 5;
        let control = ScanControl::default();
        let mut search = Search::new(settings(ValueType::U8, 1, 1), &Comparison::Equal(Number::Int(5)), snapshot(0, &[(r.clone(), data.clone())]), &control).unwrap();
        assert_eq!(addresses(&search), vec![0x1002, 0x1009]);

        data[2] = 6;
        search.refine(&Comparison::Increased, snapshot(1, &[(r.clone(), data.clone())]), &control).unwrap();
        assert_eq!(addresses(&search), vec![0x1002]);
        assert_eq!(search.results(0, 1)[0].previous, vec![6]);
        assert_eq!(search.results(0, 1)[0].first, vec![5]);
        assert!(search.undo());
        assert_eq!(addresses(&search), vec![0x1002, 0x1009]);
        assert!(search.redo());
        assert_eq!(addresses(&search), vec![0x1002]);
    }

    #[test]
    fn values_do_not_straddle_regions_and_respect_alignment() {
        let a = region("A", 0x1000, 4);
        let b = region("B", 0x1004, 4);
        let control = ScanControl::default();
        let search = Search::new(settings(ValueType::U16, 2, 1), &Comparison::Unknown, snapshot(0, &[(a.clone(), vec![0; 4]), (b.clone(), vec![0; 4])]), &control).unwrap();
        assert_eq!(addresses(&search), vec![0x1000, 0x1001, 0x1002, 0x1004, 0x1005, 0x1006]);
        let aligned = Search::new(settings(ValueType::U16, 2, 2), &Comparison::Unknown, snapshot(0, &[(a, vec![0; 4]), (b, vec![0; 3])]), &control).unwrap();
        assert_eq!(addresses(&aligned), vec![0x1000, 0x1002, 0x1004]);
    }

    #[test]
    fn range_and_region_filters() {
        let a = region("A", 0x1000, 16);
        let b = region("B", 0x2000, 16);
        let control = ScanControl::default();
        let mut s = settings(ValueType::U8, 1, 1);
        s.regions = vec![1];
        s.range = Some((0x2004, 0x2008));
        let search = Search::new(s, &Comparison::Unknown, snapshot(0, &[(a.clone(), vec![0; 16]), (b.clone(), vec![0; 16])]), &control).unwrap();
        assert_eq!(addresses(&search), vec![0x2004, 0x2005, 0x2006, 0x2007]);

        // A 2-byte value must end inside the range too.
        let mut wide = settings(ValueType::U16, 2, 1);
        wide.range = Some((0x1FFF, 0x2003));
        let search = Search::new(wide, &Comparison::Unknown, snapshot(0, &[(a, vec![0; 16]), (b, vec![0; 16])]), &control).unwrap();
        assert_eq!(addresses(&search), vec![0x2000, 0x2001]);
    }

    #[test]
    fn patterns_and_bytes() {
        let r = region("RAM", 0, 8);
        let control = ScanControl::default();
        let data = vec![0x12, 0x34, 0x56, 0x12, 0x3F, 0x00, 0x12, 0x30];
        let pattern = parse_pattern("12 3?").unwrap();
        let search = Search::new(settings(ValueType::Bytes, 2, 1), &Comparison::Pattern(pattern), snapshot(0, &[(r.clone(), data)]), &control).unwrap();
        assert_eq!(addresses(&search), vec![0, 3, 6]);
        assert!(Search::new(settings(ValueType::Bytes, 2, 1), &Comparison::Greater(Number::Int(1)), snapshot(0, &[(r, vec![0; 8])]), &control).is_err());
    }

    #[test]
    fn signed_floats_and_bcd() {
        let r = region("RAM", 0, 12);
        let control = ScanControl::default();
        let mut data = vec![0u8; 12];
        data[0..2].copy_from_slice(&(-300i16).to_le_bytes());
        data[4..8].copy_from_slice(&1.5f32.to_le_bytes());
        data[8..11].copy_from_slice(&[0x01, 0x23, 0x45]);
        let signed = Search::new(settings(ValueType::I16, 2, 2), &Comparison::Less(Number::Int(-100)), snapshot(0, &[(r.clone(), data.clone())]), &control).unwrap();
        assert_eq!(addresses(&signed), vec![0]);
        let float = Search::new(settings(ValueType::F32, 4, 4), &Comparison::Equal(Number::Float(1.505)), snapshot(0, &[(r.clone(), data.clone())]), &control).unwrap();
        assert_eq!(addresses(&float), vec![4]);
        let mut bcd = settings(ValueType::Bcd, 3, 1);
        bcd.format.big_endian = true;
        let bcd_search = Search::new(bcd, &Comparison::Equal(Number::Int(12345)), snapshot(0, &[(r, data)]), &control).unwrap();
        assert_eq!(addresses(&bcd_search), vec![8]);
    }

    #[test]
    fn cancellation() {
        let r = region("RAM", 0, 1 << 20);
        let control = ScanControl::default();
        control.cancel.store(true, Ordering::Relaxed);
        assert_eq!(Search::new(settings(ValueType::U8, 1, 1), &Comparison::Unknown, snapshot(0, &[(r, vec![0; 1 << 20])]), &control).err(), Some(SearchError::Cancelled));
    }

    #[test]
    fn layout_changes_are_refused() {
        let control = ScanControl::default();
        let mut search = Search::new(settings(ValueType::U8, 1, 1), &Comparison::Unknown, snapshot(0, &[(region("A", 0, 1024), vec![0; 1024])]), &control).unwrap();
        let result = search.refine(&Comparison::Unchanged, snapshot(1, &[(region("B", 0, 1024), vec![0; 1024])]), &control);
        assert!(matches!(result, Err((SearchError::LayoutChanged, _))));
    }

    /// Dense and sparse states and paging agree with a brute-force search over random data and
    /// random scan sequences.
    #[test]
    fn matches_brute_force() {
        let mut rng = 0x2545F4914F6CDD1Du64;
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let regions = [region("A", 0x0200_0000, 4096), region("B", 0x0300_0000, 1000)];
        let control = ScanControl::default();

        for trial in 0..24 {
            let types = [ValueType::U8, ValueType::I8, ValueType::U16, ValueType::I16, ValueType::U32, ValueType::Bcd];
            let ty = types[trial % types.len()];
            let size = if ty == ValueType::Bcd { 2 } else { 0 };
            let mut s = settings(ty, size, [1u8, 2, 4][trial % 3]);
            s.format = ValueFormat::new(ty, size, trial % 2 == 0);
            let len = s.format.len();
            let alignment = s.alignment as usize;

            // Small value ranges so comparisons actually match.
            let mut memory: Vec<Vec<u8>> = regions.iter().map(|r| (0..r.len).map(|_| (next() % 4) as u8).collect()).collect();
            let make = |memory: &Vec<Vec<u8>>, frame| snapshot(frame, &regions.iter().cloned().zip(memory.iter().cloned()).collect::<Vec<_>>());
            let mut search = Search::new(s.clone(), &Comparison::Unknown, make(&memory, 0), &control).unwrap();

            // Brute force: (address, region, slice offset, first value, previous value).
            let mut expected: Vec<(u32, usize, usize, Vec<u8>, Vec<u8>)> = Vec::new();
            for (ri, r) in regions.iter().enumerate() {
                let mut offset = 0;
                while offset + len <= r.len as usize {
                    let v = memory[ri][offset..offset + len].to_vec();
                    expected.push((r.base + offset as u32, ri, offset, v.clone(), v));
                    offset += alignment;
                }
            }

            let comparisons = [Comparison::Changed, Comparison::Unchanged, Comparison::Increased, Comparison::Decreased, Comparison::NotEqualToFirst, Comparison::LessOrEqual(Number::Int(2)), Comparison::ChangedBy(Number::Int(1))];
            for step in 0..6 {
                // Change a few bytes.
                for _ in 0..600 {
                    let ri = (next() % 2) as usize;
                    let i = (next() % memory[ri].len() as u64) as usize;
                    memory[ri][i] = (next() % 4) as u8;
                }
                let comparison = comparisons[(next() % comparisons.len() as u64) as usize].clone();
                search.refine(&comparison, make(&memory, step + 1), &control).unwrap();

                let reader = Reader { format: s.format, len, epsilon: s.epsilon };
                expected.retain_mut(|(_, ri, offset, first, previous)| {
                    let current = memory[*ri][*offset..*offset + len].to_vec();
                    let keep = reader.passes(&comparison, &current, Some(previous), Some(first));
                    *previous = current;
                    keep
                });

                let rows = search.results(0, usize::MAX);
                assert_eq!(rows.len(), expected.len(), "trial {trial} step {step} {comparison:?} sparse={}", search.is_sparse());
                for (row, e) in rows.iter().zip(&expected) {
                    assert_eq!((row.address, &row.previous, &row.first), (e.0, &e.4, &e.3), "trial {trial} step {step}");
                }
                // Paging from the middle.
                if expected.len() > 10 {
                    let middle = expected.len() / 2;
                    let page = search.results(middle as u64, 5);
                    assert_eq!(page.iter().map(|r| r.address).collect::<Vec<_>>(), expected[middle..middle + 5].iter().map(|e| e.0).collect::<Vec<_>>());
                }
            }
        }
    }

    #[test]
    fn large_scan_is_fast_and_goes_sparse() {
        let r = region("MAIN", 0x0200_0000, 4 << 20);
        let mut data = vec![0u8; 4 << 20];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let control = ScanControl::default();
        let started = std::time::Instant::now();
        let mut search = Search::new(settings(ValueType::U8, 1, 1), &Comparison::Unknown, snapshot(0, &[(r.clone(), data.clone())]), &control).unwrap();
        let initial = started.elapsed();
        assert_eq!(search.count(), 4 << 20);
        assert!(!search.is_sparse());

        data[100] = data[100].wrapping_add(1);
        let started = std::time::Instant::now();
        search.refine(&Comparison::Changed, snapshot(1, &[(r.clone(), data.clone())]), &control).unwrap();
        let refine = started.elapsed();
        assert_eq!(addresses(&search), vec![0x0200_0064]);
        assert!(search.is_sparse());
        eprintln!("4 MiB: unknown-value scan {initial:?}, changed scan {refine:?}");
    }
}
