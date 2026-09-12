use alloc::borrow::Cow;
use alloc::format;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::mem::transmute;
use core::ffi::CStr;
use num_enum::TryFromPrimitive;
use zstd_sys::{ZSTD_CCtx_setParameter, ZSTD_cParameter, ZSTD_compress2, ZSTD_createCCtx, ZSTD_decompress, ZSTD_freeCCtx, ZSTD_getErrorName, ZSTD_isError, ZSTD_maxCLevel, ZSTD_minCLevel};
use crate::replay_file::ReplayHeaderBlake3Hash;

/// Describes an enum that may or may not be valid.
#[derive(Copy, Clone, PartialEq, Debug)]
#[repr(transparent)]
pub struct MaybeEnum<T: Sized + TryFromPrimitive<Primitive: Copy + Clone> + Copy + Clone> {
    inner: T::Primitive
}

impl<T: Sized + TryFromPrimitive<Primitive: Copy + Clone> + Copy + Clone> MaybeEnum<T> {
    /// Instantiate from a valid value.
    pub fn new(value: T) -> MaybeEnum<T> where T: Into<T::Primitive> {
        Self { inner: value.into() }
    }

    /// Get the value if it is valid.
    pub fn get(self) -> Result<T, T::Primitive> {
        T::try_from_primitive(self.inner).map_err(|_| self.inner)
    }

    /// Get the value or return its default if it is not valid.
    pub fn get_or_default(self) -> T where T: Default {
        self.get().unwrap_or(T::default())
    }
}

impl<T: Sized + TryFromPrimitive<Primitive: Copy + Clone> + Copy + Clone + Default + Into<T::Primitive>> Default for MaybeEnum<T> {
    fn default() -> MaybeEnum<T> {
        Self::new(T::default())
    }
}

/// Reinterpret a reference to `F` as `T`.
///
/// # Panics
///
/// Panics if `size_of::<F>() != size_of::<T>()`
///
/// # Safety
///
/// To avoid UB, the following must be true:
/// * `F` and `T` must have the same alignment.
/// * Data on `F` can be safely
pub(crate) const unsafe fn reinterpret_ref<F: Copy, T: Copy>(from: &F) -> &T {
    assert!(size_of::<F>() == size_of::<T>(), "reinterpret_ref cannot be used for different sized types");
    unsafe { transmute(from) }
}

/// Inputs above this size get a window large enough to cover the whole input (capped at 128 MiB)
/// plus long-distance matching, so every keyframe delta in a blob can match against every earlier
/// one.
const LARGE_WINDOW_THRESHOLD: usize = 8 * 1024 * 1024;

/// `ZSTD_WINDOWLOG_LIMIT_DEFAULT`: the largest window a decoder accepts without opting in.
const MAX_WINDOW_LOG: u32 = 27;

fn zstd_error(code: usize) -> Cow<'static, str> {
    // SAFETY: ZSTD_getErrorName always returns a valid static C string.
    let error_name = unsafe { CStr::from_ptr(ZSTD_getErrorName(code)).to_string_lossy() };
    Cow::Owned(format!("zstd error: {code} - {error_name}"))
}

