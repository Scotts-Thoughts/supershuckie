# Azahar (Nintendo 3DS) core spike — 2026-09-25

Question: can Azahar's core be driven headless the way Super Shuckie drives melonDS, and what do
frame rate, save-state size/time and determinism look like on a real Pokémon game? This is the
measurement half of the feasibility assessment. The architectural blockers (the `Core::System`
singleton, analog input in the replay format, Poke-A-Byte mappers, wireless local play) are
unchanged by anything here.

## What was built

- `third-party/azahar/` — shallow clone of https://github.com/azahar-emu/azahar (master, with
  submodules), git-ignored. Not a submodule yet. One patch applied, see below.
- `azahar-rs/spike/spike.cpp` — a headless harness in the shape of a core binding: no window, one
  emulated frame per call (run the core until the renderer's VBlank), input through Azahar's
  input-device factories (a fixed deterministic script, or an A-mashing mode that gets through
  the intro), pixels copied from the software renderer's screen buffers or read back from the
  OpenGL renderer through `TryPresent` into an offscreen framebuffer, in-memory save states,
  xxHash of the process heap / linear heap / FCRAM / VRAM, periodic frame dumps.
- `azahar-rs/spike/hook.cmake` + `spike.cmake` — the spike target is injected into Azahar's own
  configure with `-DCMAKE_PROJECT_citra_INCLUDE` (a deferred `include`), because Azahar's CMake
  hardcodes `CMAKE_SOURCE_DIR` for Boost and zstd and cannot be a subdirectory.
- `azahar-rs/patches/0001-supershuckie-deterministic-fs-delays.patch` — see Determinism.
- `scripts/build-azahar-spike.ps1` — configure + build the core libraries only (no Qt, SDL, Vulkan,
  room server, web service, audio devices, tests) with the same MSYS2 UCRT64 GCC as melonDS.
- `scripts/run-azahar-spike.ps1 -Rom <file>` — a six-run measurement matrix; results in
  `azahar-spike-results/<name>/` (git-ignored).

Build cost on this machine (GCC 16.1, Ninja): about 15 minutes wall for the first build, 1 GB
build directory, static spike exe 66 MB. Biggest archives: Crypto++ 19.8 MB, dynarmic 17.4 MB,
citra_core 11.3 MB, teakra 8.4 MB.

## Test payloads

- Pokémon X (USA), the decrypted `.3ds` from `A:\Dropbox\stp-projects\programs\roms\6`, copied
  under a `.cci` name (Azahar 2120+ refuses the `.3ds` extension; all four `.3ds` files there have
  the NoCrypto flag set, so they are usable as they are). It boots to the title screen with no
  system archives installed, and an A-mashing session reaches the overworld on Route 2 in about
  four emulated minutes.
- The open-source 3DS homebrew menu (3ds-hbmenu v2.4.3 `boot.3dsx`) as a light 2D control.

## Numbers — Pokémon X, RTX 3080 Ti, New 3DS mode unless noted

| Measure | Result |
|---|---|
| OpenGL, JIT, overworld gameplay, no input | 423 fps (2.4 ms per frame), 7x real time |
| OpenGL, JIT, five-minute A-mashing session from power-on (intro + overworld) | 365–433 fps |
| OpenGL, JIT, title screen with scripted input | 285–312 fps |
| OpenGL, interpreter, overworld | 254 fps |
| Software renderer, overworld | 3.8 fps (260 ms per frame) |
| Software renderer, title screen | 11.5 fps |
| Save state in the overworld | 13.0 MB zstd, 308 MB serialised; save 580–680 ms |
| Save state at the title screen | 14.7 MB zstd; Old 3DS mode 170 MB serialised, save 400 ms |
| Load state (same or another process) | 450–470 ms |
| Process heap / linear heap in use | 14.9 MB / 48.7 MB |
| ROM load to first frame | about 145 ms |
| Hash of heap + linear heap | about 7 ms per sample |

Homebrew menu for comparison: OpenGL 966–1131 fps, interpreter 501 fps, software 9.2 fps, state
1.4 MB compressed (303 MB serialised, the same fixed cost).

Frame dumps: `azahar-spike-results/x/boot-*.png` (title screen), `x-play/f-*-top.png`
(overworld at 3901, 7502, 11103, 14704, 18305 frames), `x-play/sw-*.bmp` (software renderer in
the overworld).

## Determinism

Hashes of the application's memory (process heap and linear heap, walked from the kernel's VMA
map) with identical scripted input in two separate processes, sampled every 150 or 300 frames:

| Comparison | Result |
|---|---|
| Two processes from power-on, first 2100 frames (boot + title), unpatched Azahar | heaps differed at frames 450 and 600, then converged in these runs; in the matrix one run stayed diverged for the whole run |
| Two processes from power-on, patched Azahar | identical at every sample (450 … 2100) |
| Two processes from a loaded overworld state, 1800 frames | identical at every sample |
| JIT vs interpreter (title screen) | identical heaps |
| Save → run 600 → hash, vs load → run 600 → hash | identical heaps (linear heap differs: the renderer's write-backs are not part of the state) |
| OpenGL vs software renderer | identical heaps; linear heap differs (the software renderer writes rendered frames back to memory, OpenGL does not) |

The divergence and its cause: Azahar's HLE file service completes reads and opens after an
emulated delay computed as `emulated delay − host time the operation took`, even with
`deterministic_async_operations` on (that setting only stops the work from running on a host
thread). So the emulated completion time of every ROM read depends on disk speed and on what
else the machine is doing, which is exactly what a replay or a Play Together follower cannot
tolerate. The patch in `azahar-rs/patches/` uses the full emulated delay in deterministic mode
(three sites: `File::Read`, `FS_USER::OpenFile`, `FS_USER::OpenFileDirectly`). It is small
enough to propose upstream.

Other things the spike pins that a binding would have to keep: fixed init clock and init ticks,
`deterministic_async_operations` on and `async_fs_operations` off, no audio stretching, Null
audio sink, static microphone input, no async shader compilation or presentation. A hash of all
of FCRAM is not stable even when the game's memory is identical (kernel-owned regions such as
the GSP shared memory move), so a 3DS sync hash must cover the process heap, not FCRAM.

## What this changes in the assessment

- **Headless driving works** with about 800 lines of glue, and the API is a close match to the
  `EmulatorCore` trait: run one frame, save/load a byte buffer, read process memory by virtual
  address, inject input, copy pixels.
- **Frame rate is fine with OpenGL**: 7x real time in the overworld on this GPU, and the
  interpreter alone would still be 4x. Fast-forward and replay seeking are workable.
- **OpenGL is mandatory.** The software rasterizer runs at 3.8 fps in the overworld, 16x too slow
  for real time. Every core thread therefore needs its own GL 4.3 context (hidden window + WGL
  here) and a per-frame read-back, followers included, and their frame rate is bounded by the GPU.
- **Save states cost 0.6 s to save and 0.45 s to load** regardless of content, because the whole
  256 MB FCRAM is serialised (128 MB in Old 3DS mode halves it). At 13 MB compressed a state is
  10–20x a DS keyframe. Periodic replay keyframes and follower resync snapshots need a different
  approach than the DS core's (a keyframe every few seconds would stall the game).
- **Determinism is good** with the settings above plus the patch, including across processes
  from a loaded state, which is what Play Together's sync hash and replay seeking need.
- Still untested: audio output, touch, system-archive-dependent features (none were needed to
  reach the overworld), and the singleton problem, which the spike does not touch (one 3DS per
  process remains the blocker for in-process followers and the link cable).

## Addendum: replay-format study (same day)

The spike grew `--delta-every`, `--mask-test`, `--raw-bench` and `--skip-drawing` to measure what
a replay format for this core costs; the results and the design they lead to are in
`replay-3ds-spec.md`. Headlines: 30–60 MB of the state changes per keyframe at any interval, a
keyframe compressed alone is as big as a full state (10–13 MB) but 1.6 MB (2 s) to 4.2 MB (30 s)
with the previous keyframe's payload as a zstd reference prefix; the linear heap cannot be left
stale (the game crashes); raw save/load are 80/230 ms (patch 0002); skip-drawing seeks run at
738 fps (patch 0003); JIT and interpreter diverge in gameplay, so the JIT is the only CPU back end
a replay may use.
