use alloc::borrow::Cow;
use alloc::format;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::mem::transmute;
use core::ffi::CStr;
use num_enum::TryFromPrimitive;
use zstd_sys::{ZSTD_CCtx_setParameter, ZSTD_cParameter, ZSTD_compress2, ZSTD_createCCtx, ZSTD_createDCtx, ZSTD_DCtx, ZSTD_DCtx_setParameter, ZSTD_dParameter, ZSTD_decompress, ZSTD_decompressStream, ZSTD_freeCCtx, ZSTD_freeDCtx, ZSTD_getErrorName, ZSTD_getFrameContentSize, ZSTD_inBuffer, ZSTD_isError, ZSTD_maxCLevel, ZSTD_minCLevel, ZSTD_outBuffer};
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
pub fn compress_data(data: &[u8], compression_level: i32) -> Result<Vec<u8>, Cow<'static, str>> {
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

/// zstd's sentinel for "the frame does not record its content size" (`ZSTD_CONTENTSIZE_UNKNOWN`,
/// `-1` reinterpreted as the `u64` this binding's `ZSTD_getFrameContentSize` returns).
const ZSTD_CONTENTSIZE_UNKNOWN: u64 = u64::MAX;

/// zstd's sentinel for "the frame header is malformed" (`ZSTD_CONTENTSIZE_ERROR`, `-2`
/// reinterpreted as `u64`).
const ZSTD_CONTENTSIZE_ERROR: u64 = u64::MAX - 1;

/// Decompress a single zstd frame made by [`compress_data`] into exactly `uncompressed_size`
/// bytes.
///
/// The frame's own claimed content size is checked against `uncompressed_size` before any memory
/// is reserved, so a corrupt or hostile frame claiming a huge size fails without a huge
/// allocation.
pub fn decompress_data(data: &[u8], uncompressed_size: usize) -> Result<Vec<u8>, Cow<'static, str>> {
    // Check the frame's own claimed content size against what the caller asked for BEFORE
    // reserving `uncompressed_size` bytes: a corrupt or hostile header claiming e.g. 1 << 40 bytes
    // must not cause a huge allocation attempt just to find out decompression fails anyway.
    //
    // SAFETY: `data` is a valid slice for its length; this function only inspects the frame header.
    let claimed_size = unsafe { ZSTD_getFrameContentSize(data.as_ptr() as *const c_void, data.len()) };
    if claimed_size == ZSTD_CONTENTSIZE_UNKNOWN {
        return Err(Cow::Borrowed("zstd frame does not record its content size"));
    }
    if claimed_size == ZSTD_CONTENTSIZE_ERROR {
        return Err(Cow::Borrowed("zstd frame header is malformed"));
    }
    if claimed_size != uncompressed_size as u64 {
        return Err(Cow::Owned(format!("zstd frame claims {claimed_size} bytes but {uncompressed_size} were expected")));
    }

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

/// [`compress_data`] with `prefix` as zstd's reference prefix: matches may reach into those bytes
/// as if they preceded `data`, and [`decompress_data_with_prefix`] must be given the same prefix.
/// Always a 128 MiB window with long-distance matching: this is for 3DS keyframes, whose delta
/// against the previous keyframe is tens of MB of bytes that mostly already occur in the
/// previous keyframe's delta (double-buffered GPU data moving around), which is what makes a
/// 44 MB payload a 2–3 MB frame.
pub fn compress_data_with_prefix(data: &[u8], compression_level: i32, prefix: &[u8]) -> Result<Vec<u8>, Cow<'static, str>> {
    use core::ffi::c_void;
    // SAFETY: pure function of its argument.
    let bound = unsafe { zstd_sys::ZSTD_compressBound(data.len()) };
    let mut v: Vec<u8> = Vec::new();
    if v.try_reserve_exact(bound).is_err() {
        return Err(Cow::Borrowed("failed to allocate RAM to compress"));
    }
    // SAFETY: creating a context has no preconditions; a null result is checked.
    let cctx = unsafe { ZSTD_createCCtx() };
    if cctx.is_null() {
        return Err(Cow::Borrowed("ZSTD_createCCtx failed"));
    }
    let result = (|| -> Result<usize, Cow<'static, str>> {
        let set = |parameter: ZSTD_cParameter, value: i32| -> Result<(), Cow<'static, str>> {
            // SAFETY: cctx is a live context.
            let r = unsafe { ZSTD_CCtx_setParameter(cctx, parameter, value) };
            if unsafe { ZSTD_isError(r) } != 0 { Err(zstd_error(r)) } else { Ok(()) }
        };
        // SAFETY: pure functions.
        let level = unsafe { compression_level.clamp(ZSTD_minCLevel() as i32, ZSTD_maxCLevel() as i32) };
        set(ZSTD_cParameter::ZSTD_c_compressionLevel, level)?;
        set(ZSTD_cParameter::ZSTD_c_windowLog, MAX_WINDOW_LOG as i32)?;
        set(ZSTD_cParameter::ZSTD_c_enableLongDistanceMatching, 1)?;
        if !prefix.is_empty() {
            // SAFETY: the prefix outlives the compression (it is borrowed for this whole function).
            let r = unsafe { zstd_sys::ZSTD_CCtx_refPrefix(cctx, prefix.as_ptr() as *const c_void, prefix.len()) };
            if unsafe { ZSTD_isError(r) } != 0 { return Err(zstd_error(r)); }
        }
        // SAFETY: `v` has `bound` bytes of capacity; `data` is a valid slice.
        let n = unsafe { ZSTD_compress2(cctx, v.as_mut_ptr() as *mut c_void, v.capacity(), data.as_ptr() as *const c_void, data.len()) };
        if unsafe { ZSTD_isError(n) } != 0 { return Err(zstd_error(n)); }
        Ok(n)
    })();
    // SAFETY: created above, not used afterwards.
    unsafe { ZSTD_freeCCtx(cctx) };
    let n = result?;
    assert!(n <= bound);
    // SAFETY: zstd wrote `n` bytes.
    unsafe { v.set_len(n) };
    Ok(v)
}

