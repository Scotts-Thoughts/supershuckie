# Super Game Boy Colours for Red and Blue — Implementation Plan

**Target:** Claude Code, working in the `SnowyMouse/supershuckie` repository (branch off `ram-tools`).
**Goal:** Pokémon Red and Blue (and every other SGB-enhanced Game Boy game) run with their Super Game
Boy palettes in Super Shuckie, out of the box and without the user having to know which two settings to
combine.

---

## 0. What already exists (read this first)

The core work is done, and it works. Verified on 2026-09-13 against `~/Documents/stp-pokered/pokered.gbc`
(header: `0x143 = 0x00` DMG-only, `0x146 = 0x03` SGB-enhanced, `0x14B = 0x33`):

| Model handed to `GameBoyColor::new_from_rom` | Screen after the intro |
|---|---|
| `Model::DmgB` | 4 greys only (`000000`, `555555`, `AAAAAA`, `FFFFFF`) |
| `Model::Sgb2` | Red's own palettes (`F7B58C`, `84739C`, `181010`, `FFEFFF`; the Game Freak intro is fully multi-coloured) |

Pieces already in place:

* **Core** (`supershuckie-core/src/emulator/game_boy_color.rs`): SameBoy 1.0.2 via `safeboy 0.3.0-beta.6`.
  `Model::Sgb2` runs SameBoy's SNES HLE (`GB_is_hle_sgb`), which answers `MLT_REQ` (how Red/Blue detect an
  SGB) and applies `PAL_*`/`ATTR_*` packets to the 160x144 output. `BorderMode::Never` keeps the picture
  160x144. `hard_reset` skips the SNES-side boot animation by patching `intro_animation` in a save state
  (fixed offset `0x1AB66`; `core_name()` reports "with SGB intro skipped"). SGB2 is used rather than SGB
  because SGB2 runs the CPU at the stock 4.194 MHz, so `frame_rate()` stays 4194304/70224.
* **Boot ROM:** the 256-byte `bootrom/dmg/dmg.bin` stub is used for SGB2 too. It never sends the cartridge
  header packets (`0xF1..0xFB`), so SameBoy never runs its header check and never sets
  `disable_commands`; commands from the game are accepted, which is what we want. (Trade-off in §5.)
* **Replays:** `ReplayConsoleType::SuperGameBoy2` is written by `replay_console_type()`, and replay
  playback forces `SuperShuckieEmulatorType::GameBoySGB2` (`supershuckie-frontend/src/lib.rs` ~412).
  `docs`/`replay-size-reduction-spec.md` already lists SGB.
* **Frame server:** `supershuckie-frame-server/src/server.rs` maps `SuperGameBoy2` to `Model::Sgb2` with
  the same DMG stub; `source.rs` gives it 160x144.
* **Frontend & Qt:** `Settings → Game Boy settings → Enable SGB colors` (`GameBoySettings::sgb`, C API
  `supershuckie_frontend_set_sgb_enabled`).

### The actual problem: two hidden, interacting settings

`choose_for_game_boy` (`supershuckie-frontend/src/lib.rs` ~2319):

```
gbc_mode == AlwaysGBC            -> GameBoyColor            (SGB toggle ignored!)
gbc_mode == AlwaysGB             -> sgb ? GameBoySGB2 : GameBoy
gbc_mode == GBInGBMode (auto)    -> header[0x143]==0 ? (sgb ? GameBoySGB2 : GameBoy) : GameBoyColor
```

Defaults are `gbc_mode = AlwaysGBC` and `sgb = false`. So a fresh install boots Red as a Game Boy Color
game (CGB boot ROM compatibility palette), and ticking "Enable SGB colors" does nothing until the user
also changes "Game Boy Color mode" to "Game Boy Color games only" or "Always Game Boy". Nothing in the
UI or docs says so. That is the whole gap.

**Workaround available today:** Settings → Game Boy settings → Game Boy Color mode → *Game Boy Color
games only*, then tick *Enable SGB colors*. The ROM reloads as "Super Game Boy 2".

---

## 1. Design

