//! # safeboy
//! 
//! Safe Rust bindings for the SameBoy emulator, an accurate Game Boy Color emulator written in C
//! by Lior Halphon.

#![no_std]
#![warn(missing_docs)]

const _: () = {
    assert!(size_of::<usize>() >= size_of::<u32>());
};

pub use sameboy_sys::GB_VERSION;

extern crate alloc;

pub mod rgb_encoder;

mod instance;

pub use instance::*;

mod model;
pub use model::*;

/// Seed SameBoy's random number generator, which decides the contents of RAM, HRAM, OAM and the
/// wave RAM at every reset (and the camera's noise).
///
/// The generator is one per process, seeded from the clock at start-up, so by default two
/// resets of the same ROM (in one process or in two) start from different garbage. Seeding it
/// with the same value right before each reset makes resets reproducible: a replay's
/// `ResetConsole`, and a reset of a console linked to another machine's, then lead to the same
/// memory everywhere.
pub fn seed_random(seed: u64) {
    unsafe { sameboy_sys::GB_random_seed(seed) }
}