/// Decompress a frame made by [`compress_data_with_prefix`] with the same `prefix` (empty when
/// it was compressed without one) into exactly `uncompressed_size` bytes.
pub fn decompress_data_with_prefix(data: &[u8], uncompressed_size: usize, prefix: &[u8]) -> Result<Vec<u8>, Cow<'static, str>> {
    use core::ffi::c_void;
    // SAFETY: `data` is a valid slice; only the frame header is inspected.
    let claimed_size = unsafe { ZSTD_getFrameContentSize(data.as_ptr() as *const c_void, data.len()) };
    if claimed_size == ZSTD_CONTENTSIZE_UNKNOWN {
        return Err(Cow::Borrowed("zstd frame does not record its content size"));
    }
    if claimed_size == ZSTD_CONTENTSIZE_ERROR {
        return Err(Cow::Borrowed("zstd frame header is malformed"));
    }
    if claimed_size != uncompressed_size as u64 {
        return Err(Cow::Owned(format!("zstd frame claims {claimed_size} bytes but {uncompressed_size} were expected")));
    }
    let mut out: Vec<u8> = Vec::new();
    if out.try_reserve_exact(uncompressed_size).is_err() {
        return Err(Cow::Borrowed("failed to allocate RAM to decompress"));
    }
    // SAFETY: no preconditions; null checked.
    let dctx = unsafe { ZSTD_createDCtx() };
    if dctx.is_null() {
        return Err(Cow::Borrowed("ZSTD_createDCtx failed"));
    }
    let result = (|| -> Result<(), Cow<'static, str>> {
        // SAFETY: dctx is a live context.
        let r = unsafe { ZSTD_DCtx_setParameter(dctx, ZSTD_dParameter::ZSTD_d_windowLogMax, MAX_WINDOW_LOG as i32) };
        if unsafe { ZSTD_isError(r) } != 0 { return Err(zstd_error(r)); }
        if !prefix.is_empty() {
            // SAFETY: the prefix outlives the decompression.
            let r = unsafe { zstd_sys::ZSTD_DCtx_refPrefix(dctx, prefix.as_ptr() as *const c_void, prefix.len()) };
            if unsafe { ZSTD_isError(r) } != 0 { return Err(zstd_error(r)); }
        }
        // SAFETY: `out` has the capacity; `data` is a valid slice.
        let n = unsafe { zstd_sys::ZSTD_decompressDCtx(dctx, out.as_mut_ptr() as *mut c_void, uncompressed_size, data.as_ptr() as *const c_void, data.len()) };
        if unsafe { ZSTD_isError(n) } != 0 { return Err(zstd_error(n)); }
        if n != uncompressed_size {
            return Err(Cow::Owned(format!("Uncompressed size is incorrect (expected {uncompressed_size} but was {n})")));
        }
        Ok(())
    })();
    // SAFETY: created above, not used afterwards.
    unsafe { ZSTD_freeDCtx(dctx) };
    result?;
    // SAFETY: zstd wrote exactly `uncompressed_size` bytes.
    unsafe { out.set_len(uncompressed_size) };
    Ok(out)
}