/// Compress `data` as a single zstd frame (readable by [`decompress_data`]).
pub(crate) fn compress_data(data: &[u8], compression_level: i32) -> Result<Vec<u8>, Cow<'static, str>> {
    // SAFETY: This function is safe.
    let bound = unsafe { zstd_sys::ZSTD_compressBound(data.len()) };

    // Reserve everything.
    //
    // Internally the vector should now have enough capacity.
    let mut v: Vec<u8> = Vec::new();
    v.try_reserve_exact(bound).map_err(|_| Cow::Borrowed("could not reserve memory for compression buffer"))?;

    // SAFETY: These are safe.
    let level = unsafe { compression_level.clamp(ZSTD_minCLevel() as i32, ZSTD_maxCLevel() as i32) };

    // SAFETY: Creating a context is safe; a null result means allocation failed.
    let cctx = unsafe { ZSTD_createCCtx() };
    if cctx.is_null() {
        return Err(Cow::Borrowed("could not allocate a zstd compression context"));
    }

    let set_parameter = |parameter: ZSTD_cParameter, value: i32| -> Result<(), Cow<'static, str>> {
        // SAFETY: cctx is a valid context and every parameter is validated by zstd.
        let result = unsafe { ZSTD_CCtx_setParameter(cctx, parameter, value) };
        if unsafe { ZSTD_isError(result) } != 0 {
            return Err(zstd_error(result));
        }
        Ok(())
    };

    let compressed_data_len = (|| {
        set_parameter(ZSTD_cParameter::ZSTD_c_compressionLevel, level)?;

        if data.len() > LARGE_WINDOW_THRESHOLD {
            let window_log = (usize::BITS - (data.len() - 1).leading_zeros()).min(MAX_WINDOW_LOG);
            set_parameter(ZSTD_cParameter::ZSTD_c_windowLog, window_log as i32)?;
            set_parameter(ZSTD_cParameter::ZSTD_c_enableLongDistanceMatching, 1)?;
        }

        // SAFETY: We've reserved everything and we've supplied the correct arguments
        let compressed_data_len = unsafe {
            ZSTD_compress2(
                cctx,
                v.as_mut_ptr() as *mut c_void,
                v.capacity(),
                data.as_ptr() as *const c_void,
                data.len()
            )
        };

        // SAFETY: This function is safe.
        if unsafe { ZSTD_isError(compressed_data_len) } != 0 {
            return Err(zstd_error(compressed_data_len));
        }

        Ok(compressed_data_len)
    })();

    // SAFETY: cctx was created above and is not used afterwards.
    unsafe { ZSTD_freeCCtx(cctx) };

    let compressed_data_len = compressed_data_len?;

    assert!(compressed_data_len <= bound, "compressed_data_len 0x{compressed_data_len:X} exceeds buffer len 0x{bound:X}");

    // SAFETY: compressed data was initialized
    unsafe { v.set_len(compressed_data_len) };

    Ok(v)
}

pub(crate) fn decompress_data(data: &[u8], uncompressed_size: usize) -> Result<Vec<u8>, Cow<'static, str>> {
    let mut decompressed_data: Vec<u8> = Vec::new();
    if decompressed_data.try_reserve_exact(uncompressed_size).is_err() {
        return Err(Cow::Borrowed("failed to allocate RAM to decompress compressed blob"))
    }

    // SAFETY: Everything's reserved
    let decompressed_len = unsafe {
        ZSTD_decompress(
            decompressed_data.as_mut_ptr() as *mut c_void,
            uncompressed_size,
            data.as_ptr() as *mut c_void,
            data.len()
        )
    };

    if decompressed_len != uncompressed_size {
        // SAFETY: This function is safe.
        return if unsafe { ZSTD_isError(decompressed_len) } != 0 {
            let error_name = unsafe { CStr::from_ptr(ZSTD_getErrorName(decompressed_len)).to_string_lossy() };
            Err(Cow::Owned(format!("zstd error: {decompressed_len} - {error_name}")))
        } else {
            Err(Cow::Owned(format!("Uncompressed size is incorrect (expected {uncompressed_size} but was {decompressed_len})")))
        }
    }

    // SAFETY: It's been initialized.
    unsafe { decompressed_data.set_len(uncompressed_size) };
    Ok(decompressed_data)
}

/// Hash the given data.
pub fn blake3_hash(data: &[u8]) -> ReplayHeaderBlake3Hash {
    *blake3::hash(data).as_bytes()
}

pub(crate) unsafe fn launder_reference<T>(what: &T) -> &'static T {
    unsafe { transmute::<&T, &'static T>(what) }
}

/// Run-length region diff at 4-byte granularity (the format-v4 keyframe delta codec).
///
/// Both buffers are viewed as `ceil(len / 4)` little-endian words; a trailing partial word is
/// zero-padded for comparison and encoding (same convention as [`apply_diff`]).
///
/// `control` is a sequence of unsigned-LEB128 pairs `(gap, len)`: `gap` = unchanged words since
/// the end of the previous run (from word 0 for the first run), `len` = changed words (>= 1).
/// `data` is the concatenation of each run's `len * 4` replacement bytes, in order.
///
/// The two streams are kept separate (rather than interleaved) because zstd compresses the
/// near-repetitive control stream and the raw replacement bytes far better apart.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct RegionDiff {
    /// LEB128 `(gap, len)` pairs, in words.
    pub control: Vec<u8>,
    /// Replacement bytes of every run, concatenated.
    pub data: Vec<u8>,
    /// Number of runs (informational).
    pub runs: usize,
}

