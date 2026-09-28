//! Payloads of Nintendo 3DS keyframes ([`Packet::StoredKeyframe`](crate::Packet::StoredKeyframe))
//! and the ROM references of format v9.
//!
//! A level-0 keyframe's payload is the whole state in both versions. A delta keyframe's payload
//! is a [region diff](crate::region_diff_resizing) against the state of its reference keyframe:
//!
//! * v8: `[u64 control_len][control][data]`, one zstd frame with the reference keyframe's own
//!   payload as prefix.
//! * v9: `[u64 control_len][u64 refs_len][control][refs][literal]`, one zstd frame with the
//!   reference keyframe's whole *state* as prefix (a window over all of it; see
//!   [`compress_data_with_state_prefix`](crate::compress_data_with_state_prefix)). The diff's
//!   `data` (every run's replacement bytes, in order) is `literal` with copies of the game's ROM
//!   spliced in: `refs` is a sequence of LEB128 triples `(literal bytes before this copy, ROM
//!   offset as a zigzag delta from the end of the previous copy, length)`, and `literal` holds
//!   whatever is left after the last copy. Half of what changes between two keyframes is data
//!   the game just read from its cartridge (`replay-3ds-format-research.md` §2.1), which the
//!   player always has.
//!
//! The recorder finds ROM copies with a [`RomIndex`] of the ROM ranges the emulator read since
//! the reference keyframe (Azahar reports them); a file with no ROM references is just as valid.

use alloc::borrow::Cow;
use alloc::sync::Arc;
use alloc::vec::Vec;

/// The bytes of a game's ROM file, shared between the emulator's owner, a recorder and a player.
pub type RomBytes = Arc<dyn AsRef<[u8]> + Send + Sync>;

/// One copy of ROM bytes in a v9 delta's data stream.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) struct RomCopy {
    /// Byte offset in the diff's data stream.
    pub data_offset: usize,
    /// Byte offset in the ROM file.
    pub rom_offset: u64,
    pub len: usize,
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
        let bits = u64::from(byte & 0x7F);
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

#[inline]
fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

#[inline]
fn unzigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

/// The v8 payload of a region diff (this build writes v9; tests build v8 files with it).
#[cfg(test)]
pub(crate) fn encode_payload_v8(control: &[u8], data: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(8 + control.len() + data.len());
    payload.extend_from_slice(&(control.len() as u64).to_le_bytes());
    payload.extend_from_slice(control);
    payload.extend_from_slice(data);
    payload
}

/// A v8 payload's `(control, data)`.
pub(crate) fn split_payload_v8(payload: &[u8]) -> Result<(&[u8], &[u8]), &'static str> {
    let (control_len, rest) = payload.split_at_checked(8).ok_or("3DS delta payload is too short")?;
    let control_len = usize::try_from(u64::from_le_bytes(control_len.try_into().expect("8 bytes"))).map_err(|_| "3DS delta control length does not fit")?;
    rest.split_at_checked(control_len).ok_or("3DS delta control stream is truncated")
}

/// The v9 payload of a region diff whose `data` contains `copies` (sorted, non-overlapping, each
/// inside `data` and equal to the ROM bytes it names).
pub(crate) fn encode_payload_v9(control: &[u8], data: &[u8], copies: &[RomCopy]) -> Vec<u8> {
    let mut refs = Vec::with_capacity(copies.len() * 8);
    let mut literal = Vec::with_capacity(data.len());
    let mut data_pos = 0usize;
    let mut rom_pos = 0u64;
    for copy in copies {
        debug_assert!(copy.data_offset >= data_pos && copy.data_offset + copy.len <= data.len());
        literal.extend_from_slice(&data[data_pos..copy.data_offset]);
        write_leb128(&mut refs, (copy.data_offset - data_pos) as u64);
        write_leb128(&mut refs, zigzag(copy.rom_offset.wrapping_sub(rom_pos) as i64));
        write_leb128(&mut refs, copy.len as u64);
        data_pos = copy.data_offset + copy.len;
        rom_pos = copy.rom_offset + copy.len as u64;
    }
    literal.extend_from_slice(&data[data_pos..]);

    let mut payload = Vec::with_capacity(16 + control.len() + refs.len() + literal.len());
    payload.extend_from_slice(&(control.len() as u64).to_le_bytes());
    payload.extend_from_slice(&(refs.len() as u64).to_le_bytes());
    payload.extend_from_slice(control);
    payload.extend_from_slice(&refs);
    payload.extend_from_slice(&literal);
    payload
}