/// Incremental decompression of one zstd frame (a compressed blob) straight into a caller-owned
/// buffer, so a reader can stop once it has the bytes it needs.
///
/// The output `Vec` must be created with capacity for the whole frame and never reallocated: zstd
/// is told the buffer is stable (`ZSTD_d_stableOutBuffer`), which lets it decode in place without
/// its own window copy, and it verifies the pointer and size on every call.
pub(crate) struct BlobDecoder {
    dctx: *mut ZSTD_DCtx,
    input_pos: usize,
}

// The context is only ever driven from one thread at a time; moving it between threads is fine.
unsafe impl Send for BlobDecoder {}

impl BlobDecoder {
    /// `ZSTD_d_stableOutBuffer`, spelled as the public header spells it.
    const STABLE_OUT_BUFFER: ZSTD_dParameter = ZSTD_dParameter::ZSTD_d_experimentalParam2;

    /// Prepare to decode `data`, a frame that must decompress to exactly `uncompressed_size` bytes.
    pub(crate) fn new(data: &[u8], uncompressed_size: usize) -> Result<Self, Cow<'static, str>> {
        // SAFETY: `data` is a valid slice for its length; this function only inspects the frame header.
        let claimed_size = unsafe { ZSTD_getFrameContentSize(data.as_ptr() as *const c_void, data.len()) };
        if claimed_size == ZSTD_CONTENTSIZE_UNKNOWN {
            return Err(Cow::Borrowed("zstd frame does not record its content size"));
        }
        if claimed_size == ZSTD_CONTENTSIZE_ERROR {
            return Err(Cow::Borrowed("zstd frame header is malformed"));
        }
        if claimed_size != uncompressed_size as u64 {
            return Err(Cow::Owned(format!("zstd frame claims {claimed_size} bytes but {uncompressed_size} were expected")));
        }

        // SAFETY: creating a context is safe; a null result means allocation failed.
        let dctx = unsafe { ZSTD_createDCtx() };
        if dctx.is_null() {
            return Err(Cow::Borrowed("could not allocate a zstd decompression context"));
        }
        let decoder = Self { dctx, input_pos: 0 };

        // SAFETY: dctx is a valid context and the parameter is validated by zstd.
        let result = unsafe { ZSTD_DCtx_setParameter(dctx, Self::STABLE_OUT_BUFFER, 1) };
        if unsafe { ZSTD_isError(result) } != 0 {
            return Err(zstd_error(result));
        }

        Ok(decoder)
    }

    /// Feed up to `input_chunk` more compressed bytes of `data` (the same slice every call) and
    /// append whatever they decode to `out`, whose capacity must be at least `uncompressed_size`
    /// (the same every call) and which must not have been reallocated since the first call.
    ///
    /// Returns `true` once the frame is complete; `out` then holds `uncompressed_size` bytes.
    pub(crate) fn step(&mut self, data: &[u8], out: &mut Vec<u8>, uncompressed_size: usize, input_chunk: usize) -> Result<bool, Cow<'static, str>> {
        if self.input_pos >= data.len() {
            return Err(Cow::Borrowed("zstd frame ended before producing its declared content"));
        }
        if out.capacity() < uncompressed_size || out.len() > uncompressed_size {
            return Err(Cow::Borrowed("blob decode buffer is the wrong size"));
        }

        let end = self.input_pos.saturating_add(input_chunk).min(data.len());
        let mut input = ZSTD_inBuffer { src: data.as_ptr() as *const c_void, size: end, pos: self.input_pos };
        let mut output = ZSTD_outBuffer { dst: out.as_mut_ptr() as *mut c_void, size: uncompressed_size, pos: out.len() };

        // SAFETY: `input` covers a valid prefix of `data`; `output` covers the reserved capacity
        // of `out`, whose pointer and size are the same on every call as the stable-buffer
        // contract requires (the caller never reallocates it).
        let result = unsafe { ZSTD_decompressStream(self.dctx, &mut output, &mut input) };
        if unsafe { ZSTD_isError(result) } != 0 {
            return Err(zstd_error(result));
        }

        // SAFETY: zstd initialised `out` up to `output.pos`, which is within its capacity.
        unsafe { out.set_len(output.pos) };
        self.input_pos = input.pos;

        Ok(result == 0)
    }
}

