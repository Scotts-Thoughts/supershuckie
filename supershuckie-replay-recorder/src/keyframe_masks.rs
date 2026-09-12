//! Transient-buffer masks for delta keyframes.
//!
//! Some emulator save states carry large buffers that are pure *output*: the game regenerates
//! them from scratch every frame it renders, and nothing the CPU computes can ever read them. They
//! change completely between keyframes and are a large share of every keyframe delta (23-66% on
//! Nintendo DS, ~60% on Game Boy Advance) for information that is thrown away one frame later.
//!
//! [`transient_ranges`] identifies those buffers by inspecting the state bytes themselves (magic
//! numbers, section lengths, driver identifiers); when any check fails it returns nothing and the
//! keyframe is stored losslessly. The recorder ([`apply_masks`]) copies the ranges forward from the
//! previous keyframe before diffing, so the masked bytes contribute nothing to any delta of the
//! chain. Restart keyframes (the first of each blob, frame 0) are always exact, and the chain stays
//! self-consistent without any reader-side knowledge: a reader reconstructs exactly what the
//! recorder diffed against.
//!
//! After loading a delta keyframe the emulator therefore holds the geometry / audio of the chain's
//! restart keyframe for at most one frame, until the game overwrites it. That only ever affects
//! what is *presented* (and only if a frame were shown before the game runs), never what is
//! computed, so playback, seeking and resume stay deterministic.
//!
//! | Console | Buffer | Why it is output-only |
//! |---|---|---|
//! | Nintendo DS (melonDS) | `GP3D` section: `VertexRAM[2][6144]` (12,288 x 64 B) and `PolygonRAM[2][2048]` (4,096 x 188 B) | The geometry engine's polygon/vertex banks. Written by the GX FIFO, read only by the rasteriser; the CPU cannot read them. The game re-submits the whole scene every frame it renders 3D. |
//! | Game Boy Advance (mGBA) | m4a `SoundInfo.pcmBuffer`, 1,584 x 2 bytes | The sound driver's mixed-PCM ring buffer, fully refilled every VBlank by the mixer and read only by the sound DMA. |
//!
//! Game Boy / Game Boy Color / Super Game Boy states have no masks.

use alloc::vec::Vec;
use core::ops::Range;

use crate::replay_file::ReplayConsoleType;

/// melonDS `Savestate` magic.
const MELN_MAGIC: &[u8; 4] = b"MELN";

/// melonDS `SAVESTATE_MAJOR` these offsets were derived from (`Savestate.h`).
const MELN_MAJOR: u16 = 13;

/// Size of the melonDS global header; sections start right after it.
const MELN_HEADER_LEN: usize = 16;

/// Size of a melonDS section header (`magic`, `u32 length` including the header, 8 reserved).
const MELN_SECTION_HEADER_LEN: usize = 16;

/// `GPU3D::DoSavestate` section magic.
const GP3D_MAGIC: &[u8; 4] = b"GP3D";

/// Total `GP3D` section length (header included) for `MELN_MAJOR`. Every field of
/// `GPU3D::DoSavestate` (`GPU3D.cpp`) is fixed-size, so any other length means a different layout.
const GP3D_SECTION_LEN: usize = 1_572_864;

/// Offset of `VertexRAM` from the start of the `GP3D` section header: 16 header bytes + 7,393
/// payload bytes (command FIFOs, matrices and stacks, viewport, test results, temp vertex buffer,
/// bank counters).
const GP3D_VERTEX_RAM_OFFSET: usize = 7_409;

/// `VertexRAM[6144 * 2]` at 64 bytes each, immediately followed by `PolygonRAM[2048 * 2]` at 188
/// bytes each. `CmdStallQueue`, the render list and the rest of the section come after and stay
/// live.
const GP3D_TRANSIENT_LEN: usize = 12_288 * 64 + 4_096 * 188;

/// mGBA `GBASerializedState` size (`gba/serialize.h`); extdata (save data, RTC) may follow.
const GBA_STATE_LEN: usize = 0x61000;

/// `versionMagic` is `GBASavestateMagic (0x01000000) + version`.
const GBA_MAGIC: u32 = 0x0100_0000;

/// Offset of `iwram` in `GBASerializedState`.
const GBA_IWRAM_OFFSET: usize = 0x19000;

/// GBA IWRAM address range.
const GBA_IWRAM_BASE: u32 = 0x0300_0000;
const GBA_IWRAM_SIZE: u32 = 0x8000;

/// Every m4a/MP2K game keeps a pointer to its `SoundInfo` here.
const M4A_SOUND_INFO_POINTER: u32 = 0x0300_7FF0;

