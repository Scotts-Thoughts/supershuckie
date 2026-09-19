# safeboy patches

`../` is safeboy 0.3.0-beta.6 as published on crates.io, plus the patches here (already
applied; the files are kept so the change can be reviewed and offered upstream). The workspace
selects this copy over the crates.io release with `[patch.crates-io]` in the root `Cargo.toml`.

- `0001-serial-slave-api.patch` — the slave side of SameBoy's serial API
  (`RunnableInstanceFunctions::serial_get_data_bit` / `serial_set_data_bit`, wrapping
  `GB_serial_get_data_bit` / `GB_serial_set_data_bit`), `set_infrared_input`
  (`GB_set_infrared_input`), and `Gameboy::running_instance_ptr` (a stable pointer to the pinned
  running instance). Together they let the master instance of a link cable exchange bits with the
  slave instance from its `serial_transfer_bit_end` callback, the way SameBoy's own frontend
  links two windows. Used by `supershuckie-core`'s `emulator::link`. The same patch also makes
  `direct_access` hand out an empty slice for a region the cartridge does not have
  (`GB_get_direct_access` returns a null pointer for cartridge RAM on a ROM-only cartridge, which
  `slice::from_raw_parts_mut` must not be given), and adds `safeboy::seed_random`
  (`GB_random_seed`), so a reset's RAM garbage can be made reproducible.

To regenerate the patch after editing: `diff -ruN <registry copy>/src src`.