impl Drop for BlobDecoder {
    fn drop(&mut self) {
        // SAFETY: dctx was created by ZSTD_createDCtx and is not used after this.
        unsafe { ZSTD_freeDCtx(self.dctx) };
    }
}

/// Hash the given data.
pub fn blake3_hash(data: &[u8]) -> ReplayHeaderBlake3Hash {
    *blake3::hash(data).as_bytes()
}

/// Hash the concatenation of `parts` without concatenating them.
pub fn blake3_hash_slices<'a>(parts: impl IntoIterator<Item = &'a [u8]>) -> ReplayHeaderBlake3Hash {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(part);
    }
    *hasher.finalize().as_bytes()
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

/// [`region_diff`] for states whose length may differ: the 3DS core's raw states vary by a few
/// bytes from one keyframe to the next (Azahar serialises variable-size kernel bookkeeping), and
/// a full 170 MB keyframe every time that happens is not an option.
///
/// The common prefix (whole words) is diffed exactly as [`region_diff`] would; whatever `cur`
/// has past it is one final run. The player resizes its state to the keyframe's `state_len`
/// (zero-extending or truncating) before applying, so a run may extend past `prev`'s length.
/// For equal lengths the output is byte-for-byte what [`region_diff`] gives, which keeps every
/// existing (fixed-size) console's files unchanged.
#[must_use]
pub fn region_diff_resizing(prev: &[u8], cur: &[u8]) -> RegionDiff {
    if prev.len() == cur.len() {
        return region_diff(prev, cur).expect("equal lengths");
    }

    let common_words = prev.len().min(cur.len()) / 4;
    let common = common_words * 4;
    let mut diff = region_diff(&prev[..common], &cur[..common]).expect("equal lengths");

    let tail = &cur[common..];
    if !tail.is_empty() {
        let (tail_words, tail_extra) = tail.as_chunks::<4>();
        let gap = common_words - control_end(&diff.control);
        let len = tail_words.len() + usize::from(!tail_extra.is_empty());
        write_leb128(&mut diff.control, gap as u64);
        write_leb128(&mut diff.control, len as u64);
        for word in tail_words {
            diff.data.extend_from_slice(word);
        }
        if !tail_extra.is_empty() {
            diff.data.extend_from_slice(&pad_word(tail_extra));
        }
        diff.runs += 1;
    }
    diff
}