One rule, stated in the menu and in the docs: **an SGB-enhanced game runs on the Super Game Boy whenever
the SGB option is on, regardless of the Game Boy Color mode.** Non-SGB games are unaffected by the option
unless they are already running in Game Boy mode (existing behaviour, kept).

New selection rule (pure function, unit-tested):

```
fn choose_game_boy_hardware(mode: GameBoyMode, sgb: bool, header: &[u8]) -> SuperShuckieEmulatorType
  let sgb_enhanced = header[0x146] == 0x03 && header[0x14B] == 0x33;   // same test SameBoy's HLE applies
  let cgb_capable  = header[0x143] & 0x80 != 0;                          // 0x80 or 0xC0
  if sgb && sgb_enhanced && mode != AlwaysGBC_forced_by_user_for_this_rom { return GameBoySGB2 }
  match mode {
      AlwaysGBC  => GameBoyColor,
      AlwaysGB   => if sgb { GameBoySGB2 } else { GameBoy },
      GBInGBMode => if cgb_capable { GameBoyColor } else if sgb { GameBoySGB2 } else { GameBoy },
  }
```

Decisions:

* **Yellow and other dual-mode carts** (`0x143 = 0x80`, also SGB-enhanced): with `sgb` on they become
  SGB2 too. Do that deliberately: it is what "SGB colors on" means, and a user who prefers Yellow's GBC
  palette turns the option off. Not silently keeping them on GBC avoids a second hidden rule. Say this in
  the docs.
* **`0x143` test:** the existing code tests `== 0x00`; switch to `& 0x80` so odd header bytes are treated
  as DMG-only, which is what a CGB does.
* **Default for `sgb`:** keep `false` for existing installs; ship the setting as before, so nobody's
  recordings change console type under them. Recording a replay writes `SuperGameBoy2`, which old builds
  already understand.
* **Menu wording:** rename "Enable SGB colors" to "Super Game Boy colors for SGB-enhanced games" and
  give the "Game Boy Color mode" submenu a status-tip so the interaction is visible. Show the running
  hardware in the window title (`supershuckie_frontend_get_emulator_type_name`, "Super Game Boy 2" already
  exists) so the user can see which model is live.

---

## 2. Implementation steps

1. **Extract and test the selection rule** — `supershuckie-frontend/src/lib.rs`
   Move the body of `choose_for_game_boy` into a free `fn choose_game_boy_hardware(mode, sgb, header)` and
   add a `#[cfg(test)]` truth table: Red (DMG-only, SGB), Yellow (dual, SGB), Crystal (dual, no SGB flag),
   a DMG-only non-SGB game, an all-`0xFF` header, a ROM shorter than `0x150`. `reload_game_boy_if_needed`
   and `load_rom` keep calling the method; nothing else changes.

2. **Menu text and title** — `supershuckie-qt/src/main_window.cpp` (~948), `main_window.hpp`
   Rename the action, add `setStatusTip`/tooltip text on it and on the three GBC-mode actions describing
   the rule from §1, and append the hardware name to the window title on load/reload (there is already a
   reload path in `reload_game_boy_if_needed`; the title update hooks the same place the ROM name is set).
   The C API (`supershuckie-frontend-c`) needs no new functions.

