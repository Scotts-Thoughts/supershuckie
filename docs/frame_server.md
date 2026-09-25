# Frame server

`supershuckie-frame-server` is a small headless binary that plays a replay and hands the
pictures and sound to another program over its standard input and output. It exists so a
video editor can treat a recording like a video file: ask for frame `n`, get frame `n`.

It links `supershuckie-core` and the replay recorder only (no Qt, no SDL, no web server) and is
built by `build.sh` next to the app; `cargo build --release -p supershuckie-frame-server` builds
it on its own once the emulator cores under `build/` exist (set `SUPERSHUCKIE_BUILD_DIR` to point
elsewhere).

## Wire format

The protocol is not ours to define: the binary implements Cutter's frame-server protocol,
version 1, exactly as written in Cutter's `docs/frame-server-protocol.md`. Anything that speaks
that protocol can drive this binary; nothing below changes it.

## Modes

```text
supershuckie-frame-server [--gb-colors <colors>]     serve on stdin/stdout
supershuckie-frame-server --probe <replay> [--layout N]
supershuckie-frame-server --version
```

`--gb-colors` draws Game Boy games (and Game Boy games on a Game Boy Color, which get the boot
ROM's colors for them) with custom colors instead of their own, as the app does with
**Settings › Game Boy › Custom colors…** switched on: twelve `RRGGBB` values separated by commas,
the four shades (lightest first) of the background palette, then of object palette 0, then of
object palette 1. Game Boy Color games and Super Game Boy colors are unaffected. It changes how
the pictures are drawn and nothing else; the app keeps its own colors in `settings.json` under
`game_boy_settings.custom_colors`, so a caller can pass exactly what the app shows.

`--probe` reads only the replay's header and index (no ROM, no emulation, blobs stay compressed)
and prints one JSON object: console, geometry for the layout, native frame rate, frame count,
sample rate, ROM hash (blake3, lowercase hex) and names, recording core, keyframe count,
bookmarks, crop range and counters. A 120 MB DS replay probes in well under 100 ms. A file it
cannot read prints `{"error": "..."}` and exits 1.

Each bookmark in the probe is `{"name", "frame", "id", "out_frame", "keyframe", "type"}`: `frame`
is the in frame, `out_frame` is `null` for a point, and `type` is `{"name", "color": "#RRGGBB"}` as
recorded in the replay, or `null`. The `Info` reply carries only each bookmark's name and in frame,
in frame order, as the protocol defines.

## What "frame `n`" means

Picture `n` is what the screens hold after `n` emulated frames, with picture `0` being the first
frame ever drawn (the same picture as `1`); this is exactly what `SuperShuckieCore::export_frames`
produces for a range starting at `n`, so the server and an export of the same replay agree byte
for byte. Pixels are the emulator's `0xAARRGGBB` words in memory order (B, G, R, A). The sound of
picture `n` is what the run that produced it emitted; `Audio` requests return interleaved stereo
`i16` at 48 kHz for a run of pictures.

## How it reaches a frame

* The frame already on the screens is answered without emulating.
* A frame up to 240 ahead is reached by stepping: hidden runs (no drawing) up to the frame before
  it, then one drawn run.
* Anything else loads the nearest keyframe at least three frames before the target
  (`SuperShuckieCore::go_to_replay_keyframe`, the first half of `go_to_replay_frame`) and steps
  from there.

`Memory` asks for the state behind a picture rather than the picture: `(address, length)` blocks
of the core's memory once picture `n` has been produced, read exactly as the live Poke-A-Byte
integration reads them (`EmulatorCore::read_ram`, a block the core cannot read left as zeros),
concatenated in the order asked. It positions the core exactly as `Frame` does, so a `Frame` for
the same picture afterwards costs nothing, and it is superseded and cancelled like one. This is
how the editor serves a recording's memory to Poke-A-Byte as its playhead moves.

Requests are parsed on a reader thread; the emulation loop checks for newer requests every eight
hidden runs and between the frames of a `Run`, and abandons a walk that a newer `Frame`/`Run` or a
`Cancel` has superseded (answering `Cancelled`). Speed changes recorded in the replay are ignored
so audio is always one emulated second per 48 000 samples.

The DS core runs the interpreter only: the JIT is not reproducible against recorded keyframes.

## One file on Windows

The Windows build links libstdc++ and winpthread as static archives (`build.rs` names
`libstdc++.a` and `libpthread.a` outright), so the executable is the whole install: point the
editor at it and nothing else has to be beside it or on `PATH`. `objdump -p
supershuckie-frame-server.exe | findstr DLL` should list only system libraries (`KERNEL32`,
`msvcrt`, `SHLWAPI`, …); a `libstdc++-6.dll` or `libgcc_s_seh-1.dll` in that list means the
static archives were not found and the import libraries were taken instead.

## Stdout is the wire

Nothing but protocol bytes may reach standard output. Before the first request is read, `serve`
duplicates stdout's handle for the protocol and points descriptor 1 at stderr (`dup2`). This
redirects the C runtime's descriptor 1 — what the cores' C glue writes to with `printf` ("Bad
BIOS", "Failed to init mGBA", each followed by `std::terminate()`) — so that output lands in the
log the client keeps rather than in the middle of a frame. It does not touch Rust's own stdout
(on Windows in particular, `_dup2` on descriptor 1 does not move the Win32 handle Rust's stdout
writes through), so Rust code in the server uses `eprintln!` directly instead, which it does.
Cutter reads that log back into its error message together with the exit status, which is how a
server that cannot start on a machine says why.
