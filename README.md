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

### macOS

On macOS, you can satisfy these requirements by installing the following:
* Xcode Command Line Tools
* Homebrew (`brew install sdl3 qt@6`)
* Rustup

Run `build.sh` and locate the executables in the `build` directory.

### Windows

TODO

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
