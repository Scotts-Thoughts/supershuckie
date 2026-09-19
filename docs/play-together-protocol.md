# Play Together protocol, version 4

`supershuckie-play-together` is the network layer of Play Together: one player hosts a TCP port,
the others connect, and the host relays every participant's stream to every other participant.
Each participant publishes its emulator session as the same packets a replay file holds, so a
follower is an emulator playing a replay that arrives while it is written.

Everything read from a socket is treated as hostile: lengths are capped before anything is
allocated, bodies are read in 64 KiB chunks, and unknown message tags are skipped so a newer
version can add messages without breaking an older one.

## Framing

Everything is little-endian:

```text
message := u32 length | u8 tag | payload      (length counts tag + payload)
string  := u32 length | UTF-8 bytes
bytes   := u32 length | raw bytes
hash    := 32 raw bytes (blake3)
bool    := u8 (0 or 1)
vec<T>  := u32 count | T*                      (count is capped per field before any allocation)
```

Limits: a `Stream`, `Snapshot` or `StartState` message may be up to 48 MiB; every other tag (and
any unknown tag) at most 64 KiB, and a `LinkFrame`'s events at most 32 KiB. A snapshot's or start state's decompressed state is at most
48 MiB. Display names are at most
32 bytes, metadata strings 255 (the replay header's limit), free text 1024, input buffers 64
bytes, and a snapshot carries at most 256 counters. A session holds at most 8 participants.

Before a connection has sent `Hello`, only `Hello` (and unknown tags) are accepted; any other
known tag is a protocol violation and the host answers `Error` and closes.

## Messages

Peer ids are `u16`: the host is always 1, clients get 2, 3, ... and an id is never reused within
a session; 0 means "none" or, as a target, "everyone".

| Tag | Message | Payload | Direction |
|---|---|---|---|
| 0x01 | Hello | `protocol_version u32, replay_version u32, app_version string, display_name string, color u8, publisher PublisherInfo` | client → host |
| 0x02 | Welcome | `your_peer_id u16, session_id u64, your_display_name string, your_color u8, participants vec<ParticipantInfo>` (the host included) | host → client |
| 0x03 | Refused | `reason u32, text string` (then the host closes) | host → client |
| 0x04 | PeerJoined | `participant ParticipantInfo` | host → clients |
| 0x05 | PeerLeft | `peer_id u16, reason u32` | host → clients |
| 0x06 | ResetAll | `race_id u32, countdown_millis u32` | host → clients |
| 0x07 | SyncPause | `enabled bool, paused bool` | host → clients |
| 0x08 | Pause | `from u16, paused bool` | participant → host → others |
| 0x10 | Stream | `from u16, first_frame u64, bytes bytes` | publisher → host → others |
| 0x11 | Snapshot | `from u16, target u16, frame u64, elapsed_millis u64, input bytes, speed u16, counters vec<(string, i64)>, encoding u8, state_len u64, state bytes` | publisher → host → target(s) |
| 0x12 | SyncHash | `from u16, frame u64, hash` | publisher → host → others |
| 0x13 | RequestSnapshot | `requester u16, target u16` | follower → host → publisher |
| 0x14 | StartState | `rom_checksum hash, encoding u8, state_len u64, state bytes` | host → clients |
| 0x20 | Ping | `nonce u32, sent_unix_millis u64` | both |
| 0x21 | Pong | same as Ping | both |
| 0x22 | Goodbye | (empty) | both |
| 0x2F | Error | `text string` (then the sender closes) | both |
| 0x30 | LinkRequest | `from u16, target u16, nonce u32, console u32` | participant → host → target |
| 0x31 | LinkAccept | `from u16, target u16, nonce u32` | target → host → requester |
| 0x32 | LinkDecline | `from u16, target u16, nonce u32, reason u32` | target (or host) → requester |
| 0x33 | LinkStart | `from u16, target u16, nonce u32, frame u64, input bytes, rtt_millis u32, delay_setting u8` | both ends, via the host |
| 0x34 | LinkFrame | `from u16, target u16, frame u64, elapsed_millis u64, events bytes, pair_hash_frame u64, pair_hash hash` | both ends, via the host, once per lockstep frame |
| 0x35 | Unlink | `from u16, target u16, reason u32` | either end (or host) → the other |
| 0x36 | PeerLinked | `a u16, b u16` | host → clients |
| 0x37 | PeerUnlinked | `a u16, b u16` | host → clients |

`PublisherInfo` is `metadata, initial_input bytes, speed u16, frame u64`, where `metadata` is
`console_type u32, rom_name string, rom_filename string, rom_checksum hash, bios_checksum hash,
emulator_core_name string, patch_format u32, patch_target_checksum hash` (the replay header's
fields; crops and the timer offset are not sent). `ParticipantInfo` is `peer_id u16,
display_name string, color u8, app_version string, publisher PublisherInfo`. A `speed` of 0 and a
peer id of 0 where one is required are errors.

### Colours

Every participant has a colour nobody else in the session has, so their window can be told apart.
A colour is a 1-based index into a fixed 22-entry palette (Pink, Red, Tan, Orange, Brown, Pale
Yellow, Yellow, Olive, Lime, Pale Green, Green, Cyan, Teal, Blue, Navy, Dark Aqua, Bluish Grey,
Purple, Magenta, White, Grey, Black; the RGB values are in `src/color.rs`). `Hello.color` is a
request: 0 means "any", and the host gives the requested colour when nobody has it, otherwise a
free one of its choosing. `Welcome.your_color` and `ParticipantInfo.color` are assignments and must
name a palette entry (0 or out of range is a decode error). The host picks its own colour the same
way.

### Sync pause

With sync pause on, one participant pausing pauses everyone, and unpausing likewise. The host's
setting rules the session: it sends `SyncPause` right after `Welcome` (so a joiner adopts the
session's pause state as it arrives) and again whenever the setting changes, with `paused` being
the host's own pause state at that moment, which everyone adopts. `paused` is meaningless while
`enabled` is false. A client that receives `SyncPause` from another client is a protocol error at
the host.

A participant that pauses or unpauses sends `Pause` (a client leaves `from` as 0; the host fills
it in, like `RequestSnapshot.requester`, at the same byte offset as `Stream.from`). The host
relays it to everyone but the sender, only while sync pause is enabled; otherwise it is dropped
without an error, since the client may not have heard that the setting changed. Nobody gets their
own pause echoed back. There is no arbitration: the last `Pause` relayed wins, and a participant
whose pause differs from the last state it adopted publishes it, so two players pausing at once
simply both pause everyone. The race-start reset unpauses everyone by itself, without a `Pause`.

### Start state

The host can make everyone start from its own save state. `StartState` carries that state the
way a snapshot carries one (`encoding` 0 raw or 1 zstd, `state_len` checked before decompressing)
together with the host's ROM hash; a `state_len` of 0 with an empty `state` clears it. The host
sends it to every client when it is set or cleared, and to each client right after `Welcome`
while one is set. A client loads it into its own game and pauses. While one is set, a `Hello`
whose console, ROM, emulator core or BIOS differs from the host's is refused with reason 7, since
that game could not load the state; a participant already in the session is never removed for it
(the host's application refuses to set a start state while such a participant is present). A
race start (`ResetAll`) while one is set loads the state again instead of resetting the console.
A client sending `StartState` is a protocol error.

### Link cable

Two participants that follow each other can plug a link cable between their games (trading and
battling in the Game Boy and Game Boy Advance games). Nothing of the cable itself crosses the
network: each machine already runs both games (its own and its follower of the other's), so it
plugs its own console into that follower and runs the two in delay-based lockstep; what the two
machines exchange is each console's events, `delay` frames ahead of time, so that both feed both
consoles the same inputs on the same frames and compute the same pair.

The handshake, all relayed by the host (which overwrites `from` at the same offset as
`Stream.from`):

1. The requester sends `LinkRequest { target, nonce, console }`. The host declines on the
   target's behalf (`LinkDecline` reason 1 busy, 2 console mismatch, 5 unavailable) when either
   end is already linked, the two consoles are not of one family (Game Boy / Game Boy Color /
   Super Game Boy 2, or Game Boy Advance), or the target is gone; otherwise it relays it and
   remembers the request (one outstanding per requester; a later one replaces it).
2. The target answers `LinkAccept` or `LinkDecline { reason }` (0 declined, 3 not following the
   requester closely enough, 4 no answer, 5 unavailable) carrying the request's `nonce`. An
   accept nobody asked for is dropped. On an accept the host records the pair and tells everyone
   with `PeerLinked`; if either end got linked meanwhile it sends the accepter `Unlink` reason 5
   instead.
3. Both ends stop their own game at a frame boundary and send `LinkStart`: the frame it stopped
   at, the input held there, their last round-trip time to the host and their input-delay
   setting (0 = automatic, else 1–15 frames). From the two `LinkStart`s both ends compute the
   same delay: `max(ceil((rtt_a + rtt_b) / 2 / frame_ms) + 1, setting_a, setting_b)` clamped to
   1..=15, where `frame_ms` is 1000 / 59.7275. Each end brings its follower of the other's game
   exactly to the other's stopped frame from the stream it already has, then plugs the two
   consoles together; the first console of the pair is the lower peer id.
4. While linked each end sends one `LinkFrame` per lockstep frame: `frame` is the link frame the
   events land on (counted from the plug-in, `delay` frames ahead of the sender's own console),
   `elapsed_millis` the sender's recording clock as it sends (the receiver's copy of that console
   adopts it, so its replay file's time never runs backwards at the next snapshot), `events`
   whole replay packets limited to `NoOp`, `ChangeInput`, `WriteMemory` and `ResetConsole` (at
   most 32 KiB; anything else is a protocol error), and every 60 frames of the first console a
   pair hash: the blake3 of the two consoles' work-RAM hashes, keyed by the first console's link
   frame (`pair_hash_frame`; an all-zero hash means none). A mismatch between the two machines'
   hashes for the same frame ends the link with reason 1. The host relays a `LinkFrame` or
   `LinkStart` only between the two ends it has recorded as linked, and a receiver drops frames
   from anyone it has no link with. A frame that has not arrived stalls the pair (nothing is
   skipped); two minutes of stall end the link with reason 2 (a partner that left is unlinked
   by the host long before that). Link cable messages travel on a
   lane of their own, written ahead of queued streams and snapshots.
5. `Unlink { reason }` from either end (0 unplugged, 1 desync, 2 timeout, 3 the other player
   left, 4 failed, 5 busy) clears the pair; the host relays it and tells everyone with
   `PeerUnlinked`. When one end leaves the session the host sends the other `Unlink` reason 3
   (ahead of the `PeerLeft`) and `PeerUnlinked`. Unknown reasons decode as "other" rather than
   failing. Afterwards each end asks the other for a fresh snapshot and follows the stream again.

While linked, what each console received over the cable during a frame goes into its stream and
its replay files as a `SerialIn` packet (replay format 7), so a third participant following a
linked game, and a replay of one, reproduce the transfer without a partner.

### Version history

- 4: the link cable messages `0x30`–`0x37`; `Stream.bytes` may carry `SerialIn` (replay format 7).
- 3: `SyncPause`, `Pause`, `StartState`, `Refused.reason` 7.
- 2: `Hello.color`, `Welcome.your_color`, `ParticipantInfo.color`.
- 1: initial.

`Refused.reason`: 0 protocol version, 1 replay version, 2 session full, 3 console unsupported
(Nintendo DS, or unknown), 4 patched ROM unsupported, 5 bad Hello, 6 host shutting down, 7 start
state mismatch. `PeerLeft.reason`: 0 left, 1 timeout, 2 too slow, 3 protocol error, 4 I/O error,
5 kicked, 6 host left.

### Streams

`Stream.bytes` is a run of whole replay packets in the replay file encoding (the `replay_version`
from `Hello`), and `first_frame` is the publisher's frame count before the first `NextFrame` in
it. Only these packets may appear: `NoOp`, `NextFrame`, `ChangeInput`, `WriteMemory`,
`ChangeSpeed`, `ResetConsole`, `LoadSaveState`, `IncrementCounter`, `SerialIn`. Keyframes, blobs
and bookmarks are file structure and are refused with a protocol error.

A publisher sends one `Stream` per emulated frame when its writer keeps up; when it does not,
whatever frames are queued go out as one message (split between packets at 256 KiB). The host
relays each message as received, with `from` overwritten by the sender's real id, never
re-batching.

### Snapshots

A snapshot is the publisher's exact save state at `frame`, with the input, speed, time and
counters that go with it. `encoding` is 0 for a raw state or 1 for a zstd frame (level 3); the
receiver checks `state_len` before decompressing. `target` is one peer or 0 for everyone.

A follower asks for one with `RequestSnapshot` (a client leaves `requester` as 0 and the host
fills it in): when it subscribes, when a stream's `first_frame` is not the one it expected, when
its work-RAM hash differs from the publisher's, when it fell too far behind, or when its queue
overflowed. Requests to the same publisher are deduplicated for two seconds on the follower, and
the publisher announces at most one `SnapshotRequested` per two seconds carrying every requester
since the last one; it answers with a snapshot to the single requester, or to everyone.

Until its first snapshot from a publisher, a follower discards that publisher's streams and
hashes. From the snapshot on it expects `first_frame` to run contiguously.

### Sync hashes

Every 60 frames a publisher sends the blake3 of its work RAM (GB: WRAM, WRAMX and HRAM; GBA:
EWRAM and IWRAM) after frame `frame`, right after that frame's `NextFrame` and before any packet
of the next frame. A follower hashes the same regions when it reaches that frame and asks for a
snapshot on a mismatch; hashes for the first three frames after a snapshot are ignored.

## Sessions

Host: a listener thread accepts (polled every 20 ms), a ticker (about once a second) enforces the
handshake and idle timeouts, and each connection has a reader and a writer thread. Client: a
connector thread resolves and connects (5 s), sends `Hello`, waits for `Welcome`/`Refused` (5 s),
then becomes the reader, beside a writer thread.

Every connection has a bounded outbound queue (64 MiB or 16 384 items); pushing never blocks,
and a peer whose queue overflows is dropped as too slow. The writer sends `Ping` after one second
of silence; a reader that hears nothing for ten seconds drops the connection. Snapshots are
compressed on the writer thread, never on the emulator's.

Admission, in order: protocol version, replay version, session full, console (Nintendo DS is
refused unless the host allows it), patched ROM, then the display name is made unique
(`Ash (2)`, `Ash (3)`, ...). A second `Hello` on an admitted connection is a protocol error.

`Goodbye` from a client removes it and tells everyone else; `Goodbye` from the host ends the
session for everyone (there is no host migration).

## Ports

The host binds `0.0.0.0` on port 30170 unless configured otherwise (the REST server is
`127.0.0.1:30158`, Poke-A-Byte `127.0.0.1:55356`). There is no encryption or authentication in
version 1: share a code only with people you trust, and prefer a LAN or a VPN.