impl RegionDiff {
    /// Total encoded size; the recorder writes a full keyframe instead when this is not smaller
    /// than `state.len()`.
    #[inline]
    pub fn encoded_len(&self) -> usize {
        self.control.len() + self.data.len()
    }
}

fn write_leb128(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn read_leb128(input: &mut &[u8]) -> Option<u64> {
    let mut result = 0u64;
    let mut shift = 0u32;
    loop {
        let (&byte, rest) = input.split_first()?;
        let bits = (byte & 0x7F) as u64;
        // 10 bytes max; the 10th may only carry the top bit.
        if shift > 63 || (shift == 63 && bits > 1) {
            return None;
        }
        result |= bits << shift;
        *input = rest;
        if byte & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
    }
}

/// Zero-pad a trailing partial word.
#[inline]
fn pad_word(extra: &[u8]) -> [u8; 4] {
    let mut word = [0u8; 4];
    word[..extra.len()].copy_from_slice(extra);
    word
}

/// Compute the [`RegionDiff`] that turns `prev` into `cur`.
///
/// Returns `None` only when the lengths differ.
#[must_use]
pub fn region_diff(prev: &[u8], cur: &[u8]) -> Option<RegionDiff> {
    if prev.len() != cur.len() {
        return None;
    }

    let (prev_words, prev_extra) = prev.as_chunks::<4>();
    let (cur_words, cur_extra) = cur.as_chunks::<4>();
    let full_words = prev_words.len();

    let mut control = Vec::new();
    let mut data = Vec::new();
    let mut runs = 0usize;

    // Unchanged words since the end of the previous run.
    let mut gap = 0usize;
    // Length of the run currently being emitted (0 = not inside a run).
    let mut run_len = 0usize;

    // Identical stretches are skipped a block at a time (a slice comparison compiles to memcmp),
    // which is what makes diffing a 19 MiB state take milliseconds; the output is exactly the
    // per-word run structure regardless of block size.
    const BLOCKS: [usize; 2] = [1024, 16];

    let mut i = 0usize;
    'outer: while i < full_words {
        if run_len == 0 {
            for block in BLOCKS {
                if i + block <= full_words && prev[i * 4..(i + block) * 4] == cur[i * 4..(i + block) * 4] {
                    gap += block;
                    i += block;
                    continue 'outer;
                }
            }
        }

        if prev_words[i] != cur_words[i] {
            if run_len == 0 {
                write_leb128(&mut control, gap as u64);
                gap = 0;
                runs += 1;
            }
            run_len += 1;
            data.extend_from_slice(&cur_words[i]);
        }
        else {
            if run_len > 0 {
                write_leb128(&mut control, run_len as u64);
                run_len = 0;
            }
            gap += 1;
        }
        i += 1;
    }

    if !cur_extra.is_empty() {
        let cur_last = pad_word(cur_extra);
        if pad_word(prev_extra) != cur_last {
            if run_len == 0 {
                write_leb128(&mut control, gap as u64);
                runs += 1;
            }
            run_len += 1;
            data.extend_from_slice(&cur_last);
        }
    }

    if run_len > 0 {
        write_leb128(&mut control, run_len as u64);
    }

    Some(RegionDiff { control, data, runs })
}

/// Parse `control`, checking that every run stays within `words` words and that `data` holds
/// exactly the replacement bytes the runs need.
fn validate_region_diff(words: usize, control: &[u8], data_len: usize) -> Option<()> {
    let mut ctl = control;
    let mut pos = 0usize;
    let mut data_pos = 0usize;
    while !ctl.is_empty() {
        let gap = usize::try_from(read_leb128(&mut ctl)?).ok()?;
        let len = usize::try_from(read_leb128(&mut ctl)?).ok()?;
        if len == 0 {
            return None;
        }
        pos = pos.checked_add(gap)?;
        let end = pos.checked_add(len)?;
        if end > words {
            return None;
        }
        data_pos = data_pos.checked_add(len.checked_mul(4)?)?;
        pos = end;
    }
    (data_pos == data_len).then_some(())
}

