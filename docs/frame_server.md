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
supershuckie-frame-server                            serve on stdin/stdout
supershuckie-frame-server --probe <replay> [--layout N]
supershuckie-frame-server --version
```

`--probe` reads only the replay's header and index (no ROM, no emulation, blobs stay compressed)
and prints one JSON object: console, geometry for the layout, native frame rate, frame count,
sample rate, ROM hash (blake3, lowercase hex) and names, recording core, keyframe count,
bookmarks, crop range and counters. A 120 MB DS replay probes in well under 100 ms. A file it
cannot read prints `{"error": "..."}` and exits 1.

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

Requests are parsed on a reader thread; the emulation loop checks for newer requests every eight
hidden runs and between the frames of a `Run`, and abandons a walk that a newer `Frame`/`Run` or a
`Cancel` has superseded (answering `Cancelled`). Speed changes recorded in the replay are ignored
so audio is always one emulated second per 48 000 samples.

The DS core runs the interpreter only: the JIT is not reproducible against recorded keyframes.