/// The parts of a v9 payload.
pub(crate) struct PayloadV9<'a> {
    pub control: &'a [u8],
    pub refs: &'a [u8],
    pub literal: &'a [u8],
}

pub(crate) fn split_payload_v9(payload: &[u8]) -> Result<PayloadV9<'_>, &'static str> {
    let (lens, rest) = payload.split_at_checked(16).ok_or("3DS delta payload is too short")?;
    let control_len = usize::try_from(u64::from_le_bytes(lens[..8].try_into().expect("8 bytes"))).map_err(|_| "3DS delta control length does not fit")?;
    let refs_len = usize::try_from(u64::from_le_bytes(lens[8..].try_into().expect("8 bytes"))).map_err(|_| "3DS delta ROM reference length does not fit")?;
    let (control, rest) = rest.split_at_checked(control_len).ok_or("3DS delta control stream is truncated")?;
    let (refs, literal) = rest.split_at_checked(refs_len).ok_or("3DS delta ROM references are truncated")?;
    Ok(PayloadV9 { control, refs, literal })
}

impl PayloadV9<'_> {
    /// Rebuild the diff's data stream into `out` (its allocation is reused): `literal` with the
    /// ROM copies spliced in. `rom` may be `None` only when there are no copies.
    pub(crate) fn data_into(&self, rom: Option<&[u8]>, out: &mut Vec<u8>) -> Result<(), Cow<'static, str>> {
        out.clear();
        if self.refs.is_empty() {
            out.extend_from_slice(self.literal);
            return Ok(());
        }
        let Some(rom) = rom else {
            return Err(Cow::Borrowed("this 3DS keyframe copies data from the game's ROM, which the player was not given"));
        };
        let mut refs = self.refs;
        let mut literal = self.literal;
        let mut rom_pos = 0u64;
        while !refs.is_empty() {
            let (Some(gap), Some(delta), Some(len)) = (read_leb128(&mut refs), read_leb128(&mut refs), read_leb128(&mut refs)) else {
                return Err(Cow::Borrowed("3DS delta ROM references are malformed"));
            };
            let gap = usize::try_from(gap).map_err(|_| Cow::Borrowed("3DS delta ROM reference does not fit"))?;
            let len = usize::try_from(len).map_err(|_| Cow::Borrowed("3DS delta ROM reference does not fit"))?;
            let (before, rest) = literal.split_at_checked(gap).ok_or(Cow::Borrowed("3DS delta ROM reference points past its literal data"))?;
            out.extend_from_slice(before);
            literal = rest;
            let start = rom_pos.wrapping_add(unzigzag(delta) as u64);
            let bytes = usize::try_from(start).ok()
                .and_then(|start| start.checked_add(len).map(|end| (start, end)))
                .and_then(|(start, end)| rom.get(start..end))
                .ok_or(Cow::Borrowed("3DS delta copies bytes from outside the ROM (is it the right game file?)"))?;
            out.extend_from_slice(bytes);
            rom_pos = start + len as u64;
        }
        out.extend_from_slice(literal);
        Ok(())
    }
}

/// Bytes per ROM block the index holds.
const BLOCK: usize = 64;
const BLOCK_WORDS: usize = BLOCK / 4;
/// Base of the polynomial hash over a block's 32-bit words.
const HASH_BASE: u64 = 0x0000_0100_0000_01B3;
/// `HASH_BASE ^ (BLOCK_WORDS - 1)`: the weight of the word leaving a rolling window.
const HASH_TOP: u64 = pow(HASH_BASE, BLOCK_WORDS as u32 - 1);
/// Most blocks one index holds (128 MB of table). A larger set of ranges (a whole 2 GB ROM) is
/// indexed at a coarser stride, which only misses shorter copies.
const MAX_INDEX_ENTRIES: u64 = 8 << 20;