3. **Frame server parity** — `supershuckie-frame-server/src/server.rs`
   No behaviour change needed (it follows the replay's console type), but add a comment pointing at
   `get_bios_for_core` so the DMG stub and `Model::Sgb2` stay paired in both places. If step 6 is done,
   both must switch together.

4. **Headless check** — new `supershuckie-core/examples/sgb_check.rs`
   Same shape as `gb_audio_check.rs`: boot a ROM on `Model::Sgb2` with the DMG stub, run N frames with a
   scripted Start/A mash, and fail unless the screen shows chromatic pixels (|r−g| or |g−b| > 8) by frame
   N, and also that the same ROM on `Model::DmgB` never does. Document the command in the README's
   testing notes. This is the regression test for a future `safeboy` bump (see step 5).

5. **Harden the intro skip** — `game_boy_color.rs` `hard_reset`
   `state[0x1AB66]` is a raw offset into SameBoy's save-state layout and will silently patch the wrong
   field after a `safeboy`/SameBoy upgrade. Replace it with a helper that walks the section headers the
   way `zero_pending_apu_cycles` does (8-byte header, then `u32` size + payload per section) to find the
   SGB section, and patch `intro_animation` at its known offset within that section. Add a test that
   patches, reloads, re-saves and reads the value back as 201, so a layout change fails loudly. The
   `GB_VERSION_WITH_HACKS` core name stays.

6. **Docs** — `README.md`, new `docs/game_boy_hardware.md`
   A short "Super Game Boy colours" section: the rule in §1, the three GBC modes, what Red/Blue/Yellow do
   under each combination, that replays record the hardware and that RAM-tool addresses are identical
   between GB and SGB2 (no `WRAMX` region on either). Link from the README's settings area.

7. **Verify in the app** (`/run` skill)
   Load `pokered.gbc` with defaults → GBC compatibility palette and title "Game Boy Color". Tick the SGB
   option → reload, title "Super Game Boy 2", coloured title screen. Record a 10-second replay, reopen it
   in a fresh instance with the option off → still plays back in colour (console type comes from the
   replay). Serve that replay through the frame server and confirm coloured frames. Run `sgb_check` on
   `pokered.gbc` and `pokeblue.gbc`.

Suggested commits: (1)+(2) "SGB colours: honour the option for SGB-enhanced games", (4)+(5) "SGB: headless
colour check, section-walking intro skip", (6) docs.

---

## 3. Files touched

| File | Change |
|---|---|
| `supershuckie-frontend/src/lib.rs` | pure selection fn + tests, `0x143 & 0x80`, title hook |
| `supershuckie-qt/src/main_window.cpp/.hpp` | action rename, status tips, title |
| `supershuckie-core/src/emulator/game_boy_color.rs` | section-walking intro skip + test |
| `supershuckie-core/examples/sgb_check.rs` | new |
| `supershuckie-frame-server/src/server.rs` | comment only |
| `README.md`, `docs/game_boy_hardware.md` | docs |

Nothing here changes the replay format, save states, the C ABI, or the boot ROMs, so no attribution
script changes and no Windows build risk.

---

## 4. Out of scope, deliberately (possible follow-ups)

* **SGB border.** Red/Blue upload a custom border. `BorderMode::Always` makes SameBoy output 256x224.
  The Qt side would follow (screen dimensions arrive through the change-video-mode callback), but
  `supershuckie-frame-server/src/source.rs` hard-codes 160x144 for SGB2 and export geometry would grow,
  and the audio shadow must keep `BorderMode::Never`. Needs its own setting; not needed for palettes.
* **SGB colour correction.** Palette colours come out as raw 5→8-bit expansion (`0x1F → 0xFF`);
  `safeboy` exposes `set_color_correction_mode` if a CRT-like look is ever wanted. Cosmetic.
* **Per-ROM hardware override.** `get_rom_config_or_default` only stores `save_name` today; a per-ROM
  hardware choice would slot in there if the global option proves too coarse.

---

## 5. The real SGB2 boot ROM: considered, not recommended now

SameBoy's `sgb2_boot.asm` (MIT) is in the user's checkout at `~/Documents/SameBoy/BootROMs/` (0.16.3; the
crate bundles SameBoy 1.0.2, so take the file from a matching tag) and `rgbasm` is installed at
`/opt/homebrew/bin`. Using it would make SameBoy run its header check, disable SGB commands for
non-SGB games and apply the SGB's built-in palette table to old titles. Costs:

* the Game Boy-side logo scroll comes back (the DMG stub exists to skip it for content creation);
* a new BIOS hash in every SGB2 replay, so `load_builtin_bios_override` (`lib.rs` ~483, currently GBA-only)
  must learn to pick the old stub for old replays, and `server.rs` must switch in lockstep;
* `scripts/make-attributions.py` `section_boot_roms` must list it or the Windows build fails.

None of that is needed for Red/Blue palettes, which already work with the stub. Revisit only if a
non-SGB game misbehaves because stray joypad writes are being parsed as packets.
