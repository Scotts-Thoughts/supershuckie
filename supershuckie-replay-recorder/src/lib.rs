//! TODO
#![no_std]

#![warn(missing_docs)]

#[cfg(feature = "std")]
extern crate std;

extern crate alloc;

pub mod replay_file;
pub mod keyframe_masks;
pub mod bookmarks;

mod packet;
mod util;

#[cfg(test)]
mod test_support;

pub use packet::*;
pub use util::*;
pub use bookmarks::{Bookmark, BookmarkTable, BookmarkTypeRecord, KEYFRAME_BOOKMARK_LEAD_FRAMES};
