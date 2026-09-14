# Built-in Poke-A-Byte — Implementation Spec

## 0. Status: proposed (2026-09-13)

**Short answer: yes, it is possible, and the right shape is a Rust re-implementation of the
Poke-A-Byte engine and its HTTP/SignalR API inside Super Shuckie, not an embedded .NET runtime.**

What the user gets:

| Goal | How this spec meets it |
|---|---|
| No driver delay | The engine reads game memory in-process at every frame boundary. No shared-memory file, no UDP, no 5 ms polling loop, no torn reads. |
| No read/write latency (writes especially) | A write lands in emulated memory at the next frame boundary through the same path the RAM tools use, and the changed property is pushed to clients from that frame's snapshot. Today a write crosses UDP, a channel, a frame, a shared-memory copy and a poll before a client sees it. |
| No second program | Shuckie itself serves `http://localhost:8085` (REST + SignalR). Existing clients (the user's `gen2-stp-frontend` overlay, `pokeaclient`, Poke-A-Byte's own web UI) connect to it unchanged. |
| Headless, mapper visible in the status bar | No window is needed. The status bar's free area gets a `QToolButton` like the "N FROZEN" one: it shows the loaded mapper's name, click opens a native picker, hover shows details. |

What it costs: one new pure-logic crate (mapper engine + server, roughly the size of
`supershuckie-memory-tools` plus the server), a JavaScript engine dependency (QuickJS-NG through
`rquickjs`, 30 crates, prebuilt bindings for the MinGW target), a small extension of the memory
monitor in `supershuckie-core`, a frontend glue module, a C ABI header and a Qt controller. The
existing shared-memory protocol crate stays as an optional "legacy protocol server" for anyone still
running the external Poke-A-Byte.

Measured inputs used below (this Mac, Apple Silicon, release builds):

| Measurement | Value | Source |
|---|---|---|
| 4 MiB full-memory snapshot on the core thread | 0.13 ms mean, 0.43 ms max | `ram-tools-spec.md` §0 |
| Memory monitor attached but idle | 0.2 µs / frame | `ram-tools-spec.md` §0 |
| QuickJS: gen-4-style `preprocessor()` (384 PRNG steps, 6 party slots) | 99 µs / call | probe in scratch, `rquickjs 0.9.0` |
| QuickJS: `after-read-value-expression` evaluated from its string | 2.1–3.1 µs each | same probe |
| QuickJS: the same expression precompiled once to a function | 0.04 µs each | same probe |

The rest of this document is written so it can be implemented phase by phase without re-deriving
anything: §2 explains today's pipeline, §3 the alternatives, §4 the compatibility contract, §5–§11
the design, §12 licensing, §13 build, §14 performance gates, §15 tests, §16 the phase order.

---

## 1. Goal and non-goals

Goal: a user loads a ROM, picks (or has Shuckie remember) a mapper, and every program that speaks
Poke-A-Byte's API sees live, frame-accurate property values from the emulator itself, with writes
and freezes applied on the very next frame. Poke-A-Byte 0.11.4 mappers (XML + JavaScript) load
unmodified.

Non-goals:
- Supporting other emulators or RetroArch. The engine only reads Shuckie's cores.
- Replacing Poke-A-Byte's web UI with a native one. The native UI is a picker plus the status bar;
  the full property editor stays a web page (optionally bundled, §10.6).
- Bit-for-bit reproduction of Poke-A-Byte's *timing*. Its 200 Hz polling is replaced by
  frame-boundary evaluation, which is strictly more accurate.
- The overlay web-view host (`~/.claude/plans/i-am-wondering-if-tender-snowglobe.md`, Part B). That
  plan's *data feed* parts (A2, A3, A6, Part C) are superseded by this spec, see §17.

---

## 2. How it works today (read this first)

### 2.1 Shuckie's side: `supershuckie-pokeabyte-integration`
- A UDP server on `127.0.0.1:55356` (`lib.rs:14`) and a shared-memory file `EDPS_MemoryData.bin`
  (`/tmp` on macOS, `/dev/shm` on Linux, a named mapping on Windows; `src/shared_memory/*.c`).
- Poke-A-Byte sends `SETUP` with up to 128 read blocks and a frame-skip hint (`protocol.rs:95-100`).
- The core thread, in `handle_pokeabyte_integration()` (`supershuckie-core/src/thread.rs:958-1015`),
  runs every loop iteration: drains the write/freeze queue into `enqueue_write` / a `freezes` map,
  then at a frame boundary applies freezes with `write_if_changed` and copies every block into the
  shared file with `read_ram` (`thread.rs:1008-1013`). Shuckie ignores a frame skip of `-1`
  (`protocol.rs:124` turns it into `None`), so it copies every frame.
- macOS caps the mapping at 4 MiB (`shared_memory.rs:11-13`), exactly the NDS default range.

### 2.2 Poke-A-Byte's side
- `PokeAProtocolDriver.ReadBytes` memcpys each block out of the shared file
  (`PokeAProtocolClient.cs:338-347`). There is no synchronisation: the emulator may be writing the
  next frame while Poke-A-Byte copies, so a 4 MiB NDS read can be half old frame, half new.
- `PokeAByteInstance.ReadLoop` (`PokeAByteInstance.cs:170-199`): `Read()` then
  `Task.Delay(DELAY_MS_BETWEEN_READS)` (default 5 ms). `Read()` = driver read → JS `preprocessor()` →
  every property's `ProcessLoop` → JS `postprocessor()` → collect `FieldsChanged` → SignalR
  `PropertiesChanged`.
- Writes (`WriteValue`/`WriteBytes`, `:367-483`): JS hooks → bytes → `Driver.WriteBytes` → UDP
  → Shuckie's channel → `enqueue_write` on the core thread → applied between frames. The new
  bytes become visible to Poke-A-Byte only after the next shared-memory copy *and* the next poll.
- Scripts run on Jint, a .NET JavaScript interpreter. The gen 3/4/5 mappers decrypt every party
  slot in JavaScript on every tick; this, not the transport, is where a big mapper's tick time goes.

### 2.3 The latency chain, today and after

| Path | Today (Shuckie + Poke-A-Byte 0.11.4) | Built-in |
|---|---|---|
| Frame end → property values on the wire | shared-file copy (same frame) + up to 5 ms poll + Jint script + C# property loop + SignalR ≈ 6–20 ms, more for gen 4/5, and reads may be torn | snapshot copy (≤ 0.13 ms) + engine tick (0.1–0.6 ms) + WebSocket ≈ 1–2 ms, always a coherent frame |
| REST write → bytes in emulated memory | UDP + channel + wait for the frame boundary: ≤ 1 frame + a few ms | queued to the core thread, applied at the next frame boundary: ≤ 1 frame |
| REST write → client sees the change | the above + next shared copy + next poll + tick ≈ 2 frames + 5–20 ms | next snapshot after the write ≈ 1 frame + 1–2 ms |
| Freeze re-applied after the game changes the value | Poke-A-Byte notices on a later tick and re-sends a write (bit fields), or the emulator restores whole bytes each frame | every frame on the core thread, bit fields included (§6.2) |

---

## 3. Approaches considered

**A. Embed the .NET runtime in Shuckie (hostfxr / NativeAOT).** Rejected. The Windows release is a
single static MinGW executable (`-DSUPERSHUCKIE_STATIC=ON`); NativeAOT needs the MSVC linker and
`hostfxr` needs a 70+ MB runtime beside the exe. Jint and ASP.NET are reflection-heavy and trimming
them is fragile. It would also keep every latency source of §2.2 except the shared file.

**B. Launch `PokeAByte.Web` as a hidden child process.** Rejected. It removes the visible second
window but nothing else: the shared-memory driver, the polling loop and Jint stay, and the
self-contained publish is ~100 MB per platform that Shuckie would have to ship and update.

**C. Re-implement the engine and API in Rust, in-process (this spec).** The engine is small: the
whole Poke-A-Byte `Domain` is 4.5 k lines of C#, `Web` 2.1 k, and most of that is glue. The
behaviour is documented (`docs/APIs/*.md`) and pinned by 60-odd xunit tests that translate directly.
The one genuinely new dependency is a JavaScript engine; §13 shows QuickJS-NG builds for every
target Shuckie ships and costs 0.1 ms per tick for the heaviest scripts.

