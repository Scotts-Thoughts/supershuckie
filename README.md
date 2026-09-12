# Super Shuckie

This is an emulation frontend for GBC, GBA, and NDS, optimized for content
creation.

It is licensed under version 3 of the GNU General Public License as published by
the Free Software Foundation in 2007. Unless otherwise specified, it is not
available under any other license.

## How to obtain

For Windows builds, refer to the [Releases] page.

[Releases]: https://github.com/SnowyMouse/supershuckie/releases

We do not currently provide pre-built binaries for Linux or macOS. Refer to the
build instructions if you are using either of these systems.

## Building

You will need Qt6, SDL3, Rust 1.89, CMake 4.0, Git, a C17 compiler, and a C++20
compiler.

### Linux (GNU/Linux)

On Linux, refer to your package manager for obtaining all needed software.

In many cases, you can also use Rustup to get Rust.

> **NOTE:** Some distros may not have all required software available through
> built-in repositories, or they may lack sufficient versions of some software.
> 
> If you have issues on your particular distro, don't be afraid to ask for help
> from the community, but be advised that we cannot guarantee support for all
> distros.

Run `build.sh` and locate the executables in the `build` directory.

`build.sh` builds melonDS through `scripts/build-melonds.sh`, which applies Super Shuckie's
local melonDS patches (`melonds-rs/patches/`) and builds the core with LTO. For the fastest
Nintendo DS emulation also give it a training workload, which enables profile-guided
optimisation (12-18% faster interpretation, bit-identical emulation):

```
PGO_ROM=/path/to/game.nds PGO_REPLAY="/path/to/one.replay /path/to/another.replay" ./build.sh
```

### macOS

On macOS, you can satisfy these requirements by installing the following:
* Xcode Command Line Tools
* Homebrew (`brew install sdl3 qt@6`)
* Rustup

Run `build.sh` and locate the executables in the `build` directory.

### Windows

Install [MSYS2](https://www.msys2.org/) and, from its UCRT64 environment, the toolchain:
`pacman -S mingw-w64-ucrt-x86_64-{gcc,cmake,ninja,pkgconf,qt6-base,sdl3,rust} git`. Then, from
PowerShell with `C:\msys64\ucrt64\bin` on `PATH`:

```
git submodule update --init --recursive
cmake -G Ninja ./mgba-rs/mgba -B build/mgba -DLIBMGBA_ONLY=ON -DDISABLE_FRONTENDS=ON -DCMAKE_BUILD_TYPE=Release
cmake --build build/mgba -j
.\scripts\build-melonds.ps1 -Rom game.nds -Replay one.replay, another.replay   # or no arguments for a plain LTO build
cmake -G Ninja ./supershuckie-qt -B build -DCMAKE_BUILD_TYPE=Release -DSCRIPT_BUILD=ON
cmake --build build -j
```

`scripts/build-melonds.ps1` applies the local melonDS patches and builds the core with LTO;
given a ROM and one or more replays it also trains a profile (PGO), which makes DS
interpretation 12-18% faster with bit-identical emulation. The result is `build\supershuckie.exe`
(it needs the MSYS2 `bin` directory on `PATH` for the Qt/SDL DLLs; build with
`-DSUPERSHUCKIE_STATIC=ON` against the static Qt package for a standalone exe).

## Performance notes

* The Nintendo DS core always runs melonDS's interpreter: the JIT compiles blocks from what it
  observes on their first execution, so it is not reproducible across a save-state load and
  cannot be used with replays. `nds-performance-report.md` has the measurements.
* From 2x speed up only one frame in `floor(speed)` is composited (the display cannot show more
  than 60 a second anyway); the rest are emulated but not drawn, which is about a quarter
  cheaper per frame. Emulation and save states are unaffected. Seeks, video export and speeds
  below 2x always draw every frame.
* The status bar shows the emulation rate (emulated frames per second, drawn or not); hover it
  for the frame-time average, worst case and budget. The REST `stats` endpoint reports the same
  as `emulation_fps`, `frame_time_ms`, `frame_budget_ms` and `frames_over_budget`.
* `supershuckie-core/examples/nds_bench.rs` is a headless benchmark and determinism checker
  for the DS core (throughput per replay segment, keyframe cost, `--verify` against a replay's
  recorded keyframes, JIT/interpreter reproducibility). Its header says how to link it.

## Audio

Audio is off by default. The **Audio** menu turns it on and holds every audio setting: mute,
volume, "Mute when sped up" (on by default, so turbo and any base speed other than 1x stay silent
and sound returns the moment the game is back at 1x) and the buffer size. Playback never touches
emulation: what a replay records and plays back is the same with audio on or off. For Game Boy
games the emulated SameBoy instance stays exactly as it always was and the sound comes from a
second instance run in lockstep with it (SameBoy's joypad-bounce emulation would otherwise change
with the sample rate); `supershuckie-core/examples/gb_audio_check.rs` and `nds_bench --audio
--verify` are the checks for this.

## Converting old replays

Replays recorded before format v4 (September 2026) can be re-encoded offline into the current
format, which is typically 3-6x smaller for Nintendo DS and Game Boy Advance recordings, without
losing anything: every emulated frame, keyframe, bookmark, counter and the header (crop markers,
patch) are carried over, and `--verify` re-reads both files packet by packet afterwards.

```
cargo build --release -p supershuckie-replay-recorder --features convert
target/release/supershuckie-replay-convert <in.replay> <out.replay> --verify
```

Run it with `--help` for the encoding options. Keep the original until `--verify` has passed.

To convert a whole `UserData` folder, `convert-replays.ps1` (Windows PowerShell) walks every
`<ROM>-data\replays\*.replay`, converts and verifies each one in place with several processes at
a time, and moves the verified originals to a backup folder (`-DeleteOriginals` to delete them
instead). Start with `.\convert-replays.ps1 -DryRun`; see the script header for the options.