/// `SoundInfo.ident` while the driver is initialised (`"Smsh"`).
const M4A_SOUND_INFO_IDENT: u32 = 0x6873_6D53;

/// `sizeof(SoundInfo)`.
const M4A_SOUND_INFO_SIZE: u32 = 0xFB0;

/// `pcmBuffer` follows the 0x50-byte header and 12 x 0x40-byte `SoundChannel`s.
const M4A_PCM_BUFFER_OFFSET: usize = 0x350;

/// `PCM_DMA_BUF_SIZE (1584) * 2`: double-buffered mixed PCM.
const M4A_PCM_BUFFER_LEN: usize = 0xC60;

fn u32_le(state: &[u8], offset: usize) -> Option<u32> {
    state.get(offset..offset + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// The melonDS `GP3D` vertex + polygon banks, if `state` is a melonDS savestate of the expected
/// version with a `GP3D` section of the expected length.
fn nds_transient_range(state: &[u8]) -> Option<Range<usize>> {
    if state.get(..4)? != MELN_MAGIC {
        return None;
    }
    let major = u16::from_le_bytes([state[4], state[5]]);
    if major != MELN_MAJOR {
        return None;
    }

    // Walk the sections (see `Savestate::FindSection`).
    let mut offset = MELN_HEADER_LEN;
    while let Some(header) = state.get(offset..offset + MELN_SECTION_HEADER_LEN) {
        let section_len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
        if section_len < MELN_SECTION_HEADER_LEN {
            return None;
        }

        if &header[..4] == GP3D_MAGIC {
            if section_len != GP3D_SECTION_LEN || offset.checked_add(section_len)? > state.len() {
                return None;
            }
            let start = offset + GP3D_VERTEX_RAM_OFFSET;
            return Some(start..start + GP3D_TRANSIENT_LEN);
        }

        offset = offset.checked_add(section_len)?;
    }

    None
}

/// The m4a `SoundInfo.pcmBuffer`, if `state` is an mGBA GBA state whose game runs the m4a/MP2K
/// sound driver (looked up through the driver's own pointer at `0x03007FF0`).
fn gba_transient_range(state: &[u8]) -> Option<Range<usize>> {
    if state.len() < GBA_STATE_LEN || u32_le(state, 0)? & 0xFFFF_0000 != GBA_MAGIC {
        return None;
    }

    let pointer = u32_le(state, GBA_IWRAM_OFFSET + (M4A_SOUND_INFO_POINTER - GBA_IWRAM_BASE) as usize)?;
    if pointer < GBA_IWRAM_BASE || pointer.checked_add(M4A_SOUND_INFO_SIZE)? > GBA_IWRAM_BASE + GBA_IWRAM_SIZE {
        return None;
    }

    let sound_info = GBA_IWRAM_OFFSET + (pointer - GBA_IWRAM_BASE) as usize;
    if u32_le(state, sound_info)? != M4A_SOUND_INFO_IDENT {
        return None;
    }

    let start = sound_info + M4A_PCM_BUFFER_OFFSET;
    Some(start..start + M4A_PCM_BUFFER_LEN)
}

/// Byte ranges of `state` that hold regenerated output buffers (see the module documentation).
///
/// Returns nothing unless every layout check passes, so an unexpected core version silently falls
/// back to lossless behaviour. Pure function of the state bytes: the recorder, the resync splice
/// and the converter's verifier all agree on the ranges.
pub fn transient_ranges(console: ReplayConsoleType, state: &[u8]) -> Vec<Range<usize>> {
    let range = match console {
        ReplayConsoleType::NintendoDS => nds_transient_range(state),
        ReplayConsoleType::GameBoyAdvance => gba_transient_range(state),
        ReplayConsoleType::Unknown
        | ReplayConsoleType::GameBoy
        | ReplayConsoleType::SuperGameBoy2
        | ReplayConsoleType::GameBoyColor => None,
    };

    range.into_iter().collect()
}

/// Copy the transient ranges of `prev` over `cur`, so that a delta of `cur` against `prev` carries
/// nothing for them.
///
/// Does nothing unless both states have the same length and the same transient layout (a layout
/// change between two consecutive keyframes would mean the copied bytes are not the same buffers).
pub fn apply_masks(console: ReplayConsoleType, prev: &[u8], cur: &mut [u8]) {
    if prev.len() != cur.len() {
        return;
    }

    let ranges = transient_ranges(console, cur);
    if ranges.is_empty() || ranges != transient_ranges(console, prev) {
        return;
    }

    for range in ranges {
        cur[range.clone()].copy_from_slice(&prev[range]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::pseudo_random_bytes;
    use alloc::vec;

    /// A synthetic MELN buffer: a few filler sections, then a GP3D section of `gp3d_len`, then
    /// one more section.
    fn synthetic_meln(major: u16, gp3d_len: usize) -> (Vec<u8>, usize) {
        let mut state = Vec::new();
        state.extend_from_slice(MELN_MAGIC);
        state.extend_from_slice(&major.to_le_bytes());
        state.extend_from_slice(&0u16.to_le_bytes());
        state.extend_from_slice(&0u32.to_le_bytes()); // length, patched below
        state.extend_from_slice(&0u32.to_le_bytes());

        let section = |state: &mut Vec<u8>, magic: &[u8; 4], len: usize| {
            let start = state.len();
            state.extend_from_slice(magic);
            state.extend_from_slice(&(len as u32).to_le_bytes());
            state.extend_from_slice(&[0u8; 8]);
            state.extend(pseudo_random_bytes(len as u64, len - MELN_SECTION_HEADER_LEN));
            start
        };

        section(&mut state, b"NDSG", 40);
        section(&mut state, b"ARM9", 1000);
        let gp3d = section(&mut state, GP3D_MAGIC, gp3d_len);
        section(&mut state, b"GPUG", 200);

        let len = state.len() as u32;
        state[8..12].copy_from_slice(&len.to_le_bytes());
        (state, gp3d)
    }

    #[test]
    fn nds_gp3d_section_is_masked() {
        let (state, gp3d) = synthetic_meln(MELN_MAJOR, GP3D_SECTION_LEN);
        assert_eq!(gp3d, 16 + 40 + 1000);
        let expected = gp3d + 7_409..gp3d + 1_563_889;
        assert_eq!(transient_ranges(ReplayConsoleType::NintendoDS, &state), vec![expected.clone()]);
        assert_eq!(expected.len(), 786_432 + 770_048);
        // The range lies inside the section, after the header, and leaves the tail live.
        assert!(expected.end < gp3d + GP3D_SECTION_LEN);

        // Other consoles never mask an NDS state.
        assert!(transient_ranges(ReplayConsoleType::GameBoyAdvance, &state).is_empty());
        assert!(transient_ranges(ReplayConsoleType::GameBoy, &state).is_empty());
    }

    #[test]
    fn nds_layout_checks_fall_back_to_lossless() {
        // Wrong section length.
        let (state, _) = synthetic_meln(MELN_MAJOR, GP3D_SECTION_LEN + 4);
        assert!(transient_ranges(ReplayConsoleType::NintendoDS, &state).is_empty());
        // Wrong major version.
        let (state, _) = synthetic_meln(MELN_MAJOR + 1, GP3D_SECTION_LEN);
        assert!(transient_ranges(ReplayConsoleType::NintendoDS, &state).is_empty());
        // Wrong magic.
        let (mut state, _) = synthetic_meln(MELN_MAJOR, GP3D_SECTION_LEN);
        state[0] = b'X';
        assert!(transient_ranges(ReplayConsoleType::NintendoDS, &state).is_empty());
        // Truncated inside the GP3D section.
        let (state, gp3d) = synthetic_meln(MELN_MAJOR, GP3D_SECTION_LEN);
        assert!(transient_ranges(ReplayConsoleType::NintendoDS, &state[..gp3d + 100_000]).is_empty());
        // No GP3D section at all, and garbage.
        let (state, gp3d) = synthetic_meln(MELN_MAJOR, GP3D_SECTION_LEN);
        assert!(transient_ranges(ReplayConsoleType::NintendoDS, &state[..gp3d]).is_empty());
        assert!(transient_ranges(ReplayConsoleType::NintendoDS, &[]).is_empty());
        assert!(transient_ranges(ReplayConsoleType::NintendoDS, &[0; 100]).is_empty());
        // A section claiming a zero length must not loop forever.
        let mut bad = state.clone();
        bad[16 + 4..16 + 8].copy_from_slice(&0u32.to_le_bytes());
        assert!(transient_ranges(ReplayConsoleType::NintendoDS, &bad).is_empty());
    }

    /// A synthetic mGBA state with a SoundInfo block at IWRAM address `sound_info`.
    fn synthetic_gba(sound_info: u32, ident: u32) -> Vec<u8> {
        let mut state = pseudo_random_bytes(0x6BA, GBA_STATE_LEN + 128 * 1024 + 16);
        state[0..4].copy_from_slice(&(GBA_MAGIC + 0xA).to_le_bytes());
        state[GBA_IWRAM_OFFSET + 0x7FF0..GBA_IWRAM_OFFSET + 0x7FF4].copy_from_slice(&sound_info.to_le_bytes());
        if (GBA_IWRAM_BASE..GBA_IWRAM_BASE + GBA_IWRAM_SIZE).contains(&sound_info) {
            let si = GBA_IWRAM_OFFSET + (sound_info - GBA_IWRAM_BASE) as usize;
            if si + 4 <= state.len() {
                state[si..si + 4].copy_from_slice(&ident.to_le_bytes());
            }
        }
        state
    }

    #[test]
    fn gba_pcm_buffer_is_masked() {
        // Emerald's SoundInfo lives at 0x03006380, FireRed's at 0x03005F50.
        for address in [0x0300_6380u32, 0x0300_5F50] {
            let state = synthetic_gba(address, M4A_SOUND_INFO_IDENT);
            let si = GBA_IWRAM_OFFSET + (address - GBA_IWRAM_BASE) as usize;
            assert_eq!(transient_ranges(ReplayConsoleType::GameBoyAdvance, &state), vec![si + 0x350..si + 0x350 + 0xC60]);
            assert!(transient_ranges(ReplayConsoleType::NintendoDS, &state).is_empty());
        }
    }

    #[test]
    fn gba_layout_checks_fall_back_to_lossless() {
        // Wrong ident (a game without the m4a driver).
        let state = synthetic_gba(0x0300_6380, 0x1234_5678);
        assert!(transient_ranges(ReplayConsoleType::GameBoyAdvance, &state).is_empty());
        // Pointer outside IWRAM, or too close to its end for a whole SoundInfo.
        for bad in [0x0200_0000u32, 0x0300_8000, 0x0300_7FF0 - 0x100, 0xFFFF_FFFF, 0] {
            let state = synthetic_gba(bad, M4A_SOUND_INFO_IDENT);
            assert!(transient_ranges(ReplayConsoleType::GameBoyAdvance, &state).is_empty(), "{bad:#X}");
        }
        // Wrong magic.
        let mut state = synthetic_gba(0x0300_6380, M4A_SOUND_INFO_IDENT);
        state[3] = 0x02;
        assert!(transient_ranges(ReplayConsoleType::GameBoyAdvance, &state).is_empty());
        // Too short.
        let state = synthetic_gba(0x0300_6380, M4A_SOUND_INFO_IDENT);
        assert!(transient_ranges(ReplayConsoleType::GameBoyAdvance, &state[..GBA_STATE_LEN - 1]).is_empty());
    }

    #[test]
    fn apply_masks_copies_previous_bytes_only_inside_the_range() {
        let prev = synthetic_gba(0x0300_6380, M4A_SOUND_INFO_IDENT);
        let original = synthetic_gba(0x0300_6380, M4A_SOUND_INFO_IDENT);
        let mut cur = original.clone();
        // Make cur differ everywhere.
        for b in cur.iter_mut() {
            *b = b.wrapping_add(1);
        }
        // ...but keep the layout markers so both states resolve the same range.
        cur[0..4].copy_from_slice(&prev[0..4]);
        cur[GBA_IWRAM_OFFSET + 0x7FF0..GBA_IWRAM_OFFSET + 0x7FF4].copy_from_slice(&prev[GBA_IWRAM_OFFSET + 0x7FF0..GBA_IWRAM_OFFSET + 0x7FF4]);
        let si = GBA_IWRAM_OFFSET + 0x6380;
        cur[si..si + 4].copy_from_slice(&prev[si..si + 4]);
        let before = cur.clone();

        apply_masks(ReplayConsoleType::GameBoyAdvance, &prev, &mut cur);

        let range = si + 0x350..si + 0x350 + 0xC60;
        assert_eq!(&cur[range.clone()], &prev[range.clone()]);
        assert_eq!(&cur[..range.start], &before[..range.start]);
        assert_eq!(&cur[range.end..], &before[range.end..]);

        // Different lengths or layouts: untouched.
        let mut cur2 = before.clone();
        apply_masks(ReplayConsoleType::GameBoyAdvance, &prev[..prev.len() - 1], &mut cur2);
        assert_eq!(cur2, before);
        let mut cur3 = before.clone();
        let mut prev_other_layout = prev.clone();
        prev_other_layout[si] ^= 1; // ident mismatch -> no range for prev
        apply_masks(ReplayConsoleType::GameBoyAdvance, &prev_other_layout, &mut cur3);
        assert_eq!(cur3, before);
        // Consoles without masks: untouched.
        let mut cur4 = before.clone();
        apply_masks(ReplayConsoleType::GameBoyColor, &prev, &mut cur4);
        assert_eq!(cur4, before);
    }
}
