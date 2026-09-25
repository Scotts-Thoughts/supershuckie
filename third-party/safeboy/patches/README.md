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

- `0002-rendered-dmg-palettes.patch` — `RunnableInstanceFunctions::get_rendered_dmg_palettes` /
  `set_rendered_dmg_palettes` and the `RenderedDmgPalettes` type: read and overwrite the colors
  SameBoy's renderer draws the background palette and object palettes 0 and 1 with (its internal
  `background_palettes_rgb` / `object_palettes_rgb` tables, reached through the bindings' struct
  layout and only after that layout is checked against fields the API sets). A presentation-only
  override; `supershuckie-core`'s Game Boy core uses it for custom Game Boy colors.

To regenerate a patch after editing (`0002` is the diff from the tree with `0001` applied): `diff -ruN <registry copy>/src src`.
