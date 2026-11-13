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
* Anything else loads the nearest keyframe at least three frames before the target and steps
  from there, or a snapshot from the core's seek index when one is nearer
  (`SuperShuckieCore::go_to_replay_seek_start`).

The seek index (every console but the 3DS): every walk leaves a snapshot behind every 15 frames
(every 60 once it has run on for 8 seconds, which is playback or an export passing through), kept
in memory as a compressed diff against the recording's keyframe, 128 MiB at most per served
recording (`SUPERSHUCKIE_SEEK_INDEX_MB` in the server's environment changes that; `0` turns the
index off), least recently used keyframe interval dropped first. A later request near a frame already visited then starts
from the snapshot: stepping back a frame, or coming back to where the playhead was, emulates a
handful of frames instead of up to two seconds of them. Measured on Nintendo DS recordings
(2026-11-11, `seek_index_check`): 30-70 ms a step back against 90-220 ms, and the frame served is
the same one, state and picture.

The index of a Nintendo DS recording is also filled ahead of the requests and kept between
sessions:

* It lives in a file in the local cache directory the app uses too
  (`%LOCALAPPDATA%\SuperShuckie\seek-index` on Windows), named after a fingerprint of the
  recording and the game. A server that opens a recording reads the file first, so a recording
  walked once (by the app or by any server) is never walked again by the same build.
  `SUPERSHUCKIE_SEEK_INDEX_DIR` puts the files elsewhere (empty or `off`: no files),
  `SUPERSHUCKIE_SEEK_INDEX_DISK_MB` limits what they may take together (4096; the ones used
  longest ago are deleted; `0`: no files).
* What the file lacks is walked by one more emulator with the same game on a background thread
  (below normal priority), nearest the last frame asked for first, about 500 frames a second.
  `SUPERSHUCKIE_SEEK_PREFILL_WORKERS=<n>` changes how many (`0`: none; without a file the
  default is none). It holds another copy of the game in memory until the recording is walked.
* Of all the programs that have the same recording open (an editor runs several servers per
  recording, and the app may have it open as well), only one runs workers: the one that got
  the file's fill lock (`<fingerprint>.filling` next to it). It writes the file about once a
  minute while it fills and when it is done; the others look at the file every few seconds and
  read it again when it has changed.

With the index filled the first request into a stretch is as cheap as a return to it (measured
2026-11-11 on Pokemon Platinum: 35 ms a random seek against 80 ms).

The pictures `Info` names as cheap (`keyframes`) are six frames past each keyframe, not three: a
Nintendo DS keyframe leaves the 3D engine's geometry out, a game drawing 3D at 30 fps has rebuilt
it four or five frames on, and a picture asked for sooner has to be drawn again from the keyframe
before. On Pokémon Platinum keyframe + 3 cost 390-470 ms and keyframe + 5 23 ms.

A Nintendo DS recording of format v11 stores what the game loaded from its cartridge as
references into the ROM; the server hands the ROM it was opened with to the replay's reader, so
nothing changes for a client. A server built before v11 refuses such a file ("Unrecognized replay
format version 11"), so the editor needs the server from the same build as the emulator that
made the recording, as after every format change.

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