**Where the engine runs (sub-decision).** Evaluating on the core thread would be zero-copy, but it
puts a JavaScript interpreter, an unbounded per-mapper property loop and script bugs on the
latency-sensitive thread the RAM tools were carefully kept off (`ram-tools-spec.md` §3: "the core
thread never waits, never allocates in steady state, never computes"). Instead the core thread does
exactly what it does for Poke-A-Byte today, a bounded memcpy of the mapper's read ranges at a frame
boundary, and a dedicated engine thread does everything else (§5).

---

## 4. Compatibility contract

Everything in this section is observable by existing programs and must not change.

### 4.1 Clients that must keep working without modification
- **`gen2-stp-frontend`** (`~/Documents/gen2-stp-frontend/app/index.html:757-759`): loads
  `packages/signalR.js` and `http://localhost:8085/dist/gameHookMapperClient.js` *from the
  server*. That client (`PokeAByte/src/PokeAByte.Web/wwwroot/dist/gameHookMapperClient.js`) does
  `GET /mapper`, a SignalR hub at `/updates` (handlers `PropertiesChanged`, `MapperLoaded`,
  `GameHookError`, `DriverError`, `SendDriverRecovered`, `UiBuilderScreenSaved`), and
  `POST /mapper/set-property-value/` and `POST /mapper/set-property-bytes/` with bodies
  `{path, value, freeze}` / `{path, bytes, freeze}` where `path` has its **first** `.` replaced by
  `/` and the URL has a trailing slash. It re-fetches `/mapper` every minute.
- **`pokeaclient` 0.7.0** (Apache-2.0, `@microsoft/signalr` 10): `GET /mapper`, `/updates`,
  `POST /mapper/set-property-value`, `/mapper/set-property-bytes`, `/mapper/set-property-frozen`,
  `PUT /mapper-service/change-mapper`, `PUT /mapper-service/unload-mapper`. It keys on
  `fieldsChanged` containing `"value"`, `"bytes"`, `"frozen"`.
- **Poke-A-Byte's Preact UI** (`src/Frontend`): `BASE_URL = "http://localhost:8085"` is hardcoded
  (`src/api/fetchFunctions.ts:1`); uses the `/files/*`, `/mapper-service/*`, `/settings/*` and
  `/mapper/*` routes of §9.3 and routes itself under `/ui/*`.

### 4.2 Wire facts (verified against the sources)
- Port **8085**, HTTP only, loopback. `localhost` resolves to `::1` first on some stacks (Electron
  and Node do), so bind both `127.0.0.1:8085` and `[::1]:8085`.
- JSON is **camelCase** (`Startup.cs:35-39`, `JsonSerializerDefaults.Web`); `docs/APIs/WebSocket.md`
  shows PascalCase but is stale: the legacy client reads `mapper.meta`, `mapper.glossary`,
  `mapper.properties` and the Preact UI reads `error.title` / `error.detail`.
- CORS: any origin, echoed back, with `Access-Control-Allow-Credentials: true`
  (`Startup.cs:57-63`). The SignalR JS client sends `credentials: include`, so a `*` wildcard would
  be rejected by the browser. Preflight `OPTIONS` must succeed for `POST /updates/negotiate`.
- **SignalR** (`/updates`): JSON hub protocol, server→client only (the hub has no methods):
  `POST /updates/negotiate?negotiateVersion=1` →
  `{"connectionId","connectionToken","negotiateVersion":1,"availableTransports":[{"transport":"WebSockets","transferFormats":["Text","Binary"]}]}`;
  WebSocket upgrade at `GET /updates?id=<connectionToken>`; client handshake
  `{"protocol":"json","version":1}\x1e` answered by `{}\x1e`; then invocations
  `{"type":1,"target":"<name>","arguments":[<payload>]}\x1e`, ping `{"type":6}\x1e` at least every
  15 s (the client's server timeout is 30 s), close `{"type":7}\x1e`. Every frame ends with `0x1E`.
- Hub messages: `Hello` (to the new connection only; `MapperModel` or `null`), `MapperLoaded`
  (`MapperModel`), `PropertiesChanged` (`Property[]`, only those with a non-empty `fieldsChanged`),
  `InstanceReset` (no arguments), `Error` (`{title, detail}`).
- `MapperModel` = `{"meta":{"id","gameName","gamePlatform","version","path"},"properties":[Property…],"glossary":{"<ref>":[{"key":<number>,"value":<any>}]}}`.
- `Property` = `{"path","type","memoryContainer","address":<number|null>,"length","size","reference","bits","description","value","bytes":[<number>…],"isFrozen","isReadOnly","fieldsChanged":[…]}`.
  `type` ∈ `binaryCodedDecimal | bitArray | bool | bit | int | string | uint | byteArray`.
  `fieldsChanged` is a string array in the fixed order
  `bytes, value, address, isFrozen, length, size, bits, reference, description, memoryContainer` and
  then **`"frozen"` a second time** whenever `isFrozen` changed (`FieldChangesJsonConverter.cs:16-38`,
  pinned by `JsonConverterTests.cs:30-33`). `bytes` is a number array, never base64.
- Values: `int`/`uint` → JSON number; a value that passed through `after-read-value-expression`
  is a JS number (C# `double`) but serialises as `31`, not `31.0`; `bool`; `bool[]` for `bitArray`;
  string; `null` for an unresolved address or an unmapped glossary key. The Rust serialiser must
  emit integral `f64`s without a fractional part.
- Error responses: `{"status":400,"title":"MAPPER_NOT_LOADED","detail":"Please load a mapper file first."}`
  (`ApiHelper.cs:42-48`); unhandled errors → 500 `"Request failed due to an exception: …"`.

### 4.3 Mapper files
- Poke-A-Byte 0.11.4, mapper syntax ≤ 4 (`PokeAByteMapperXmlFactory.cs:39`). Both namespace URIs
  must be accepted for `var:` attributes: upstream `PokeAByte/mappers` declares
  `https://schemas.pokeabyte.io/attributes/var`, the user's local checkout
  (`~/Documents/mappers`, `Scotts-Thoughts/mappers`, last commit 2024-06-02) still declares
  `https://schemas.gamehook.io/attributes/var`. Poke-A-Byte 0.11.4 only matches the former
  (`PokeAByteMapperXmlFactory.cs:12-20`), so those local files do not currently substitute class
  variables in Poke-A-Byte either; the port accepts both and logs which one it saw.
- The script API (`docs/APIs/MapperScript.md`): globals `__console`, `__state`, `__variables`,
  `__mapper`, `__memory`, `__driver`; module `game_functions` exporting `pokemon.Encrypt/Decrypt`;
  hooks `preprocessor`, `postprocessor`, `containerprocessor`, `read-function`, `write-function`,
  `after-read-value-expression`, `after-read-value-function`, `before-write-value-function`.
- File locations: mappers in Poke-A-Byte's `{ApplicationData}/PokeAByte/Mappers` with
  `mapper_tree.json` (`display_name`, `path`, `version`), archives in `MapperArchives`, settings in
  `settings.json`, GitHub token in `github_api_settings.json` (`MapperService.cs:14-18`,
  `DownloadService.cs:11-14`). §10.4 says how Shuckie uses these.

---

## 5. Architecture

```
Qt GUI thread (1 ms tick)                 Engine thread "PokeAByteEngine"                Core thread (latency-sensitive)
MainWindow status button ─┐               PokeAByteInstance                              run_thread loop:
PokeAByteController       ┼─ C ABI ─► PokeAByte (frontend glue)                           handle_memory_monitor()  ◄── MemoryMonitorLocal #2
MapperPickerDialog        ┘   │           ├ mapper (parsed XML, properties, glossary)        1 traces 2 pause 3 freezes (masked)
                              │           ├ memory: static container ← bulk sample          4 edits  5 sample  6 BULK COPY of the
                              │           ├ dynamic containers (script `fill`)                  mapper's read ranges into the free
                              │           ├ QuickJS runtime (mapper .js, game_functions)        half of a double buffer, then unpark
                              │           └ change set → JSON per tick                          the engine thread
                              │                 ▲            │
                              │        commands │            │ broadcast (Arc<str>)
                              │                 │            ▼
                              └────────► Server thread(s): TcpListener 127.0.0.1:8085 + [::1]:8085
                                           ├ HTTP/1.1 (httparse): REST routes, static files, CORS
                                           └ /updates: SignalR JSON hub over WebSocket (tungstenite), one thread per client
```

Principles, inherited from the RAM tools (`ram-tools-spec.md` §3) and applied here:

1. **The core thread only copies.** Per frame it memcpys the mapper's read ranges into a
   preallocated buffer and unparks the engine thread. No allocation, no JS, no property loop.
   If the engine thread has not consumed the previous buffer, the frame is skipped, never waited
   for.
2. **Everything happens at a frame boundary in the monitor's fixed order.** The bulk copy is the
   monitor's step 6, after freezes and edits, so the snapshot shows the state the next frame starts
   from, and a write made this frame is visible in this frame's snapshot.
3. **One write path.** Engine writes and freezes go through `MemoryMonitorShared::push_edit` and
   `FreezeSpec`, so they end in `SuperShuckieCore::enqueue_write`, are recorded into a replay being
   recorded and refused during playback, exactly like tool edits (`supershuckie-core/src/lib.rs:412-424`).
4. **The GUI thread only polls counters.** Status is a generation counter plus a JSON string, read
   on the existing 100-tick throttle (`main_window.cpp:456-459`).

Threads: engine (1), server accept loop (1), one thread per HTTP connection (short-lived, bounded
to 32 concurrent) and one per WebSocket client (long-lived). All are ordinary `std::thread`s;
no async runtime.

Crates (workspace members, all `version.workspace`):

| Crate | Depends on | Contents |
|---|---|---|
| `supershuckie-pokeabyte` (new) | `serde`, `serde_json`, `roxmltree`, `rquickjs` | §7 engine + §8 script host + §9 server + §10.4 files. Pure logic: it is given byte snapshots and a write sink; it never links an emulator. Testable headlessly. |
| `supershuckie-core` | — | §6: bulk sample + masked freezes in `memory_monitor.rs`. |
| `supershuckie-frontend` | `supershuckie-pokeabyte` | §10: lifecycle, settings, per-ROM memory, thread ownership. |
| `supershuckie-frontend-c` | — | §10.5: `include/supershuckie/pokeabyte.h` + `src/pokeabyte.rs`. |
| `supershuckie-qt` | — | §11: controller, picker dialog, status button, menus. |
| `supershuckie-pokeabyte-integration` | unchanged | Kept as the optional legacy protocol server (`Settings › Poke-A-Byte › Legacy protocol server`). |

---

## 6. Core layer (`supershuckie-core/src/memory_monitor.rs`)

### 6.1 Bulk sample (new)
The monitor's existing sample path is built for a few small windows (`MAX_VIEW_WINDOWS 4`,
`MAX_VIEW_BYTES 16 KiB`, `memory_monitor.rs:25-55`) and swaps the sample out under a mutex. The
engine needs up to a few MiB per frame, so add a separate mechanism instead of raising those caps:

```rust
/// Contiguous game-address ranges copied every frame for the Poke-A-Byte engine.
pub struct BulkSpec { pub ranges: Vec<Range<u32>>, pub every_frame_max_speed: f32 }
pub struct BulkSample { pub frame: u64, pub state_epoch: u64, pub ranges: Vec<Range<u32>>, pub bytes: Vec<u8> }
```

- `MonitorRequest` gains `bulk: Option<BulkSpec>`; `clamp()` enforces `MAX_BULK_BYTES = 8 MiB` and
  ≤ 32 ranges, and drops ranges that are not inside a listed region (the engine reports those to the
  user as "unmapped range", see §7.1 step 8).
- `MemoryMonitorShared` gains a double buffer: `bulk_free: Mutex<Option<BulkSample>>` (a recycled
  buffer the core thread may fill) and `bulk_ready: Mutex<Option<BulkSample>>` (filled, waiting),
  plus `bulk_waker: Mutex<Option<Thread>>`. All accessed with `try_lock` from the core thread.
- `MemoryMonitorLocal::service` step 6, after edits and the existing sample: if `new_frame` (or
  `edited`, or `request_changed`) and the pacing allows it, `try_lock` the free buffer, copy each
  range with `copy_mapped_prefix` (`memory_monitor.rs:713`), stamp `frame`/`state_epoch`, move it to
  `bulk_ready` (dropping any unconsumed one back into `bulk_free`), and `unpark` the waker. Pacing:
  every frame while the emulation speed is ≤ `every_frame_max_speed` (default 2.0, the same
  threshold the compositor uses in `README.md` "Performance notes"); above it, at most once per
  wall-clock 1/60 s. The paced check is one `Instant` compare.
- Engine side: `take_bulk() -> Option<BulkSample>` and `recycle_bulk(BulkSample)`. The engine
  thread `park_timeout`s and is woken by the waker; a 100 ms timeout keeps the server responsive
  when emulation is paused.
- The copy is the only per-frame cost. Sizes for the mappers at hand: upstream Crystal declares
  `0x8000-0x9FFF, 0xC000-0xDFFF` = 16 KiB; upstream HGSS declares `0x2000000-0x22FFFFF` = 3 MiB;
  mappers without `<memory>` fall back to the platform defaults (GBC 88 KiB, GBA ~300 KiB, NDS
  4 MiB). 3–4 MiB is 0.1–0.13 ms, under 1 % of a frame at 1× (§14 gates it).

### 6.2 Masked freezes (new)
`FreezeSpec` gains `mask: Option<Vec<u8>>` (same length as `bytes`). `apply_freezes`
(`memory_monitor.rs:963-980`) computes `new = (cur & !mask) | (bytes & mask)` and calls
`write_if_changed` only when `new != cur`. This is how a `bits="4-7"` freeze holds four bits while
the game keeps the other four, which Poke-A-Byte can only approximate by rewriting on a later tick
(`_PokeAByteProperty.cs:264-300`). Unmasked freezes are unchanged. Test: freeze mask `0xF0` over a
byte the fake core changes to `0x0F` → written value `0x?F` with the frozen high nibble, one write.

### 6.3 What is *not* changed
`MonitoredCore`, the ordering guarantee (`memory_monitor.rs:11-13`), the event ring, edits
(`MemoryEdit { edit_id, path, data }`, `MAX_EDIT_LEN 4096`), `WriteFailure`. The engine consumes
`MonitorEvent::Written / WriteFailed / Discontinuity` exactly like `MemoryTools` does.

---

## 7. The engine (`supershuckie-pokeabyte/src/`)

Every rule here is Poke-A-Byte 0.11.4's, with a file reference to where it is pinned. Deviations
are marked **(deviation)** and are all supersets.

### 7.1 `xml.rs` — load and expand (`PokeAByteMapperXmlFactory.cs:166-238`)
1. Read the file, **delete every `{` and `}` character** from the text, then parse with
   `roxmltree`. (`address="{address} + 8"` becomes `address="address + 8"`.)
2. Macros: for each `<macro type="T">` anywhere, replace it with the *children* of the first `<T>`
   under `<macros>`; the `<macro>` element vanishes from the path. Missing macro → load error
   "Unable to find macro in <macros> tag of T".
3. Classes: for each `<class type="T">` under `<properties>`, replace its *content* with the
   children of the first `<T>` under `<classes>`; the `<class>` element stays and its `name`
   attribute is a path segment. Nested classes expand in document order (the C# loop enumerates
   lazily, so a class inserted by an earlier expansion is expanded too).
4. `var:` substitution: for every attribute in either `var` namespace (§4.3) on a descendant of
   `<properties>`, take its local name and value and do a plain substring replace in every
   attribute of every descendant of that element, skipping attributes named `name` and `type`.
   Longer variable names must be substituted before shorter ones that are prefixes of them
   **(deviation, safe)**: Poke-A-Byte applies them in document order, which breaks
   `var:address` vs `var:addressHigh`; sorting by length descending only changes results for
   mappers that were already broken.
5. Hex normalisation: in attributes named `address` or `preprocessor` containing `0x`, replace
   each `0x[0-9a-fA-F]+` with its decimal value (`Extensions.cs:43-61`).
6. Metadata: `id` (GUID), `name`, `platform`, `syntax` (default 1), `version`; `syntax > 4` →
   "The mapper … is too new."
7. Platform table (§7.6); unknown platform → "Mapper specifies an unknown game platform X."
8. `<memory><read start end/>` (hex with or without `0x`, `end` inclusive). When present these are
   the read ranges, each of `end - start + 1` bytes (`PokeAByteInstance.cs:66`); when absent, the
   platform's default blocks. **(deviation)** Poke-A-Byte transfers `end - start` bytes for the
   platform defaults (`StaticMemoryContainer.cs:22-30`), so the last byte of each default block is
   never refreshed; we copy the whole block. Ranges outside Shuckie's regions are reported once as a
   warning and skipped.
9. Properties (`:89-134`): `name`, `type` (`binaryCodedDecimal | bitArray | bool | bit | int |
   string | uint | byteArray`; `bit` is stored as `bool`; anything else → "Unable to parse
   <path>. … Unknown property type"), `memoryContainer`, `address`, `length` (default **1**),
   `size`, `bits`, `reference`, `description`, `value` (static), `read-function`,
   `write-function`, `after-read-value-expression`, `after-read-value-function`,
   `before-write-value-function`. Unknown attributes are ignored (the deprecated GameHook
   `position=`, `bit=`, `preprocessor=` attributes therefore load but do nothing, as in
   Poke-A-Byte).
10. Path = element names from `<properties>` down, `class` elements contribute their `name`,
    joined with `.`, plus `.` + property `name` (`MapperXmlExtensions.cs:35-53`):
    `<player><team><class name="0" type="party_pokemon"><property name="level">` → `player.team.0.level`.
    Duplicate paths: last one wins (dictionary insert), keep document order for iteration.
11. `<references>`: element name = glossary name; `type` `string` (default) or `number`; entries
    `key` (`0x` → hex, else decimal, `u64`), `value` (absent/empty → `null`, the string
    terminator; `number` → integer). Duplicate keys are allowed at load and fail at lookup with
    "duplicate keys" (`Common.cs:68-81`).

Output: `Mapper { meta, platform, read_ranges, properties: Vec<Property> (document order) +
HashMap<path, index>, references: HashMap<name, Reference> }`.

### 7.2 `addr.rs` — address expressions (`AddressMath.cs:7-54`, NCalc)
Poke-A-Byte evaluates the (brace-stripped, hex-normalised) `address` string with NCalc against the
script's `__variables`. The mappers use only integer arithmetic (`dma_a + 3958`, `address + 20`,
`32 + 40`). Implement a small evaluator: integers, identifiers, `+ - * /` (integer division),
`%`, parentheses, unary minus. Anything else is a load error naming the property, which is a
**(deviation)** from NCalc's larger grammar; grep of all 60 known mappers finds nothing else.
Rules:
- An address whose text contains only digits, spaces, `+`, `-` is constant and resolved at load
  (`_PokeAByteProperty.cs:11`).
- Otherwise it is resolved lazily (§7.5 step 4). If **any** variable in `__variables` is `null`,
  the address is unresolved (`AddressMath.cs:14-19` aborts on the first null, whichever variable).
  An undefined identifier → unresolved, no error. A non-numeric result → unresolved.
- A `double` result truncates to `u32`.

### 7.3 `property.rs` — decode and encode
Endianness: Poke-A-Byte's `EndianTypes` names are inverted relative to usual meaning
(`PropertyLogic.cs:59-90`): platforms marked `LittleEndian` (GB, GBC, SNES) **reverse** the bytes
before reading, i.e. values are stored most-significant-byte first; platforms marked `BigEndian`
(NES, GBA, NDS, PSX) read bytes as a little-endian host integer. Pinned by
`ReadIntegerTests.cs:39-51` (GB `[1,0]` → 256) and `:91-103` (NDS `[255,255]` → 65535). Name the
Rust enum by what it does (`StoredMsbFirst` / `StoredLsbFirst`) and map the platform table to it.

Decode (`ToValue`, `_PokeAByteProperty.cs:99-121`, `PropertyLogic.cs`):

| type | rule |
|---|---|
| `binaryCodedDecimal` | `r = r*100 + 10*(b>>4) + (b&0xF)` over the bytes in order, endian ignored |
| `bitArray` | one bool per bit, LSB of byte 0 first |
| `bool` / `bit` | `bytes[0] != 0` |
| `int` | length 1 → `bytes[0]` **unsigned** (`Known_Issues.md`); else copy into 4 bytes, reverse the first `len` if stored MSB-first, read i32 LE |
| `uint` | length 1 → byte; ≥ 4 → u32 LE of the (maybe reversed) bytes; 2–3 → reversed-if-needed bytes left-aligned into 4 |
| `string` | `size > 1`: chunks of `size`, each reversed if `StoredLsbFirst`, folded big-endian into a u64 key, looked up; stop at the first missing or `null` entry. `size ≤ 1`: byte per char, same stop rule. Reference = `reference` attribute or `defaultCharacterMap`; missing glossary → error "ReferenceObject is NULL." |
| `byteArray` | the bytes |

`bits` (`PropertyLogic.cs:7-36`, `BitsTests.cs`): `a-b` inclusive range, `a,b,c` list (no spaces),
or a single index; bit indices are `BitArray` indices (0 = LSB of byte 0, 8 = LSB of byte 1); any
bad part → load error "Invalid format for attribute Bits (…)". Applied by `BytesFromBits`
(`_PokeAByteProperty.cs:317-331`): gather the selected bits into a new buffer starting at bit 0,
then decode that buffer. `bits="4-7"` on `0xF0` → 15.

Value pipeline (`CalculateObjectValue`, `:333-357`): decoded value → `after-read-value-expression`
(§8.4) → `after-read-value-function` (result replaces the value) → glossary lookup when
`reference` is set and the type is `bool | bit | int | uint` (key = value as u64; missing → `null`).
Strings use the glossary for characters instead; `byteArray`, `bitArray`, `binaryCodedDecimal`
never use it.

Encode (`BytesFromValue`, `:123-190`):

| type | rule |
|---|---|
| `byteArray` | literal `"[1, 2, 3]"`, split on `,`, parse bytes; else error "Invalid value format for byte array property. Expected '[255, 255, ...]'" |
| `binaryCodedDecimal` | digits of the decimal text packed two per byte, MSB first, then `TakeLast(length)`; `1337` → `[0x13, 0x37]` |
| `bool` / `bit` | `true`/`false` (C# `bool.Parse`, case-insensitive) → `[1]`/`[0]` |
| `int` | i32 LE bytes, take `length`, reverse if stored MSB-first |
| `uint` | parse as i32 (overflow → 0, quirk kept), same as int |
| `string` | `char_size = size or 1`; at most `length / char_size - 1` characters; each char → glossary key by value (missing → error "Missing dictionary value for 'c'…"), emitted as the low `char_size` bytes of the u64 key **host little-endian, no swap**; then the terminator (first entry whose value is `null`) |

`isReadOnly` ⇔ no address string; `isFrozen` ⇔ `bytes_frozen` non-empty.

### 7.4 `memory.rs` — containers (`MemoryManager.cs`, `StaticMemoryContainer.cs`, `DynamicMemoryContainer.cs`)
- Default namespace: the bulk sample's ranges. A read must lie entirely within one range
  (`StaticMemoryContainer.cs:35-45`); otherwise "Invalid memory read: … is outside of available
  memory regions. Check mapper." The bounds check is skipped when a property's address did not
  change since the last check (the 0.9.1 optimisation).
- Named namespaces (`memoryContainer` other than `default`): created on first `fill`; a list of
  fragments keyed by start address; `fill` writes into every fragment containing the address or
  appends one; `get_all_bytes` returns the first fragment (Poke-A-Byte's TODO, kept); a dirty flag
  drives `containerprocessor`.

### 7.5 `instance.rs` — the tick (`PokeAByteInstance.cs:201-275`) and writes
State: mapper, memory, script host, `variables` / `state` (JS objects), per-property runtime
(`bytes`, `full_value`, `value`, `address`, `address_solved`, `bytes_frozen`, `fields_changed`).

On `load(mapper, script)`: run two ticks over the first snapshot (`StartProcessing` reads twice so
the first tick primes `bytes` and the second yields real change flags, `:134-167`); a script error
during those aborts the load with "Error in mapper script: …"; then broadcast `MapperLoaded`.

Per snapshot (`Read()`):
1. Dirty named containers (only if the script exports `containerprocessor`): call
   `containerprocessor(name, Uint8Array)` for each, clear the flag.
2. `preprocessor()` if exported; a return value of exactly `false` ends the tick with no
   processing and no notification.
3. Read `__variables.reload_addresses` once (truthy → `reload`). The engine never clears it.
4. For each property in document order, `ProcessLoop` (`_PokeAByteProperty.cs:194-315`):
   a. `type == string && length == 1 && value != null` → `length = len(value)` (legacy).
   b. `read-function` → if it returns `false`, skip the property.
   c. static `value` attribute → `value` = that **string** (`StaticValueTest.cs:26`), done.
   d. if `reload && has_parameters` → unsolved; if unsolved → try to solve (§7.2); on success set
      `address` (flag `address` when it differs).
   e. no address → done (the postprocessor may set the value).
   f. read `length` bytes at `address` from `memoryContainer`; out of range → error event
      `{title:"Error", detail: message}` for this property, continue with the next.
   g. unchanged bytes → done.
   h. frozen: if `bytes_frozen != bytes`: `bytes = bytes_frozen`, recompute value/full value; then
      for the default container re-assert the freeze (already done on the core thread by §6.2, so
      the engine only refreshes its own view); for a named container `fill` + dirty. Done.
   i. `bytes = new`, `full_value = decode(bytes)`, `value = pipeline(bits(bytes))`; setters flag
      `bytes` / `value` only when different (`SequenceEqual` / `Equals`).
   Errors in one property never stop the loop (`:223-231`).
5. `postprocessor()` if exported; `false` → no notification, flags stay set for the next tick.
6. Collect properties with non-empty `fields_changed`, serialise once to a `PropertiesChanged`
   invocation, broadcast, clear the flags.

Writes (`WriteValue`, `:367-428`; `WriteBytes`, `:431-483`), executed on the engine thread from
server commands:
1. Read-only → ignore silently. Empty `bytes` → error "Bytes array is empty."
2. Reference properties: an integer value is looked up by key, anything else by value
   (`GetFirstByValue`, error if unknown); bytes = the key as u64 LE (truncated by the overlay).
   Otherwise `encode(value.to_string())`; `null` → error "Can not write null to property '…'".
3. `bits`: merge the new bits into the last known `bytes` at the bit indices.
4. `before-write-value-function` → `false` aborts.
5. Overlay onto a `length`-byte buffer: new bytes first, remainder from the last known `bytes`.
6. `write-function` → `false` aborts.
7. Named container: `fill` + run `containerprocessor` synchronously; **no** emulator write (the
   script is expected to re-encrypt and call `__driver.WriteBytes`). Done.
8. `freeze == true` → `bytes_frozen = bytes` and register a `FreezeSpec` (mask from `bits`);
   `freeze == false` → clear both; `freeze == null` while frozen → update `bytes_frozen` and the
   `FreezeSpec` bytes (`WriteUintTests.WriteToFrozenUpdatesBytes`).
9. `push_edit(MemoryEdit { path: direct(address), data: bytes })`. The REST call returns 200 as
   soon as the edit is queued, as Poke-A-Byte does; a `WriteFailed` event (playback, read-only
   region, unmapped) becomes an `Error` hub message and a log line.

`WriteMultiple` (`set-properties-by-bits`, `:321-364`): first property is the baseline (address,
type, length); seed the bit buffer from `encode(first.full_value)`; for each update check path,
same address/type/length, non-empty value, then merge its bits; one `WriteBytes` with
`freeze=false`. Results map to the four 400 messages of §9.3.

Unload: stop the tick, drop the script runtime, clear freezes it registered, broadcast
`InstanceReset`.

Discontinuities (`state_epoch` change: reset, state load, replay seek): no special message,
Poke-A-Byte has none; the next tick simply diffs everything.

### 7.6 `platform.rs` (`PokeAByte.Domain/Platforms/*.cs`)

| platform | stored | default read blocks |
|---|---|---|
| `GB` | MSB-first | `0000-00FF`, `8000-9FFF`, `A000-AFFF`, `B000-BFFF`, `C000-CFFF`, `D000-DFFF`, `FF00-FF7F`, `FF80-FFFE` |
| `GBC` | MSB-first | GB's plus WRAM banks 2, 3, 6, 7 at `10000-10FFF`, `11000-11FFF`, `14000-14FFF`, `15000-15FFF` (banks 4/5 absent) |
| `GBA` | LSB-first | `0-3FFF`, EWRAM `2020000-203FFFF` (16 × 8 KiB), IWRAM `3000000-3007FFF` (4 × 8 KiB) |
| `NDS` | LSB-first | `2000000-2400000` |
| `NES`, `SNES`, `PSX` | see the C# files | accepted at load; no Shuckie core, so a mapper for them never gets a snapshot (status: "no compatible core") |

Shuckie's regions use the same addresses (`docs/ram_tools.md`): GBC `WRAMX` at `0x10000`, the
synthetic `CART` at `0x20000` for save RAM, `IO` read-only. GB/GBC default blocks
`0000-00FF` (ROM header) and `A000-BFFF` (cartridge RAM at its CPU address) are not regions in
Shuckie: they are dropped with a one-time warning; `0x20000+` reaches save RAM instead.

### 7.7 `models.rs` — JSON
`serde` structs matching §4.2 exactly. `fields_changed` serialises through a custom serializer
producing the fixed order plus the trailing `"frozen"`. Numbers: a `Value::Float(f)` with
`f.fract() == 0.0` and `|f| < 2^53` serialises as an integer. `address` serialises as the `u32` or
`null`.

---

## 8. Script host (`supershuckie-pokeabyte/src/script/`)

### 8.1 Engine choice
QuickJS-NG through **`rquickjs 0.9`** (MIT; features `loader`, `classes`, `macro`). Verified in a
scratch project on this machine (cargo 1.96, workspace MSRV 1.89, rquickjs MSRV 1.81): compiles in
13 s, 30-crate closure, ships pregenerated bindings for `x86_64-pc-windows-gnu` (no bindgen/libclang
at build time), QuickJS-NG C sources compiled by `cc` like the existing shims. Measured costs are in
§0. Boa (pure Rust) was rejected for its 133-crate closure and interpreter speed.

Runtime settings: `Runtime::set_memory_limit(64 MiB)`, `set_max_stack_size`, and an interrupt
handler that aborts a call after 50 ms of wall-clock time and unloads the mapper with an `Error`
("Mapper script exceeded its time budget in <hook>"). This is the guarantee that a broken script can
never stall the emulator: the engine thread is the only one affected.

Jint options mirrored: strict mode; `eval`/`new Function` disabled (`StringCompilationAllowed =
false`); module root = the mapper's directory, relative imports resolved inside it only.

### 8.2 Globals (`PokeAByteInstance.cs:113-121`)
Jint exposes .NET members in both PascalCase and camelCase; the mappers use both (`__memory.fill`
and `__memory.Fill`, `__driver.WriteBytes`). Every method below is registered under both spellings.

| global | shape |
|---|---|
| `__console` | `log, trace, debug, info, warn, error(msg)` → Shuckie's log plus a 256-line ring buffer shown in the picker dialog |
| `__state` | a plain JS object, script-private |
| `__variables` | a plain JS object; read by the engine when solving addresses (§7.2): enumerate own properties, `null` → unsolved; `reload_addresses` reserved |
| `__mapper` | `metadata`, `memory {readRanges}`, `properties` (object keyed by path → property object), `references`, `platformOptions`, `get_property(path)`, `get_property_value(path)` (throws if missing), `set_property_value(path, value)` (throws if missing), `copy_properties(src, dst)` (copies memoryContainer, address, length, size, bits, reference, bytes, value for matching suffixes) |
| property object | class `PropertyRef { index }` with getters `path, type, memoryContainer, address, length, size, reference, bits, description, value, bytes (Uint8Array), isFrozen, isReadOnly` and setters for `value` (→ set value, flag), `address` (→ set address, mark solved, becomes writable), `bytes`, `length`, `size`, `bits`, `reference`, `description`, `memoryContainer`; also PascalCase aliases |
| `__memory` | `namespaces`, `defaultNamespace`, `fill(area, address, bytes)` (creates the namespace), `get(area, address, len)`, `get_raw_bytes(area?, address, len)` (Uint8Array), `getAllBytes(area)` |
| namespace object | `fragments`, `contains(a)`, `fill(a, bytes)`, `get_byte(a)`, `get_bytes(a, len)` → byte-array object, `get_raw_bytes(a, len)` → Uint8Array, `get_uint16_le/be`, `get_uint32_le/be`, `get_uint64_le/be`, `getAllBytes()` |
| byte-array object | `length`, `get_byte(i)`, `slice(start, len)`, `chunk(n)`, `get_uint16_le/be(i)`, `get_uint32_le/be(i)`, `get_uint64_le/be(i)` (`IByteArray.cs:73-113`), `toString` |
| `__driver` | `properName: "Super Shuckie"`, `delayMsBetweenReads: 0`, `WriteBytes(address, bytes)` → queues a `MemoryEdit`, returns `undefined` (Jint returned a Promise no mapper awaits) |

`byte[]` crossing into JS is a `Uint8Array` (`JintUnit8ArrayConverter.cs`); arrays or typed arrays
are accepted coming back.

### 8.3 Module `game_functions`
`import { pokemon } from "game_functions"` gives `pokemon.Encrypt(gen, bytes)` /
`pokemon.Decrypt(gen, bytes)` (`PokemonFunctions.cs`): gens 1–2 pass through; gen 3 = XOR of the
u32s in `[32, 80)` with `PID ^ OTID` and a 4 × 12-byte block shuffle by `PID % 24`; gens 4–5 =
LCRNG stream (`seed = 0x41C64E6D*seed + 0x6073`, XOR each u16 with `seed >> 16`) keyed by the
checksum over `[8, 136)` and by the PID over the party tail, 4 × 32-byte blocks shuffled by
`(PID >> 13) & 31`; gens 6–7 = same with 56-byte blocks and the PID for both. Implement in Rust
from the algorithm (it is PKHeX's, GPL-3.0-or-later, and the block-order tables are public); unit
test with a known party structure. Only exposed when `syntax >= 4`, as documented.

### 8.4 Hooks and expressions
- `preprocessor`, `postprocessor`, `containerprocessor(name, Uint8Array)`: looked up once at load
  on the module namespace; called through the engine's single `Context` (QuickJS is
  single-threaded; every call happens on the engine thread).
- `read-function`, `write-function`, `after-read-value-function`, `before-write-value-function`:
  exported function names; called with the property object; `false` has the meanings of §7.5.
- `after-read-value-expression`: Poke-A-Byte does `SetValue("x", v).Evaluate(expr)`
  (`PokeAByteInstance.cs:302-305`) in the global scope, where the module's exports are **not**
  visible in Jint's module mode; but the user's mappers use `decryptItemQuantity(x)` and
  `getBits(x, 0, 3)`, which the mappers themselves define as **exported** functions. To make both
  work: compile each distinct expression once at load into a function
  `(x) => (<expr>)` inside a scope where the module's exports are bound as globals (copy the
  namespace object's own properties onto `globalThis` after import). 0.04 µs per call instead of
  2–3 µs per string evaluation; 318 expression properties in Crystal cost 13 µs per tick.
  Compile errors are load errors naming the property.
- Script exceptions during a tick: log, send `Error {title:"Error", detail}`, and unload the
  mapper (Poke-A-Byte's `ReadLoop` also stops on an exception). During load: reject the load.

---

## 9. The server (`supershuckie-pokeabyte/src/server/`)

### 9.1 Transport
- One `TcpListener` per loopback family on port 8085 (setting `pokeabyte.port`, default 8085;
  `[::1]` failure is ignored, `127.0.0.1` failure is fatal and shown in the status bar: "Port 8085
  is in use, is Poke-A-Byte still running?").
- Accept loop thread; each connection gets a thread from a bounded pool (32); HTTP/1.1 parsed with
  `httparse` (1 crate), `Connection: close` after each response (the clients are fine with that;
  the SignalR negotiate is a single POST). Bodies up to 1 MiB.
- Responses set `Access-Control-Allow-Origin: <Origin or *>`, `Access-Control-Allow-Credentials:
  true`, `Access-Control-Allow-Headers: <requested or *>`, `Access-Control-Allow-Methods:
  GET, POST, PUT, DELETE, OPTIONS`; `OPTIONS` → 204 with those headers.
- `/updates` upgrade: `tungstenite 0.29` (`default-features = false, features = ["handshake"]`),
  server role, one thread per client, `set_read_timeout(5 ms)` on the `TcpStream` so the thread can
  both drain a bounded outgoing queue (`sync_channel(256)` of `Arc<str>`) and read client frames
  (handshake, pings, close). A client whose queue overflows is dropped (a stuck browser tab must not
  hold memory). Text frames only.

### 9.2 SignalR hub (`/updates`)
State per client: `negotiated token → connection`, handshake done flag. Sequence: negotiate →
upgrade → wait for the handshake frame → reply `{}\x1e` → send `Hello` with the current
`MapperModel` or `null` → subscribe to the broadcast. Server ping every 10 s. Client pings and
`Close` are consumed; a client `Close` or read error ends the thread. Broadcasts:
`MapperLoaded`, `PropertiesChanged`, `InstanceReset`, `Error` (§4.2). The serialised invocation
string is built once per event and shared (`Arc<str>`) across clients.

### 9.3 REST routes (all JSON unless stated; `Content-Type: application/json`)
From `MapperEndpoints.cs`, `MapperFileEndpoints.cs`, `FilesEndpoints.cs`, `GithubEndpoints.cs`,
`SettingsEndpoints.cs`, `DriverEndpoints.cs`. Trailing slashes are accepted everywhere; paths in
routes may use `/` or `%2F` for `.` (`ApiHelper.cs:31-34`).

| Method | Path | Body | Response |
|---|---|---|---|
| GET | `/mapper` | – | `MapperModel`; 400 `MAPPER_NOT_LOADED` when none |
| GET | `/mapper/meta`, `/mapper/properties`, `/mapper/properties/{path}`, `/mapper/glossary`, `/mapper/glossary/{key}` | – | the respective part; 404 for a missing property/glossary |
| GET | `/mapper/values/{path}` | – | `text/plain` value; 400 if not string/int |
| POST | `/mapper/set-property-value` | `{path, value, freeze?}` | 200 (§7.5 writes); 404 unknown path; 400 read-only |
| POST | `/mapper/set-property-bytes` | `{path, bytes:[…], freeze?}` | 200 |
| POST | `/mapper/set-property-frozen` | `{path, freeze}` | 200 / 400 "Property is read only." / 404 |
| POST | `/mapper/set-properties-by-bits` | `[{path, value, freeze}]` | 200 or 400 with one of: "One or more properties do not exist", "Values cannot be null.", "Address or length for property is null.", "Addresses or types for the properties are not the same." |
| GET | `/driver/name` | – | `"Super Shuckie"` or `null` when no mapper |
| GET | `/mapper-service/get-mappers` | – | installed mapper list (same shape as `/files/mappers`) |
| GET | `/mapper-service/is-connected` | – | `true` iff a mapper is loaded and a core is attached |
| PUT | `/mapper-service/change-mapper` | JSON string: mapper path | 200 or 400 with the load error text |
| PUT | `/mapper-service/unload-mapper` | – | 200 |
| GET | `/mapper-service/get-metadata`, `/get-get-properties` (sic), `/get-glossary?glossaryKey=` | – | as `/mapper/*` |
| PUT | `/mapper-service/write-property` | `{path, value, isFrozen}` | 200 |
| GET | `/files/mappers` | – | `[{display_name, path, version, type: "Official"|"Local"}]` |
| GET | `/files/mapper/check_for_updates`, `/get_updates`; POST `/download_updates` | `string[]` | §10.4 (phase 9); until then `false` / `[]` / 501 |
| GET | `/files/mapper/get_archived`; POST `/archive_mappers`, `/backup_mappers`, `/delete_mappers`, `/restore_mappers` | `string[]` or string | §10.4 |
| GET | `/files/open_mapper_folder`, `/files/open_mapper_archive_folder` | – | opens the folder with the platform opener (`explorer` / `open` / `xdg-open`) |
| POST/GET | `/files/save_github_settings`, `/get_github_settings`, `/test_github_settings`, `/get_github_link` | `{token, owner, repo, dir}` | stored in `github_api_settings.json` |
| GET/POST | `/settings/appsettings`, `/settings/save_appsettings`, `/settings/appsettings/reset` | `{RETROARCH_LISTEN_IP_ADDRESS, RETROARCH_LISTEN_PORT, RETROARCH_READ_PACKET_TIMEOUT_MS, DELAY_MS_BETWEEN_READS, PROTOCOL_FRAMESKIP}` | accepted and persisted for the UI's sake; none affect the engine (documented) |
| GET | `/dist/gameHookMapperClient.js`, `/favicon.png` | – | embedded, verbatim from Poke-A-Byte (`include_bytes!`) |
| GET | `/ui/*`, `/assets/*`, `/index.html` | – | the bundled Preact UI when built in (§10.6), else a one-page HTML that says the UI is not bundled and links the docs |
| GET | `/openapi/v1.json` | – | Poke-A-Byte's published `PokeAByte_OpenAPI.json`, embedded |

Unknown route → 404 JSON. Every handler that touches the engine sends a command over a channel and
waits for the reply with a 5 s timeout (500 on timeout); the property/mapper JSON for `GET`s is
served from a cache the engine refreshes after each tick, so `GET /mapper` never blocks a tick.

---

## 10. Frontend crate (`supershuckie-frontend/src/pokeabyte.rs`)

Modelled on `memory_tools.rs` (`MemoryTools`, `core_switched`, `tick`).

### 10.1 Ownership and lifecycle
`PokeAByte` (GUI-thread facade) owns: the engine thread handle and its command sender, the server
(started when `settings.pokeabyte.enabled`), the second `Arc<MemoryMonitorShared>`, a status
snapshot (`Arc<Mutex<Status>>` + generation `AtomicU64`), and the installed-mapper index.

- `new(data_dir, settings)`: create `<user_dir>/mappers/`, index mappers, start the engine thread,
  start the server if enabled.
- `core_switched(core, rom)` at the end of `assign_core` (`lib.rs:857`, after
  `memory_tools.core_switched`): build `GameIdentity { console_type, rom_checksum }`, read the
  per-ROM record (§10.3), attach the monitor (`core.set_pokeabyte_monitor(Some(shared))`, a new
  `ThreadCommand` mirroring `SetMemoryMonitor`, `thread.rs:682`), and send `Game { regions, console,
  remembered_mapper }` to the engine. The engine loads the remembered mapper if its platform matches
  the console (`GB`/`GBC` both match the Game Boy core; `GBA`; `NDS`), otherwise leaves the
  previous mapper loaded only if it matches, else unloads with `InstanceReset`. On `unload_rom`
  (`lib.rs:938`, `NullEmulatorCore`): detach and unload.
- `tick()`: drain status/events (`Arc<Mutex>` `try_lock`, one per GUI tick). Nothing else runs on
  the GUI thread.
- `set_enabled(bool)`: start/stop the server and engine; `set_legacy_protocol_enabled(bool)`
  forwards to the existing `set_pokeabyte_enabled` (`lib.rs:2206`), renamed internally.

### 10.2 Settings (`settings.rs`)
```json
"pokeabyte": { "enabled": true, "legacy_protocol_enabled": false, "port": 8085,
               "remember_mapper_per_rom": true, "every_frame_max_speed": 2.0 }
```
`#[serde(default)]` so existing `settings.json` files upgrade in place (today's `pokeabyte.enabled`
meant the legacy server; on first run with the new schema, if the old key was `true` set
`legacy_protocol_enabled = true` so nobody loses a working setup, and `enabled = true` regardless).

### 10.3 Per-ROM memory
`<ROM>-data/pokeabyte.json` = `{ "mapper": "STANDARD/gen2/pokemon_crystal.xml", "source": "shuckie" | "pokeabyte" }`
next to `ram-watch.json` (`lib.rs:856`). Written on every successful load with "remember" on,
deleted on an explicit unload with "forget".

### 10.4 Mapper files (`files.rs`)
Search roots, in order: `<user_dir>/mappers/` (Shuckie's own; "Open mappers folder" opens it) and
Poke-A-Byte's `{ApplicationData}/PokeAByte/Mappers` (`%AppData%` on Windows, `$XDG_CONFIG_HOME` or
`~/.config` elsewhere; the docs' `~/Library/Application Support/PokeAByte` is also probed on macOS).
Index = every `*.xml` whose root is `<mapper>`; `display_name` = file name; `version` from the
attribute; `type = "Official"` if `mapper_tree.json` in that root lists the path, else `"Local"`;
`path` is the root-relative path, resolved back by searching the roots in order. Platform is read
from the `<mapper platform>` attribute for gating in the picker.

GitHub download / archive / backup (`DownloadService.cs`: `GET
https://api.github.com/repos/{owner}/{repo}/commits/main` for the SHA, files from
`https://cdn.jsdelivr.net/gh/{owner}/{repo}@{sha}/{path}`, index `mapper_tree.json`, 1-hour cache
in `last_fetch.json`) is **phase 9**: Shuckie has no HTTPS client and adding one (`ureq` + rustls,
~60 crates, needs a MinGW check) is a separate decision; until then the routes answer honestly
(`check_for_updates` → `false`, `get_updates` → `[]`, `download_updates` → 501 with "Download
mappers with Poke-A-Byte or `git clone https://github.com/PokeAByte/mappers` into <folder>"). The
user's mappers are a git checkout already.

### 10.5 C ABI (`supershuckie-frontend-c/include/supershuckie/pokeabyte.h`, `src/pokeabyte.rs`)
Following `memory.h` conventions (error buffers, "bytes needed incl. NUL" string getters, JSON for
structured data, generation counters):

```c
bool     supershuckie_frontend_pokeabyte_is_enabled(const SuperShuckieFrontendRaw *);
bool     supershuckie_frontend_pokeabyte_set_enabled(SuperShuckieFrontendRaw *, bool, char *err, size_t);
bool     supershuckie_frontend_pokeabyte_is_legacy_protocol_enabled(const SuperShuckieFrontendRaw *);
bool     supershuckie_frontend_pokeabyte_set_legacy_protocol_enabled(SuperShuckieFrontendRaw *, bool, char *err, size_t);
uint16_t supershuckie_frontend_pokeabyte_port(const SuperShuckieFrontendRaw *);           // 0 while the server is down
uint64_t supershuckie_frontend_pokeabyte_status_generation(const SuperShuckieFrontendRaw *);
size_t   supershuckie_frontend_pokeabyte_status_json(const SuperShuckieFrontendRaw *, char *, size_t);
         // {"server":"up"|"down"|"error","error":"…","mapper":{"path","gameName","gamePlatform","version","id","propertyCount"}|null,
         //  "compatible":bool,"clients":n,"tickMicros":n,"copyBytes":n,"lastError":"…","scriptLogGeneration":n}
size_t   supershuckie_frontend_pokeabyte_mappers_json(const SuperShuckieFrontendRaw *, char *, size_t);
         // [{"path","displayName","platform","version","type","source","compatible"}]
bool     supershuckie_frontend_pokeabyte_rescan_mappers(SuperShuckieFrontendRaw *);
bool     supershuckie_frontend_pokeabyte_load_mapper(SuperShuckieFrontendRaw *, const char *path, bool remember, char *err, size_t);
bool     supershuckie_frontend_pokeabyte_unload_mapper(SuperShuckieFrontendRaw *, bool forget, char *err, size_t);
size_t   supershuckie_frontend_pokeabyte_mappers_directory(const SuperShuckieFrontendRaw *, char *, size_t);
size_t   supershuckie_frontend_pokeabyte_web_ui_url(const SuperShuckieFrontendRaw *, char *, size_t);   // http://localhost:8085/ui/mappers
size_t   supershuckie_frontend_pokeabyte_drain_script_log(SuperShuckieFrontendRaw *, char *, size_t);  // newline-separated, take-once
```
Included from `supershuckie.h`; `pub mod pokeabyte;` in `supershuckie-frontend-c/src/lib.rs`.
`load_mapper` returns after the engine reports success or failure (bounded 5 s), so the dialog can
show the load error text.

### 10.6 Bundling Poke-A-Byte's web UI (optional, phase 9)
The Preact build is plain static output. Add `scripts/update-pokeabyte-ui.sh` that runs
`npm ci && npm run build` in a Poke-A-Byte checkout and copies `src/Frontend/dist` into
`supershuckie-pokeabyte/ui/` together with `LICENSE.txt`; a `build.rs` embeds the folder (no extra
crate: walk the directory and emit a `static FILES: &[(&str, &[u8])]`). Cargo feature
`pokeabyte-ui`, default on when the folder is present. Shuckie's build never needs Node.

---

## 11. Qt (`supershuckie-qt/src/`)

- `pokeabyte_controller.{hpp,cpp}` (`QObject`, modelled on `MemoryToolsController`): owns the
  picker dialog, reads `status_json` on the 100-tick throttle when the generation changed, emits
  `status_changed()`, forwards menu actions. Settings keys through the custom map:
  `qt__pokeabyte_picker_geometry`.
- **Status bar**: a `QToolButton` created right after `frozen_state` (`main_window.cpp:149-154`),
  `setAutoRaise(true)`, text = mapper `gameName` (elided at 32 chars) or `No mapper` while a game is
  loaded and the server is up; hidden while no ROM is loaded; red text when the server failed
  (tooltip carries the error). Tooltip: mapper file, version, id, property count, connected
  clients, last tick time, last error. Click → the picker. Updated in `update_memory_status()`'s
  sibling `update_pokeabyte_status()` from the same throttle (`:456-459`).
- **Picker dialog** `mapper_picker_dialog.{hpp,cpp}`: a list of installed mappers (name, platform,
  version, source folder), filtered to the loaded console by default with a "show all" toggle,
  buttons *Load*, *Unload*, *Rescan*, *Open mappers folder*, *Open web UI*; a "Remember for this
  ROM" checkbox (default from settings); a collapsible *Script log* pane fed by
  `drain_script_log`. Loading disables the UI until the C ABI call returns and shows the error text
  inline on failure.
- **Menus**: `Settings › Poke-A-Byte ›` `Built-in server (port 8085)` [checkable, default on],
  `Legacy protocol server (for external Poke-A-Byte)` [checkable, default off, replaces today's
  "Enable Poke-A-Byte integration" item at `main_window.cpp:970-972`], `Open web UI` (opens
  `web_ui_url` with `QDesktopServices::openUrl`), `Open mappers folder`. `Tools › Mapper…`
  (`Ctrl+Alt+M`) opens the picker; the status button does the same.
- Startup: like the existing `is_pokeabyte_enabled` check at `main_window.cpp:210-216`, show the
  server error dialog once if the built-in server failed to bind.

---

## 12. Licensing and attribution

- Poke-A-Byte (backend and Preact UI) is **AGPL-3.0-only**; `pokeaclient` is Apache-2.0;
  `PokeCrypto.cs` is PKHeX code under GPL-3.0-or-later. Super Shuckie is GPL-3.0-only.
- GPLv3 §13 allows combining a GPLv3 work with an AGPLv3 work; the AGPL part stays AGPL and its
  network clause applies to the combination. So the repo *may* contain AGPL code, but the cleaner
  path, and the one this spec assumes, is that `supershuckie-pokeabyte` is a **new implementation
  from the documented behaviour and the test suite** (this document, `docs/APIs/*.md`, the xunit
  expectations rewritten as Rust tests), licensed GPL-3.0-only like the rest of the workspace, and
  not a translation of the C# files. Reviewers should be able to trace every rule to a doc or a
  test, not to a copied function body.
- Two files are shipped verbatim and stay AGPL: `gameHookMapperClient.js` (the user's overlay
  loads it by URL, so it must be byte-identical) and, if bundled, the Preact `dist`. Put their
  license text under `licenses/notices/pokeabyte/` and add a section for them in
  `scripts/make-attributions.py` next to the vendored-code sections.
- New crates without a license file in their published package need `licenses/crates/<name>/`
  overrides or the Windows build fails (`make-attributions.py`, `licenses/README.md`). Verified
  with `cargo fetch` on 2026-09-13: `rquickjs-sys`, `rquickjs-core`, `rquickjs-macro`,
  `relative-path` (MIT) lack one; `rquickjs`, `roxmltree`, `httparse`, `tungstenite` and its tree
  ship theirs. Also check `r-efi` (pulled by `getrandom` for UEFI targets; it may already be
  filtered by platform) at phase 0.
- QuickJS-NG itself is MIT; its license text lives inside `rquickjs-sys`'s bundled sources, so the
  override folder should carry the QuickJS-NG LICENSE too, with a NOTICE naming it.

---

## 13. Build and dependencies

| Dependency | Version | License | Closure | Why |
|---|---|---|---|---|
| `rquickjs` (`loader`, `classes`, `macro`) | 0.9 | MIT | 30 crates | mapper scripts, expressions |
| `roxmltree` | 0.20 | MIT/Apache-2.0 | 1 | mapper XML (read-only DOM; the expansion of §7.1 is done on an owned tree built from it) |
| `httparse` | 1.10 | MIT/Apache-2.0 | 1 | HTTP request parsing |
| `tungstenite` (`default-features = false`, `handshake`) | 0.29 | MIT/Apache-2.0 | 33 | WebSocket server frames and handshake |
| `serde`, `serde_json` | workspace | — | present | JSON |

- MSRVs: all ≤ 1.81 (workspace `rust-version = "1.89.0"`).
- `rquickjs-sys` compiles QuickJS-NG (`quickjs/*.c`, ~52 k lines) with `cc`, the same mechanism as
  `mgba-rs`, `melonds-rs` and the shared-memory shims; bindings for `x86_64-pc-windows-gnu` are
  pregenerated (`rquickjs-sys/src/bindings/x86_64-pc-windows-gnu.rs`), so no libclang on the MSYS2
  box. The static link (`SUPERSHUCKIE_STATIC`) is unaffected: QuickJS has no runtime dependencies
  beyond libc/libm.
- `panic = "abort"` in the release profile: the engine must never panic on user data; every parse
  and script error is a `Result`, and `unwrap` is banned in the crate (a `#![deny(clippy::unwrap_used)]`).
- Windows: binding loopback ports does not trigger the firewall prompt; `localhost` is bound on
  both families (§9.1).
- CMake: nothing to add for Rust (corrosion links the whole staticlib); two new `.cpp` files in
  `add_library(supershuckie-cpp-static …)` (`supershuckie-qt/CMakeLists.txt:19-40`).
- Optional `pokeabyte-ui` feature and `scripts/update-pokeabyte-ui.sh` (§10.6).

Phase 0 must include a full Windows static build (`-DSUPERSHUCKIE_STATIC=ON`) with the four
crates added but unused, plus `make-attributions.py`, before any engine code lands. This is the
only step that cannot be verified on the Mac.

---

## 14. Performance budget and gates

Measured with `supershuckie-core/examples/ram_tools_smoke.rs`-style harnesses (§15) and the status
bar's frame-time tooltip (`main_window.cpp:360`):

| Gate | Target |
|---|---|
| Core-thread cost per frame, Crystal (16 KiB ranges) | ≤ 15 µs |
| Core-thread cost per frame, HGSS (3 MiB) / Black 2 (4 MiB default) | ≤ 0.15 ms mean, ≤ 0.5 ms max |
| Engine tick, Crystal (2.3 k properties, 318 expressions, no script) | ≤ 0.2 ms |
| Engine tick, HGSS with its decrypting `preprocessor` | ≤ 1 ms |
| Frame end → `PropertiesChanged` on a loopback client | ≤ 3 ms at 1× |
| Emulation FPS at 1× and 4×, mapper loaded vs not | no measurable difference (same test as the RAM tools) |
| GUI thread per tick | one `try_lock` + counter compare |
| Memory | engine snapshot buffers 2 × range size; QuickJS heap ≤ 64 MiB hard limit |

If the 4 MiB NDS copy proves to matter at 4×+, the follow-up is a mapper-declared `<memory>` range
(upstream HGSS already declares 3 MiB) and, later, page-granular copy-on-read tracking; neither is
needed to ship.

---

## 15. Tests

1. **Unit, `supershuckie-pokeabyte`** (pure, no emulator), one Rust test per xunit test in
   `PokeAByte.Domain.Test` (rewritten, not copied): `BitsTests`, `PropertyTypeTests`,
   `SyntaxVersionTests`, `PlatformTests`, `CalculateAddressTest`, `ChangedFieldsTests`,
   `ValidReadTests/*` (BCD 1337, bitArray, bool, byteArray, int/uint at lengths 1–4 both
   endians, `bits` 0-3/4-7, reference, strings size 1 and 2 with terminators, static value),
   `InvalidReadTests/*`, `WriteTests/*` (BCD, bool, byteArray, int/uint, bits, `WriteMultiple`
   success and all five failures, frozen bit re-application, `WriteToFrozenUpdatesBytes`),
   `ScriptTests/*` (globals, blocking preprocessor, pre/post order, all `get_uintN_le/be`,
   named namespaces, `read-function`, `after-read-value-expression`, `after-read-value-function`,
   `containerprocessor` round trip incl. six console levels, `write-function`, `copy_properties`),
   `JsonConverterTests` (exact `fieldsChanged` text, byte arrays as numbers). Plus: both `var`
   namespaces, longest-first substitution, the `game_functions` crypto against a known Gen 4 party
   slot, expression precompilation, the interrupt handler, integral-float serialisation.
2. **Corpus load test**: every XML in `~/Documents/mappers` (27) and in a checkout of
   `PokeAByte/mappers` (33) loads, with property counts and read-range sizes printed; a snapshot
   file of those numbers is committed so drift is visible.
3. **Differential test against Poke-A-Byte** (the conformance oracle): a 40-line C# console
   program added to the Poke-A-Byte checkout (not shipped) that loads a mapper with the test
   driver over a RAM dump and prints `GET /mapper` JSON; RAM dumps come from Shuckie's RAM viewer
   for Crystal, Emerald, HGSS and Black 2 at a save point. `supershuckie-pokeabyte/examples/dump_mapper.rs`
   prints the same JSON from the Rust engine; a script diffs them. Expected identical except the
   documented deviations (§7.1 step 8, §7.2, §8.4 expression scope).
4. **Core**: `memory_monitor.rs` fake-core tests for the bulk sample (frame stamp, skip when busy,
   pacing above the speed threshold, range clamping) and masked freezes.
5. **Integration**, `supershuckie-pokeabyte/tests/signalr.rs`: engine + server over a fake
   snapshot source; a `tungstenite` client performs negotiate, handshake, receives `Hello`,
   `MapperLoaded`, then `PropertiesChanged` after the fake memory changes; `POST
   /mapper/set-property-value` produces the expected write and the next `PropertiesChanged`;
   `set-properties-by-bits` matrix; CORS preflight; a slow client is dropped without stalling the
   others.
6. **Smoke**, `supershuckie-frontend/examples/pokeabyte_smoke.rs`: load a ROM headlessly, load
   a mapper, run 600 frames, assert ticks and copy bytes in the status, print µs per frame for §14.
7. **End to end (manual, checklist in `docs/pokeabyte.md`)**: with Poke-A-Byte closed,
   (a) `gen2-stp-frontend` Electron app shows party/moves/timer from Shuckie; (b) Poke-A-Byte's
   web UI at `/ui/properties` edits and freezes a value; (c) `pokeaclient` sample connects;
   (d) the RAM watch shows a frozen property's bytes holding; (e) record a replay while a client
   writes, play it back: the writes replay, and a client write during playback yields an `Error`.

---

## 16. Implementation phases (each compiles and is verified before the next)

| # | Phase | Verify |
|---|---|---|
| 0 | Dependencies + attribution overrides; Windows static build with the crates linked but unused | `cargo build` on macOS; MSYS2 static build + `make-attributions.py` pass |
| 1 | Core: bulk sample, masked freezes, `ThreadCommand::SetPokeAByteMonitor`, `CoreMonitorHandle` | `cargo test -p supershuckie-core`; `ram_tools_pacing` unchanged |
| 2 | Engine crate: XML load/expand/paths, platform table, references, models | corpus load test (60 mappers), JSON shape tests |
| 3 | Decode/encode/bits/strings/BCD, address evaluator, change flags | the ValidRead/Write/Address/ChangedFields test groups |
| 4 | Instance tick and write paths without scripts; static + dynamic containers; `dump_mapper` example | differential test vs Poke-A-Byte on the Crystal dump (no-script mapper) |
| 5 | Script host: QuickJS runtime, globals and classes, module loader, `game_functions`, hooks, precompiled expressions, interrupt/memory limits | ScriptTests group; differential test on Emerald/HGSS/Black 2 dumps with their scripts; §14 tick timings |
| 6 | Server: HTTP, CORS, static, REST routes, SignalR hub | `tests/signalr.rs`; manual: `pokeaclient`, `gameHookMapperClient.js`, Poke-A-Byte UI from a `vite preview` pointed at 8085 |
| 7 | Frontend glue, settings migration, per-ROM memory, auto-load, C ABI + header | `pokeabyte_smoke`; compile a C TU against `pokeabyte.h` with `-Wall -Wextra` |
| 8 | Qt: controller, status button, picker, menus, startup error | manual checklist §15.7 on macOS |
| 9 | Optional: bundled web UI (`pokeabyte-ui` feature), GitHub download (`ureq` decision), archives | UI served from Shuckie; download of one mapper |
| 10 | Docs: `docs/pokeabyte.md` (folders, ports, settings, differences from Poke-A-Byte, mapper authoring notes), README section, `licenses/README.md`; update the overlay plan (§17); Windows build and end-to-end on the user's machine | release checklist |

Branch off `ram-tools`. Phases 1–6 have no UI and are fully testable with `cargo test`; phases
2–6 can proceed in parallel with 1 (the crate takes snapshots as plain byte slices).

---

## 17. Relationship to the overlay plan

`~/.claude/plans/i-am-wondering-if-tender-snowglobe.md` built a custom WebSocket feed
(`supershuckie-overlay`, port 30159, `mapper.json` converted from the GameHook XML) because the
emulator had to replace GameHook. With the built-in Poke-A-Byte on 8085, the overlay's existing
client code needs **no changes** to get its data: `index.html` already loads
`http://localhost:8085/dist/gameHookMapperClient.js` and SignalR. What remains of that plan's Part A
is only the static pack server, the hotkey relay and the file operations (`FeedMsg::WriteFile…`),
which can live on this server under `/overlays/…` and a small `/shuckie/*` namespace; A2, A3's
mapper/decode/expr modules, A6's converter and Part C steps 1, 2 and 6 are dropped. Part B (the
web-view stage) is untouched. Update that plan file in phase 10 rather than now.

---

## 18. Risks and open questions

- **Port 8085 already taken** by a running `PokeAByte.Web`: the status button turns red with the
  reason; the user closes Poke-A-Byte. No fallback port, because every client hard-codes 8085.
- **Stale local mappers**: `~/Documents/mappers` is a 2024 checkout with the GameHook namespace
  and without `<memory>` ranges (so NDS mappers copy 4 MiB instead of 3). The port accepts them,
  but the user should pull `PokeAByte/mappers` (the STP folder is now `ST/`, and the STP mappers are
  at 1.6.x versus the local 1.x) before judging behaviour.
- **QuickJS vs Jint differences**: both are ES2020+; the known divergence is the expression scope
  (§8.4, made a superset). `Math.round(x, 2)` ignores its second argument in both. Interop
  differences (PascalCase aliases, `Uint8Array` in/out) are enumerated in §8.2 and covered by tests.
- **Snapshot size on NDS at high speed** (§14): paced to 60 Hz above 2×; measure before optimising.
- **Playback**: writes and freezes are refused during replay playback (`enqueue_write` drops
  them); clients get an `Error`. Poke-A-Byte had no concept of this. Documented.
- **`WriteFailed` is asynchronous**: REST returns 200 on queueing, like Poke-A-Byte; a failure is
  only visible as an `Error` hub message. An optional `?wait=1` query that blocks up to one frame
  for the result can be added later without breaking anyone.
- **GB/GBC ROM-header and `A000-BFFF` default blocks** are not readable in Shuckie's address space
  (§7.6); mappers that read them get `null` there and a load-time warning. None of the known
  mappers do.
- **AGPL adjacency** (§12): the two verbatim files are clearly delimited; everything else is new
  code. If the user prefers, `gameHookMapperClient.js` can be rewritten under GPL with the same
  class API (it is 340 lines), removing the last AGPL file from the tree.
- **`ureq`/rustls for GitHub downloads** (phase 9) needs its own MinGW static-link check; if it
  fails, spawn the system `curl` (present on Windows 10 1803+, macOS, most Linux) instead.
