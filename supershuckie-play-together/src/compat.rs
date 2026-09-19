//! Compatibility checks between two replay metadata records and display-name hygiene.

use supershuckie_replay_recorder::replay_file::{ReplayConsoleType, ReplayFileMetadata};

use crate::protocol::MAX_DISPLAY_NAME_BYTES;

/// Whether a follower with `local` metadata can run a publisher's stream.
#[derive(Clone, Debug, PartialEq)]
pub enum FollowCompatibility {
    /// Same console, ROM, core and BIOS.
    Ok,
    /// Different console.
    ConsoleMismatch {
        /// The publisher's console.
        theirs: ReplayConsoleType,
    },
    /// Different ROM checksum.
    RomMismatch,
    /// Different emulator core (name and version).
    CoreMismatch {
        /// The publisher's core.
        theirs: String,
    },
    /// Different BIOS checksum.
    BiosMismatch,
}

/// Compare two metadata records, in the order console, ROM, core, BIOS; the first difference
/// wins.
///
/// In Play Together the follower loads the publisher's ROM, so the frontend uses this for the
/// console/core/BIOS checks between two records and decides for itself what a ROM mismatch means.
pub fn follow_compatibility(local: &ReplayFileMetadata, publisher: &ReplayFileMetadata) -> FollowCompatibility {
    if local.console_type != publisher.console_type {
        return FollowCompatibility::ConsoleMismatch { theirs: publisher.console_type };
    }
    if local.rom_checksum != publisher.rom_checksum {
        return FollowCompatibility::RomMismatch;
    }
    if local.emulator_core_name != publisher.emulator_core_name {
        return FollowCompatibility::CoreMismatch { theirs: publisher.emulator_core_name.clone() };
    }
    if local.bios_checksum != publisher.bios_checksum {
        return FollowCompatibility::BiosMismatch;
    }
    FollowCompatibility::Ok
}

/// A family of consoles a link cable can join: two games link only within one family.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum LinkFamily {
    /// Game Boy, Game Boy Color and Super Game Boy 2 (the Game Boy link cable).
    GameBoy,
    /// Game Boy Advance (the GBA link cable).
    GameBoyAdvance,
}

/// The link cable family of a console, or `None` when it cannot link.
pub fn link_family(console: ReplayConsoleType) -> Option<LinkFamily> {
    match console {
        ReplayConsoleType::GameBoy | ReplayConsoleType::SuperGameBoy2 | ReplayConsoleType::GameBoyColor => Some(LinkFamily::GameBoy),
        ReplayConsoleType::GameBoyAdvance => Some(LinkFamily::GameBoyAdvance),
        ReplayConsoleType::Unknown | ReplayConsoleType::NintendoDS => None,
    }
}

/// Whether two consoles can be joined by a link cable.
pub fn can_link(a: ReplayConsoleType, b: ReplayConsoleType) -> bool {
    matches!((link_family(a), link_family(b)), (Some(x), Some(y)) if x == y)
}

/// A sentence for the user about why `display_name`'s session cannot be followed.
pub fn describe_incompatibility(compat: &FollowCompatibility, display_name: &str) -> String {
    match compat {
        FollowCompatibility::Ok => format!("{display_name}'s session can be followed."),
        FollowCompatibility::ConsoleMismatch { theirs } => {
            format!("{display_name} is playing on a {theirs}, which is a different console from yours.")
        }
        FollowCompatibility::RomMismatch => format!("{display_name} is playing a different ROM from yours."),
        FollowCompatibility::CoreMismatch { theirs } => {
            format!("{display_name} is using the emulator core '{theirs}', which is not the one you have loaded.")
        }
        FollowCompatibility::BiosMismatch => format!("{display_name} is using a different BIOS from yours."),
    }
}

/// Trim, strip control characters, clamp to [`MAX_DISPLAY_NAME_BYTES`] on a character boundary;
/// an empty result becomes `"Player"`.
pub fn sanitize_display_name(requested: &str) -> String {
    let cleaned: String = requested.chars().filter(|c| !c.is_control()).collect();
    let cleaned = cleaned.trim();
    let mut name = String::new();
    for c in cleaned.chars() {
        if name.len() + c.len_utf8() > MAX_DISPLAY_NAME_BYTES {
            break;
        }
        name.push(c);
    }
    let name = name.trim_end().to_owned();
    if name.is_empty() { "Player".to_owned() } else { name }
}

/// [`sanitize_display_name`], then make it unique against `taken` by appending ` (2)`, ` (3)`, ...
///
/// The suffix counts toward the byte cap: the base is shortened so the result still fits.
pub fn dedupe_display_name(requested: &str, taken: &[String]) -> String {
    let base = sanitize_display_name(requested);
    if !taken.iter().any(|t| t == &base) {
        return base;
    }
    for n in 2u32.. {
        let suffix = format!(" ({n})");
        let room = MAX_DISPLAY_NAME_BYTES.saturating_sub(suffix.len());
        let mut stem = String::new();
        for c in base.chars() {
            if stem.len() + c.len_utf8() > room {
                break;
            }
            stem.push(c);
        }
        let candidate = format!("{}{suffix}", stem.trim_end());
        if !taken.iter().any(|t| t == &candidate) {
            return candidate;
        }
    }
    unreachable!("the counter runs out of u32 before names run out")
}
