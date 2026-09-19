//! Player colours: each participant gets one that nobody else in the session has, so their
//! window can be told apart at a glance.

/// A player colour's wire value: an index into [`PALETTE`] (1-based). 0 means "no preference"
/// in a `Hello` and never appears in a `ParticipantInfo`.
pub type PlayerColor = u8;

/// "Let the host pick."
pub const COLOR_RANDOM: PlayerColor = 0;

/// One entry of the palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaletteEntry {
    /// What to call it.
    pub name: &'static str,
    /// `0xRRGGBB`.
    pub rgb: u32,
}

/// The colours a player may pick, in menu order. Index `i` here is wire value `i + 1`.
pub const PALETTE: [PaletteEntry; 22] = [
    PaletteEntry { name: "Pink", rgb: 0xF27FBC },
    PaletteEntry { name: "Red", rgb: 0xE02020 },
    PaletteEntry { name: "Tan", rgb: 0xD2A56D },
    PaletteEntry { name: "Orange", rgb: 0xFF8A0E },
    PaletteEntry { name: "Brown", rgb: 0x8B5A2B },
    PaletteEntry { name: "Pale Yellow", rgb: 0xF5F0A0 },
    PaletteEntry { name: "Yellow", rgb: 0xEBE129 },
    PaletteEntry { name: "Olive", rgb: 0x8F9E2E },
    PaletteEntry { name: "Lime", rgb: 0x7CFC00 },
    PaletteEntry { name: "Pale Green", rgb: 0x9BFF95 },
    PaletteEntry { name: "Green", rgb: 0x1E9E3A },
    PaletteEntry { name: "Cyan", rgb: 0x1CD3EA },
    PaletteEntry { name: "Teal", rgb: 0x1CA7EA },
    PaletteEntry { name: "Blue", rgb: 0x4C7BFF },
    PaletteEntry { name: "Navy", rgb: 0x2038C8 },
    PaletteEntry { name: "Dark Aqua", rgb: 0x3E6DB0 },
    PaletteEntry { name: "Bluish Grey", rgb: 0x6E7B9B },
    PaletteEntry { name: "Purple", rgb: 0x8A2BE2 },
    PaletteEntry { name: "Magenta", rgb: 0xE040FB },
    PaletteEntry { name: "White", rgb: 0xF2F2F2 },
    PaletteEntry { name: "Grey", rgb: 0x8C8C8C },
    PaletteEntry { name: "Black", rgb: 0x1A1A1A },
];

/// How many colours there are; always more than [`crate::MAX_PARTICIPANTS`].
pub const COLOR_COUNT: u8 = PALETTE.len() as u8;

/// The palette entry for a wire value, or `None` for 0 or out of range.
pub fn color_entry(color: PlayerColor) -> Option<&'static PaletteEntry> {
    if color == COLOR_RANDOM {
        None
    }
    else {
        PALETTE.get(usize::from(color) - 1)
    }
}

/// `true` for a wire value that names a palette entry.
pub fn is_valid_color(color: PlayerColor) -> bool {
    color_entry(color).is_some()
}

/// Pick a colour for a joining player: `requested` when it names a colour nobody in `taken`
/// has; otherwise one nobody has, chosen from `seed` so "random" is not always Pink. `taken`
/// values that are not colours are ignored.
pub fn assign_color(requested: PlayerColor, taken: &[PlayerColor], seed: u64) -> PlayerColor {
    let free = |c: PlayerColor| !taken.contains(&c);
    if is_valid_color(requested) && free(requested) {
        return requested;
    }
    let free_colors: Vec<PlayerColor> = (1..=COLOR_COUNT).filter(|&c| free(c)).collect();
    if free_colors.is_empty() {
        // More players than colours cannot happen (MAX_PARTICIPANTS < COLOR_COUNT), but never
        // hand out 0.
        return 1;
    }
    free_colors[(seed % free_colors.len() as u64) as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_outnumbers_participants() {
        assert!(usize::from(COLOR_COUNT) > crate::MAX_PARTICIPANTS);
        assert!(color_entry(0).is_none());
        assert_eq!(color_entry(1).unwrap().name, "Pink");
        assert_eq!(color_entry(COLOR_COUNT).unwrap().name, "Black");
        assert!(color_entry(COLOR_COUNT + 1).is_none());
    }

    #[test]
    fn assignment_respects_requests_and_avoids_taken() {
        assert_eq!(assign_color(5, &[], 0), 5);
        assert_ne!(assign_color(5, &[5], 0), 5);
        assert_ne!(assign_color(0, &[], 0), 0);
        assert_ne!(assign_color(99, &[], 0), 0);
        let taken: Vec<PlayerColor> = (1..=7).collect();
        for seed in 0..50 {
            let c = assign_color(0, &taken, seed);
            assert!(is_valid_color(c) && !taken.contains(&c));
        }
    }
}