/// Apply a [`RegionDiff`] (`control` + `data`) to `state` in place.
///
/// Returns `false`, leaving `state` untouched, if the diff is malformed (run past the end,
/// leftover or missing `data`, bad LEB128).
#[must_use]
pub fn apply_region_diff_in_place(state: &mut [u8], control: &[u8], data: &[u8]) -> bool {
    let words = state.len().div_ceil(4);
    if validate_region_diff(words, control, data.len()).is_none() {
        return false;
    }

    let mut ctl = control;
    let mut pos = 0usize;
    let mut data_pos = 0usize;
    while !ctl.is_empty() {
        // Validated above.
        let gap = read_leb128(&mut ctl).expect("validated") as usize;
        let len = read_leb128(&mut ctl).expect("validated") as usize;
        pos += gap;

        let bytes = &data[data_pos..data_pos + len * 4];
        let start = pos * 4;
        let end = start + len * 4;
        if end <= state.len() {
            state[start..end].copy_from_slice(bytes);
        }
        else {
            // The run ends in the zero-padded trailing partial word.
            let available = state.len() - start;
            state[start..].copy_from_slice(&bytes[..available]);
        }

        data_pos += len * 4;
        pos += len;
    }

    true
}

/// Apply a [`RegionDiff`] (`control` + `data`) to `prev`, returning the new state.
///
/// Returns `None` if the diff is malformed (run past the end, leftover `data`, bad LEB128).
#[must_use]
pub fn apply_region_diff(prev: &[u8], control: &[u8], data: &[u8]) -> Option<Vec<u8>> {
    let mut out = prev.to_vec();
    apply_region_diff_in_place(&mut out, control, data).then_some(out)
}

/// Diff two buffers and return the result (the format-v2/v3 codec; the writer now uses
/// [`region_diff`]).
///
/// Return `None` if a diff cannot be made.
#[must_use]
pub fn fast_diff(a_buf: &[u8], b_buf: &[u8]) -> Option<Vec<u64>> {
    // can't use this algorithm if the buffer is too large or bufs aren't the same size
    if a_buf.len() != b_buf.len() || a_buf.len() > u32::MAX as usize {
        return None
    }

    let (a_chunks, a_extra) = a_buf.as_chunks::<4>();
    let (b_chunks, b_extra) = b_buf.as_chunks::<4>();

    let mut diff = Vec::new();

    let mut offset = 0usize;

    for (a,b) in a_chunks.iter().copied().zip(b_chunks.iter().copied()) {
        let this_offset = offset;
        offset += a.len();

        let a_int = u32::from_le_bytes(a);
        let b_int = u32::from_le_bytes(b);

        if a_int != b_int {
            diff.push(((this_offset as u64) << 32) | (b_int as u64));

            // if the diff is more than 25% the size of the original state, it's probably a bad idea
            if diff.len() > a_buf.len() / 4 / 2 / 4 {
                return None;
            }
        }
    }

    // last bit might not be 4 bytes
    if !b_extra.is_empty() && a_extra != b_extra {
        let b_int = u32::from_le_bytes([
            b_extra[0],
            b_extra.get(1).copied().unwrap_or_default(),
            b_extra.get(2).copied().unwrap_or_default(),
            0
        ]);

        diff.push(((offset as u64) << 32) | (b_int as u64));
    }

    Some(diff)
}

/// Apply a diff.
///
/// Return `None` if the diff is wrong.
#[must_use]
pub fn apply_diff(buff: &[u8], diff: &[u64]) -> Option<Vec<u8>> {
    let mut output = buff.to_vec();
    apply_diff_in_place(&mut output, diff).then_some(output)
}