const fn pow(base: u64, exp: u32) -> u64 {
    let mut result = 1u64;
    let mut i = 0;
    while i < exp {
        result = result.wrapping_mul(base);
        i += 1;
    }
    result
}

#[inline]
fn word(bytes: &[u8], at: usize) -> u64 {
    u64::from(u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]))
}

#[inline]
fn block_hash(bytes: &[u8], at: usize) -> u64 {
    let mut h = 0u64;
    for w in 0..BLOCK_WORDS {
        h = h.wrapping_mul(HASH_BASE).wrapping_add(word(bytes, at + w * 4));
    }
    h
}

#[inline]
fn mix(h: u64) -> u64 {
    let x = (h ^ (h >> 31)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^ (x >> 29)
}

/// Sort and merge `(offset, length)` ranges; empty ones are dropped.
pub(crate) fn merge_ranges(ranges: &mut Vec<(u64, u64)>) {
    ranges.retain(|&(_, len)| len > 0);
    ranges.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for &(start, len) in ranges.iter() {
        let end = start.saturating_add(len);
        match merged.last_mut() {
            Some((last_start, last_len)) if start <= *last_start + *last_len => {
                *last_len = (*last_len).max(end - *last_start);
            }
            _ => merged.push((start, len)),
        }
    }
    *ranges = merged;
}

/// Where the 64-byte blocks of some ROM ranges are, by content: what the recorder looks up the
/// changed bytes of a keyframe in.
///
/// Blocks are indexed at ROM offsets that are a multiple of the stride (64 bytes, or coarser for
/// more than [`MAX_INDEX_ENTRIES`] blocks), and the changed bytes are probed at every
/// 4-byte-aligned position, so a copy is found when the game's buffer and the file offset it read
/// agree modulo 4 (sector and word-aligned reads, which is what games do) and it is at least
/// stride + 60 bytes long. Constant blocks (zero padding) are left out: zstd stores those for
/// nothing anyway.
pub(crate) struct RomIndex {
    /// Upper 32 bits of each block's mixed hash (never 0; 0 = empty slot).
    keys: Vec<u32>,
    /// Block number (ROM offset / `stride`).
    blocks: Vec<u32>,
    mask: usize,
    stride: u64,
    filter: Vec<u64>,
    filter_shift: u32,
}

impl RomIndex {
    /// Index the blocks of `ranges` (merged, see [`merge_ranges`]) in `rom`. `None` when there is
    /// nothing to index.
    pub(crate) fn build(rom: &[u8], ranges: &[(u64, u64)]) -> Option<Self> {
        Self::build_with_limit(rom, ranges, MAX_INDEX_ENTRIES)
    }

    fn build_with_limit(rom: &[u8], ranges: &[(u64, u64)], max_entries: u64) -> Option<Self> {
        let rom_len = rom.len() as u64;
        let covered: u64 = ranges.iter().map(|&(start, len)| start.saturating_add(len).min(rom_len).saturating_sub(start)).sum();
        let stride = (BLOCK as u64) * covered.div_ceil(BLOCK as u64 * max_entries).max(1).next_power_of_two();
        // Block numbers are 32-bit.
        if rom_len / stride > u64::from(u32::MAX) {
            return None;
        }
        let blocks = |&(start, len): &(u64, u64)| -> core::ops::Range<u64> {
            let end = start.saturating_add(len).min(rom_len);
            let last = end.saturating_sub(BLOCK as u64) / stride + u64::from(end >= BLOCK as u64);
            let first = start.div_ceil(stride);
            first..last.max(first)
        };
        let count: u64 = ranges.iter().map(|r| blocks(r).end - blocks(r).start).sum();
        if count == 0 {
            return None;
        }
        let count = count as usize;
        let slots = (count * 2).next_power_of_two();
        let filter_bits = (count * 8).next_power_of_two().clamp(1 << 16, 1 << 27);
        let mut index = Self {
            keys: alloc::vec![0; slots],
            blocks: alloc::vec![0; slots],
            mask: slots - 1,
            stride,
            filter: alloc::vec![0; filter_bits / 64],
            filter_shift: 64 - filter_bits.trailing_zeros(),
        };
        for range in ranges {
            for block in blocks(range) {
                let at = (block * stride) as usize;
                let bytes = &rom[at..at + BLOCK];
                if bytes.iter().all(|&b| b == bytes[0]) {
                    continue;
                }
                index.insert(mix(block_hash(rom, at)), block as u32);
            }
        }
        Some(index)
    }

    fn insert(&mut self, hash: u64, block: u32) {
        let bit = hash >> self.filter_shift;
        self.filter[(bit / 64) as usize] |= 1 << (bit % 64);
        let key = ((hash >> 32) as u32).max(1);
        let mut slot = hash as usize & self.mask;
        loop {
            if self.keys[slot] == 0 {
                self.keys[slot] = key;
                self.blocks[slot] = block;
                return;
            }
            if self.keys[slot] == key {
                return; // same content hash: the first block is as good as any
            }
            slot = (slot + 1) & self.mask;
        }
    }

    /// The ROM offset of a block whose hash is `hash` (the bytes must still be compared).
    #[inline]
    fn lookup(&self, hash: u64) -> Option<u64> {
        let bit = hash >> self.filter_shift;
        if self.filter[(bit / 64) as usize] & (1 << (bit % 64)) == 0 {
            return None;
        }
        let key = ((hash >> 32) as u32).max(1);
        let mut slot = hash as usize & self.mask;
        loop {
            match self.keys[slot] {
                0 => return None,
                k if k == key => return Some(u64::from(self.blocks[slot]) * self.stride),
                _ => slot = (slot + 1) & self.mask,
            }
        }
    }

    /// The copies of ROM bytes in `data` (a region diff's data stream, whose offsets are all
    /// word-aligned positions of the state), longest possible, sorted and non-overlapping.
    pub(crate) fn find_copies(&self, rom: &[u8], data: &[u8]) -> Vec<RomCopy> {
        let mut copies = Vec::new();
        if data.len() < BLOCK {
            return copies;
        }
        let last = data.len() - BLOCK;
        let mut floor = 0usize; // where the previous copy ended
        let mut i = 0usize;
        let mut h = block_hash(data, 0);
        loop {
            if let Some(rom_at) = self.lookup(mix(h)) {
                let rom_at = rom_at as usize;
                if rom[rom_at..rom_at + BLOCK] == data[i..i + BLOCK] {
                    // Grow it both ways, byte by byte.
                    let (mut start, mut rom_start) = (i, rom_at);
                    while start > floor && rom_start > 0 && data[start - 1] == rom[rom_start - 1] {
                        start -= 1;
                        rom_start -= 1;
                    }
                    let (mut end, mut rom_end) = (i + BLOCK, rom_at + BLOCK);
                    while end < data.len() && rom_end < rom.len() && data[end] == rom[rom_end] {
                        end += 1;
                        rom_end += 1;
                    }
                    copies.push(RomCopy { data_offset: start, rom_offset: rom_start as u64, len: end - start });
                    floor = end;
                    i = end.next_multiple_of(4);
                    if i > last {
                        break;
                    }
                    h = block_hash(data, i);
                    continue;
                }
            }
            if i + 4 > last {
                break;
            }
            h = h.wrapping_sub(word(data, i).wrapping_mul(HASH_TOP)).wrapping_mul(HASH_BASE).wrapping_add(word(data, i + BLOCK));
            i += 4;
        }
        copies
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::pseudo_random_bytes;

    #[test]
    fn copies_round_trip_through_a_v9_payload() {
        let rom = pseudo_random_bytes(3, 200_000);
        // Data: noise, a ROM copy at an unaligned-but-word-congruent place, noise, two adjacent
        // copies, a short (unfindable) copy, and constant bytes.
        let mut data = pseudo_random_bytes(4, 1000);
        data.extend_from_slice(&rom[4096 + 12..4096 + 12 + 3000]);
        data.extend_from_slice(&pseudo_random_bytes(5, 500));
        data.extend_from_slice(&rom[50_000..51_000]);
        data.extend_from_slice(&rom[90_004..92_000]);
        data.extend_from_slice(&rom[120_000..120_100]);
        data.extend_from_slice(&[0u8; 700]);
        let len = data.len();
        data.truncate(len - len % 4);

        let index = RomIndex::build(&rom, &[(0, rom.len() as u64)]).unwrap();
        let copies = index.find_copies(&rom, &data);
        let copied: usize = copies.iter().map(|c| c.len).sum();
        assert!(copied >= 3000 + 1000 + 1996 - 3 * 128, "found only {copied} bytes in {copies:?}");
        for c in &copies {
            assert_eq!(&data[c.data_offset..c.data_offset + c.len], &rom[c.rom_offset as usize..c.rom_offset as usize + c.len]);
        }
        assert!(copies.windows(2).all(|w| w[0].data_offset + w[0].len <= w[1].data_offset));

        let control = [1u8, 2, 3];
        let payload = encode_payload_v9(&control, &data, &copies);
        assert!(payload.len() < data.len() - copied + 200);
        let parts = split_payload_v9(&payload).unwrap();
        assert_eq!(parts.control, &control);
        let mut rebuilt = Vec::new();
        parts.data_into(Some(&rom), &mut rebuilt).unwrap();
        assert_eq!(rebuilt, data);
        assert!(parts.data_into(None, &mut rebuilt).is_err(), "copies need the ROM");
        assert!(parts.data_into(Some(&rom[..1000]), &mut rebuilt).is_err(), "a copy past the end of the ROM is refused");

        // No copies: literal only, and no ROM needed.
        let payload = encode_payload_v9(&control, &data, &[]);
        let mut rebuilt = Vec::new();
        split_payload_v9(&payload).unwrap().data_into(None, &mut rebuilt).unwrap();
        assert_eq!(rebuilt, data);
    }

    #[test]
    fn only_the_given_ranges_are_indexed() {
        let rom = pseudo_random_bytes(9, 100_000);
        let data = rom[40_000..42_000].to_vec();
        assert!(RomIndex::build(&rom, &[(0, 30_000)]).unwrap().find_copies(&rom, &data).is_empty());
        let copies = RomIndex::build(&rom, &[(39_000, 5_000)]).unwrap().find_copies(&rom, &data);
        assert_eq!(copies, [RomCopy { data_offset: 0, rom_offset: 40_000, len: 2000 }]);
        assert!(RomIndex::build(&rom, &[(10, 20)]).is_none(), "no whole block");
    }

    /// More blocks than the index may hold: a coarser stride, and copies long enough to span a
    /// stride are still found (short ones are not).
    #[test]
    fn a_large_rom_is_indexed_at_a_coarser_stride() {
        let rom = pseudo_random_bytes(11, 300_000);
        let index = RomIndex::build_with_limit(&rom, &[(0, rom.len() as u64)], 1000).unwrap();
        assert_eq!(index.stride, 512, "300 KB / 1000 entries rounds up to 512-byte blocks");
        let mut data = pseudo_random_bytes(12, 400);
        data.extend_from_slice(&rom[100_004..100_004 + 2000]);
        data.extend_from_slice(&rom[200_000..200_200]);
        let copies = index.find_copies(&rom, &data);
        assert_eq!(copies, [RomCopy { data_offset: 400, rom_offset: 100_004, len: 2000 }]);
    }

    #[test]
    fn ranges_merge() {
        let mut r = alloc::vec![(100, 50), (0, 10), (140, 100), (10, 5), (500, 0), (300, 1)];
        merge_ranges(&mut r);
        assert_eq!(r, [(0, 15), (100, 140), (300, 1)]);
    }
}