/// The word position just past the last run of a valid `control` stream.
fn control_end(control: &[u8]) -> usize {
    let mut ctl = control;
    let mut pos = 0usize;
    while let Some(gap) = read_leb128(&mut ctl) {
        let Some(len) = read_leb128(&mut ctl) else { break };
        pos += gap as usize + len as usize;
    }
    pos
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

/// Whether the [`RegionDiff`] `control` stream changes any whole 4-byte word inside the byte
/// range `start..end` (words the range only partly covers are ignored, since a change to their
/// other bytes says nothing about the range). A malformed stream counts as touching it.
#[must_use]
pub fn region_diff_touches(control: &[u8], start: usize, end: usize) -> bool {
    let first_word = start.div_ceil(4);
    let end_word = end / 4;
    if first_word >= end_word {
        return false;
    }

    let mut ctl = control;
    let mut pos = 0usize;
    while !ctl.is_empty() {
        let (Some(gap), Some(len)) = (read_leb128(&mut ctl), read_leb128(&mut ctl)) else {
            return true;
        };
        let Some(run_start) = usize::try_from(gap).ok().and_then(|gap| pos.checked_add(gap)) else {
            return true;
        };
        if run_start >= end_word {
            return false;
        }
        let Some(run_end) = usize::try_from(len).ok().and_then(|len| run_start.checked_add(len)) else {
            return true;
        };
        if run_end > first_word {
            return true;
        }
        pos = run_end;
    }
    false
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

    /// A minimal (header-only, no compressed blocks) zstd frame whose header claims `size` bytes of
    /// content. Used to check that `decompress_data` rejects a mismatched claim before allocating.
    fn frame_header_claiming(size: u64) -> Vec<u8> {
        let mut frame = Vec::new();
        frame.extend_from_slice(&0xFD2FB528u32.to_le_bytes()); // zstd magic number
        frame.push(0b1100_0000); // Frame_Header_Descriptor: Frame_Content_Size_flag = 3 (8-byte field)
        frame.push(0x00); // Window_Descriptor (unused: we never reach block decoding)
        frame.extend_from_slice(&size.to_le_bytes()); // Frame_Content_Size (flag 3: no offset)
        frame
    }

    #[test]
    fn decompress_rejects_a_mismatched_claimed_size_before_allocating() {
        // A genuine, valid small frame, but asked for the wrong size.
        let data = pseudo_random_bytes(42, 200);
        let compressed = compress_data(&data, 3).unwrap();
        assert!(decompress_data(&compressed, data.len() + 1).is_err());
        assert!(decompress_data(&compressed, data.len() - 1).is_err());
        assert_eq!(decompress_data(&compressed, data.len()).unwrap(), data);

        // A frame whose header claims an enormous content size must be rejected immediately: if the
        // mismatch were only caught after reserving, this would try to allocate a TiB.
        let huge_claim = frame_header_claiming(1u64 << 40);
        assert!(decompress_data(&huge_claim, 100).is_err());

        // The "unknown size" and "malformed header" sentinels are also rejected, not misread as
        // literal sizes.
        assert!(decompress_data(&[0xFFu8; 32], 100).is_err());
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
    fn prefix_compression_round_trips_and_needs_its_prefix() {
        let prefix: Vec<u8> = (0..200_000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        // Data that is mostly the prefix's bytes shifted: small with the prefix, large without.
        let mut data = prefix[1000..].to_vec();
        data.extend_from_slice(&prefix[..1000]);
        let with = compress_data_with_prefix(&data, 3, &prefix).unwrap();
        let without = compress_data_with_prefix(&data, 3, &[]).unwrap();
        assert!(with.len() * 4 < without.len(), "{} vs {}", with.len(), without.len());
        assert_eq!(decompress_data_with_prefix(&with, data.len(), &prefix).unwrap(), data);
        assert!(decompress_data_with_prefix(&with, data.len(), &[]).is_err());
        assert_eq!(decompress_data_with_prefix(&without, data.len(), &[]).unwrap(), data);
    }

    #[test]
    fn resizing_diff_reconstructs_longer_and_shorter_states() {
        let prev: Vec<u8> = (0..1003u32).map(|i| (i * 7) as u8).collect();
        let mut longer = prev.clone();
        longer[40] ^= 0xFF;
        longer.extend_from_slice(&[9, 8, 7, 6, 5, 4, 3]);
        let d = region_diff_resizing(&prev, &longer);
        let mut state = prev.clone();
        state.resize(longer.len(), 0);
        assert!(apply_region_diff_in_place(&mut state, &d.control, &d.data));
        assert_eq!(state, longer);

        let mut shorter = prev[..990].to_vec();
        shorter[2] ^= 1;
        shorter[989] ^= 1;
        let d = region_diff_resizing(&prev, &shorter);
        let mut state = prev.clone();
        state.truncate(shorter.len());
        assert!(apply_region_diff_in_place(&mut state, &d.control, &d.data));
        assert_eq!(state, shorter);

        // Equal lengths: identical to the plain codec.
        let same = region_diff(&prev, &longer[..prev.len()]).unwrap();
        assert_eq!(region_diff_resizing(&prev, &longer[..prev.len()]), same);
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

    #[test]
    fn region_diff_touches_only_whole_words_inside_the_range() {
        let prev = pseudo_random_bytes(5, 400);
        let changed = |offsets: &[usize]| {
            let mut cur = prev.clone();
            for &o in offsets {
                cur[o] ^= 0xA5;
            }
            region_diff(&prev, &cur).expect("equal lengths").control
        };

        // Range 101..301 covers whole words 26..=74 (bytes 104..300).
        assert!(!region_diff_touches(&changed(&[]), 101, 301));
        assert!(!region_diff_touches(&changed(&[0, 99, 350]), 101, 301));
        // Bytes 101..104 and 300 share words with bytes outside the range: ignored.
        assert!(!region_diff_touches(&changed(&[102, 300]), 101, 301));
        assert!(region_diff_touches(&changed(&[104]), 101, 301));
        assert!(region_diff_touches(&changed(&[299]), 101, 301));
        assert!(region_diff_touches(&changed(&[0, 200, 399]), 101, 301));
        // A run that starts before the range and reaches into it.
        let mut cur = prev.clone();
        for b in &mut cur[40..120] {
            *b ^= 0xFF;
        }
        assert!(region_diff_touches(&region_diff(&prev, &cur).expect("equal lengths").control, 101, 301));
        // Nothing whole inside, and a malformed stream.
        assert!(!region_diff_touches(&changed(&[101, 102]), 101, 103));
        assert!(region_diff_touches(&[0x80], 101, 301));
    }
}