/// Apply a diff to `state` in place.
///
/// Returns `false`, leaving `state` untouched, if the diff is wrong (a word past the end).
#[must_use]
pub fn apply_diff_in_place(state: &mut Vec<u8>, diff: &[u64]) -> bool {
    let len = state.len();
    let padded_len = len.div_ceil(4) * 4;

    if diff.iter().any(|&d| ((d >> 32) as usize) + 4 > padded_len) {
        return false;
    }

    // The last partial word is zero-padded for the duration of the update (see `fast_diff`).
    state.resize(padded_len, 0);
    for &d in diff {
        let offset = (d >> 32) as usize;
        state[offset..offset + 4].copy_from_slice(&(d as u32).to_le_bytes());
    }
    state.truncate(len);

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::pseudo_random_bytes;
    use alloc::vec;

    /// Straightforward per-word reference encoder (no block skipping) the optimised
    /// [`region_diff`] must match byte for byte.
    fn naive_region_diff(prev: &[u8], cur: &[u8]) -> Option<RegionDiff> {
        if prev.len() != cur.len() {
            return None;
        }
        let words = prev.len().div_ceil(4);
        let word = |buf: &[u8], i: usize| -> [u8; 4] {
            let mut w = [0u8; 4];
            let end = ((i + 1) * 4).min(buf.len());
            w[..end - i * 4].copy_from_slice(&buf[i * 4..end]);
            w
        };
        let mut out = RegionDiff::default();
        let mut gap = 0usize;
        let mut i = 0usize;
        while i < words {
            if word(prev, i) == word(cur, i) {
                gap += 1;
                i += 1;
                continue;
            }
            let start = i;
            while i < words && word(prev, i) != word(cur, i) {
                out.data.extend_from_slice(&word(cur, i));
                i += 1;
            }
            write_leb128(&mut out.control, gap as u64);
            write_leb128(&mut out.control, (i - start) as u64);
            out.runs += 1;
            gap = 0;
        }
        Some(out)
    }

    fn round_trip(prev: &[u8], cur: &[u8]) -> RegionDiff {
        let d = region_diff(prev, cur).expect("same length");
        assert_eq!(d, naive_region_diff(prev, cur).unwrap(), "optimised diff differs from reference");
        assert_eq!(apply_region_diff(prev, &d.control, &d.data).expect("apply"), cur);
        let mut in_place = prev.to_vec();
        assert!(apply_region_diff_in_place(&mut in_place, &d.control, &d.data));
        assert_eq!(in_place, cur);
        d
    }

    #[test]
    fn compress_round_trips_small_and_large_inputs() {
        // Compressible but not trivial: repeated pseudo-random blocks with edits.
        let block = pseudo_random_bytes(3, 64 * 1024);
        let mut small = Vec::new();
        for i in 0..4u8 {
            small.extend_from_slice(&block);
            small[i as usize * 1000] ^= i;
        }
        // Past LARGE_WINDOW_THRESHOLD, so the window/LDM parameters are exercised.
        let mut large = Vec::new();
        while large.len() <= LARGE_WINDOW_THRESHOLD {
            large.extend_from_slice(&block);
            let n = large.len();
            large[n - 1] ^= (n % 7) as u8;
        }

        for (name, data) in [("empty", Vec::new()), ("small", small), ("large", large)] {
            for level in [1, DEFAULT_LEVEL_FOR_TEST, 19] {
                let compressed = compress_data(&data, level).unwrap_or_else(|e| panic!("{name} @ {level}: {e}"));
                let back = decompress_data(&compressed, data.len()).unwrap_or_else(|e| panic!("{name} @ {level}: {e}"));
                assert_eq!(back, data, "{name} @ {level}");
                if data.len() > 1024 {
                    assert!(compressed.len() < data.len() / 2, "{name} @ {level} did not compress: {} vs {}", compressed.len(), data.len());
                }
            }
        }

        // The wrong size is an error, not a panic.
        let compressed = compress_data(&[1, 2, 3], 3).unwrap();
        assert!(decompress_data(&compressed, 4).is_err());
        assert!(decompress_data(&[0xFF; 8], 3).is_err());
    }

    const DEFAULT_LEVEL_FOR_TEST: i32 = crate::replay_file::record::DEFAULT_ZSTD_COMPRESSION_LEVEL_V4;

    #[test]
    fn v3_diff_round_trips_and_rejects_out_of_range_words() {
        // fast_diff bails out above 1/8 changed words, so the lengths must be large enough for
        // every-64th-byte edits to stay under that.
        for len in [4096usize, 4097, 4098, 4099, 8192 + 3] {
            let prev = pseudo_random_bytes(11, len);
            let mut cur = prev.clone();
            for (i, b) in cur.iter_mut().enumerate() {
                if i % 64 == 0 {
                    *b ^= 0x3C;
                }
            }
            let diff = fast_diff(&prev, &cur).expect("sparse enough");
            assert_eq!(apply_diff(&prev, &diff).unwrap(), cur, "len {len}");
            let mut in_place = prev.clone();
            assert!(apply_diff_in_place(&mut in_place, &diff));
            assert_eq!(in_place, cur, "len {len}");
        }

        // A word starting past the padded end is rejected and leaves the state untouched.
        let state = pseudo_random_bytes(12, 10);
        let mut copy = state.clone();
        assert!(!apply_diff_in_place(&mut copy, &[(8u64 << 32) | 1, (12u64 << 32) | 2]));
        assert_eq!(copy, state);
        assert!(apply_diff(&state, &[(12u64 << 32) | 2]).is_none());
        // ...while the last (partial) word is fine.
        assert!(apply_diff(&state, &[(8u64 << 32) | 0x0201]).is_some());
    }

    #[test]
    fn leb128_round_trips() {
        for v in [0u64, 1, 127, 128, 255, 300, 16383, 16384, u32::MAX as u64, u64::MAX >> 1, u64::MAX] {
            let mut buf = Vec::new();
            write_leb128(&mut buf, v);
            let mut s = buf.as_slice();
            assert_eq!(read_leb128(&mut s), Some(v));
            assert!(s.is_empty());
        }
        // Truncated.
        assert_eq!(read_leb128(&mut &[0x80u8][..]), None);
        // Too long / overflowing.
        assert_eq!(read_leb128(&mut &[0xFFu8; 11][..]), None);
        assert_eq!(read_leb128(&mut &[0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x02][..]), None);
    }

    #[test]
    fn identity_is_empty() {
        for len in [0usize, 1, 3, 4, 5, 64, 4096, 4099] {
            let a = pseudo_random_bytes(len as u64, len);
            let d = round_trip(&a, &a);
            assert!(d.control.is_empty() && d.data.is_empty() && d.runs == 0, "len {len}");
            assert_eq!(d.encoded_len(), 0);
        }
    }

    #[test]
    fn single_word_changes() {
        let len = 4096;
        let base = pseudo_random_bytes(9, len);
        for word in [0usize, 1, 17, 511, 512, 1022, 1023] {
            let mut cur = base.clone();
            cur[word * 4] ^= 0xFF;
            let d = round_trip(&base, &cur);
            assert_eq!(d.runs, 1, "word {word}");
            assert_eq!(d.data, cur[word * 4..word * 4 + 4].to_vec());
            let mut expected_control = Vec::new();
            write_leb128(&mut expected_control, word as u64);
            write_leb128(&mut expected_control, 1);
            assert_eq!(d.control, expected_control);
        }
    }

    #[test]
    fn adjacent_changes_merge_into_one_run() {
        let base = pseudo_random_bytes(3, 1024);
        let mut cur = base.clone();
        for b in &mut cur[100..140] {
            *b = b.wrapping_add(1);
        }
        let d = round_trip(&base, &cur);
        assert_eq!(d.runs, 1);
        assert_eq!(d.data.len(), 40);
        let mut expected_control = Vec::new();
        write_leb128(&mut expected_control, 25);
        write_leb128(&mut expected_control, 10);
        assert_eq!(d.control, expected_control);

        // Two runs separated by one unchanged word.
        let mut cur2 = cur.clone();
        cur2[144] ^= 1;
        let d = round_trip(&base, &cur2);
        assert_eq!(d.runs, 2);
    }

    #[test]
    fn trailing_partial_word() {
        for extra in 1..4usize {
            let len = 4096 + extra;
            let base = pseudo_random_bytes(5, len);

            // Only the partial word changes.
            let mut cur = base.clone();
            cur[len - 1] ^= 0x0F;
            let d = round_trip(&base, &cur);
            assert_eq!(d.runs, 1, "extra {extra}");
            assert_eq!(d.data.len(), 4);
            assert_eq!(&d.data[..extra], &cur[4096..]);
            assert!(d.data[extra..].iter().all(|&b| b == 0), "padding must be zero");

            // Last full word and the partial word change together: one run.
            let mut cur = base.clone();
            cur[4092] ^= 1;
            cur[len - 1] ^= 1;
            let d = round_trip(&base, &cur);
            assert_eq!(d.runs, 1, "extra {extra}");
            assert_eq!(d.data.len(), 8);

            // The partial word is unchanged while earlier words change.
            let mut cur = base.clone();
            cur[0] ^= 1;
            let d = round_trip(&base, &cur);
            assert_eq!(d.runs, 1);
            assert_eq!(d.data.len(), 4);
        }
    }

    #[test]
    fn length_mismatch_is_none() {
        assert!(region_diff(&[0; 8], &[0; 12]).is_none());
        assert!(region_diff(&[], &[0]).is_none());
    }

    #[test]
    fn malformed_diffs_are_rejected() {
        let state = pseudo_random_bytes(1, 64); // 16 words
        let ok = |control: &[u8], data: &[u8]| apply_region_diff(&state, control, data).is_some();

        // Well-formed: gap 15, len 1.
        assert!(ok(&[15, 1], &[1, 2, 3, 4]));
        // Run past the end.
        assert!(!ok(&[15, 2], &[1, 2, 3, 4, 5, 6, 7, 8]));
        assert!(!ok(&[16, 1], &[1, 2, 3, 4]));
        // Leftover data.
        assert!(!ok(&[15, 1], &[1, 2, 3, 4, 5]));
        assert!(!ok(&[], &[1]));
        // Missing data.
        assert!(!ok(&[0, 2], &[1, 2, 3, 4]));
        // Zero-length run.
        assert!(!ok(&[0, 0], &[]));
        // Truncated control (dangling gap, unterminated LEB128).
        assert!(!ok(&[3], &[]));
        assert!(!ok(&[0x80], &[]));
        assert!(!ok(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01, 1], &[1, 2, 3, 4]));

        // A malformed diff must leave the in-place buffer untouched.
        let mut copy = state.clone();
        assert!(!apply_region_diff_in_place(&mut copy, &[0, 1, 15, 5], &[0; 24]));
        assert_eq!(copy, state);

        // Zero-length state accepts only an empty diff.
        assert_eq!(apply_region_diff(&[], &[], &[]), Some(vec![]));
        assert!(apply_region_diff(&[], &[0, 1], &[0; 4]).is_none());
    }

    #[test]
    fn random_sparse_edits_round_trip() {
        let mut x = 0x0BAD_5EED_u64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };

        for iteration in 0..400u64 {
            let len = match iteration % 8 {
                0 => (next() % 8) as usize,
                1 => 4096 + (next() % 4) as usize,
                _ => (next() % 20_000) as usize,
            };
            let prev = pseudo_random_bytes(next(), len);
            let mut cur = prev.clone();

            let edits = (next() % 40) as usize;
            for _ in 0..edits {
                if len == 0 {
                    break;
                }
                let start = (next() as usize) % len;
                let run = 1 + (next() as usize) % 300;
                let end = (start + run).min(len);
                let fill = next() as u8;
                for (i, b) in cur[start..end].iter_mut().enumerate() {
                    *b = fill.wrapping_add(i as u8);
                }
            }
            // Occasionally rewrite everything.
            if iteration % 37 == 0 {
                cur = pseudo_random_bytes(next(), len);
            }

            let d = round_trip(&prev, &cur);
            if prev == cur {
                assert_eq!(d.runs, 0);
            }
            if iteration % 37 == 0 && len > 8 {
                // A full rewrite is never smaller than the state, so the recorder falls back.
                assert!(d.encoded_len() >= len, "iteration {iteration}: {} < {len}", d.encoded_len());
            }
        }
    }

    #[test]
    fn large_state_uses_block_skip_and_matches_reference() {
        // Bigger than the largest skip block, with edits placed at block boundaries.
        let len = 1024 * 4 * 5 + 3;
        let prev = pseudo_random_bytes(77, len);
        let mut cur = prev.clone();
        for &offset in &[0usize, 4095, 4096, 4097, 8191, 8192 + 60, 8192 + 64, len - 1] {
            cur[offset] ^= 0x55;
        }
        let d = round_trip(&prev, &cur);
        // Words 0 | 1023-1024 | 2047 | 2063-2064 | 5120 (the partial word).
        assert_eq!(d.runs, 5);
    }
}
