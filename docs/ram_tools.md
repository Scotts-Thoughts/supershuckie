# RAM tools

Super Shuckie can show, search, watch, edit and freeze the running game's memory while you play.
Everything is in the **Tools** menu:

| Menu item | Shortcut | What it does |
|---|---|---|
| RAM viewer | Ctrl+Alt+V | Live hex view of a memory region |
| New RAM viewer window | | Another viewer (up to four), each on its own region |
| RAM search | Ctrl+Alt+F | Find a value by scanning memory and narrowing the results |
| RAM watch | Ctrl+Alt+W | A saved list of addresses shown as values |
| Unfreeze all | | Stop holding every frozen value |
| Ask before editing memory while recording | | See [Replays](#replays) |
| Open / Reload character tables | | See [Character tables](#character-tables) |

Every tool is its own window. Open windows, their positions and the viewers' layout are restored
the next time Super Shuckie starts.

## Performance

The tools are built so they do not slow the game down:

* Nothing is copied while every tool window is closed and nothing is frozen or logged.
* The viewers and watch window get only the bytes on screen, refreshed 15, 30 or 60 times per
  second of real time (the **Refresh** setting in a viewer, shared by all tool windows), not per
  emulated frame, so running at 4x does not cost more than 1x.
* A watch that logs changes or pauses the game, and a frozen value, are checked once per emulated
  frame: a few dozen nanoseconds each. At most 64 watches can log or pause at once, and at most 256
  values can be frozen.
* A search copies all of memory once per scan (about 0.1-0.4 ms for the 4 MiB of a Nintendo DS)
  and does the comparing on a separate low-priority thread.

`supershuckie-core/examples/nds_bench.rs --mm-*` and `supershuckie-frontend/examples/ram_tools_pacing.rs`
measure this.

## Addresses

Addresses are the ones Poke-A-Byte uses for each console,
so an address from a Poke-A-Byte mapper works here unchanged, and replays store edits with the same
addresses. Some regions go beyond what Poke-A-Byte uses; those addresses are free in its address
space.

You can type an address in any of these forms:

| Form | Example | Meaning |
|---|---|---|
| Hexadecimal | `0x02024284`, `$2024284`, `2024284` | Always hexadecimal, with or without a prefix |
| Region and offset | `EWRAM:24284`, `ewram+0x24284` | An offset into a region, by its short name |
| Pointer path | `[0x03005008]+0xC` | Read the 32-bit pointer at `0x03005008`, add `0xC` |
| Nested pointers | `[[EWRAM:1D2C]+4]-8` | Up to four pointers deep (offsets are hexadecimal) |

Pointer paths are useful on the Game Boy Advance and Nintendo DS, where games keep much of their data
on the heap; the watch window shows the address each pointer path currently resolves to.

### Game Boy / Game Boy Color

Multi-byte values default to big-endian (Pokémon Gen 1-2 store stats big-endian and money as BCD).

| Region | Short name | Addresses | Notes |
|---|---|---|---|
| VRAM | `VRAM` | `0x8000`-`0x9FFF` | Bank 0 |
| WRAM (banks 0-1) | `WRAM` | `0xC000`-`0xDFFF` | The first 8 KiB of work RAM, whatever bank is switched in |
| WRAM (banks 2-7) | `WRAMX` | `0x10000`-`0x15FFF` | The rest of the Game Boy Color's work RAM (not on the Game Boy) |
| OAM | `OAM` | `0xFE00`-`0xFE9F` | |
| I/O registers | `IO` | `0xFF00`-`0xFF7F` | Read-only in the tools |
| HRAM | `HRAM` | `0xFF80`-`0xFFFE` | |
| Cartridge RAM | `CART` | `0x20000` onwards | All of the save RAM, every bank (the real `0xA000` window only shows one) |

### Game Boy Advance

Little-endian; the real bus addresses.

| Region | Short name | Addresses | Notes |
|---|---|---|---|
| EWRAM | `EWRAM` | `0x02000000`-`0x0203FFFF` | |
| IWRAM | `IWRAM` | `0x03000000`-`0x03007FFF` | |
| Palette RAM | `PAL` | `0x05000000`-`0x050003FF` | Many games copy their palette in every frame, overwriting edits |
| VRAM | `VRAM` | `0x06000000`-`0x06017FFF` | |
| OAM | `OAM` | `0x07000000`-`0x070003FF` | |
| Save data | `SAVE` | `0x0E000000` onwards | SRAM, flash or EEPROM contents; empty until the game first uses its save |

### Nintendo DS

Little-endian; ARM9 addresses.

| Region | Short name | Addresses | Notes |
|---|---|---|---|
| Main RAM | `MAIN` | `0x02000000`-`0x023FFFFF` | |
| Shared WRAM | `SWRAM` | `0x03000000`-`0x03007FFF` | Which CPU sees which half depends on WRAMCNT |
| ARM7 WRAM | `WRAM7` | `0x03800000`-`0x0380FFFF` | |

## RAM viewer

* Click a region tab (or press Ctrl+1 to Ctrl+9, or Ctrl+PgUp/PgDn) to switch regions. Each region
  remembers where you were in it.
* **Go to address** (Ctrl+G) accepts any address form above and switches to the right region.
  Back and forward (Alt+Left / Alt+Right) walk through where you have been.
* **Row** sets bytes per row; **Group** shows 2- or 4-byte values in the chosen byte order.
* Bytes that change flash orange and fade over a second. Frozen bytes have a blue background.
* The inspector on the right reads the value under the cursor as every integer type, f32, BCD, a
  pointer (double-click it to follow) and text.
* Right-click for copy, **Freeze**, **Unfreeze**, **Paste hex over selection**, **Fill**, **Add
  watch** and **Search for this value**.

### Editing

Press **Edit** (or Insert) and type hexadecimal digits over bytes, or characters in the text column.
Each completed byte is written at the next frame boundary; it stays outlined until a refresh shows
it. You can also double-click a value in the inspector and type a new one.

Ctrl+Z and Ctrl+Shift+Z undo and redo edits and freeze changes from any tool window (one history is
shared by all of them). An edit made before a save state load or reset cannot be undone.

## RAM search

1. Choose the **Value** type (u8 to i32, f32, BCD, bytes or text), its size and byte order, and the
   alignment (candidates at every 1, 2 or 4 bytes).
2. Choose the **Regions** to search (optionally an address range).
3. Pick a comparison and press **New search**. "Unknown value" keeps every address.
4. Play until the value changes, then pick a comparison such as "Decreased by 1", "Changed" or
   "Equal to 42" and press **Scan** (or Enter). Repeat until few results remain.

Comparisons against a number (equal, less than, between, one of…) work for new searches and
refinements; comparisons with the previous scan (changed, increased by…) and the first scan only
refine. Bytes and text searches use a pattern: `12 ?? 3F` (`?` matches any hexadecimal digit), or
text through the chosen character table.

**Undo** and **Redo** step through scans. **Pause while scanning** pauses the game for each scan.
Scans are exact to the frame: memory is copied between two frames. The results table shows each
candidate's current value, its value at the last scan and at the first scan; double-click one to
show it in the viewer, or right-click to add it to the watch list, set its value or freeze it.

A search belongs to its game: loading a different ROM ends it. Loading a save state does not (the
status line notes that memory was replaced).

## RAM watch

Watches are saved per ROM, in `ram-watch.json` in the ROM's data folder, as you change them.
**Import** and **Export** share watch lists between ROM versions or people.

Each watch has a label, an address (any form above, including pointer paths), a type and display
base, and optionally a group, notes, and:

* **Log every change**: every change, frame by frame, goes to the change log at the bottom.
  Loading a state, resetting or seeking a replay appears as a separator, not as a change.
* **Pause emulation** when the value changes, becomes a value, rises above or falls below one, or
  goes up or down by an amount. The watch window comes up with the watch selected when it happens.
* **Freeze at** a value.

Double-click a watch's value to change it (or its frozen value).

## Freezing

A frozen value is checked after every emulated frame; if the game changed it, it is written back
before the next frame starts. Within a frame the game can still see its own value briefly, the same
as with cheat devices that write memory every frame. Editing a frozen value changes what it is
frozen at.

Frozen values are saved with the watch list but always load unfrozen, so opening a game never starts
changing its memory by itself. The status bar shows **N FROZEN** while anything is frozen (click it
to open the watch window).

## Replays

Edits and freezes use the same recorded write path as Poke-A-Byte:

* **While recording**, every edit, and every frame on which a freeze had to restore its value, is
  written into the replay, so the replay plays back exactly as it was recorded. The first edit of a
  recording asks for confirmation (turn that off with "Don't ask again" or in the Tools menu), and
  the status bar shows **RAM MODIFIED** while the recording contains tool writes. Starting or
  resuming a recording with frozen values asks whether to keep them.
* **During playback** memory cannot be edited and freezes are suspended; the replay's own writes are
  applied as recorded.

A replay that writes to a region earlier versions of Super Shuckie did not map (anything outside
Game Boy VRAM/WRAM/HRAM, Game Boy Advance EWRAM/IWRAM and Nintendo DS main RAM) crashes those
versions when playback reaches the write; this version skips writes it cannot apply.

## Character tables

The viewer's text column, text searches and text watches decode bytes through a character table.
ASCII is built in. Put `.tbl` files in the tables folder (**Tools > Open character tables folder**)
and choose **Reload character tables**. The format is one entry per line:

```
; comment
80=A
81=B
E6=!
0000=PKMN
```

The left side is one to four hexadecimal bytes, the right side what they stand for. Lines starting
with `;` or `#` are comments; lines starting with `/` (end markers) or `*` (line breaks) are ignored.
