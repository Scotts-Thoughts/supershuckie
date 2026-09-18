# Play Together protocol, version 1

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

Limits: a `Stream` or `Snapshot` message may be up to 48 MiB; every other tag (and any unknown
tag) at most 64 KiB. A snapshot's decompressed state is at most 48 MiB. Display names are at most
32 bytes, metadata strings 255 (the replay header's limit), free text 1024, input buffers 64
bytes, and a snapshot carries at most 256 counters. A session holds at most 8 participants.

Before a connection has sent `Hello`, only `Hello` (and unknown tags) are accepted; any other
known tag is a protocol violation and the host answers `Error` and closes.

## Messages

Peer ids are `u16`: the host is always 1, clients get 2, 3, ... and an id is never reused within
a session; 0 means "none" or, as a target, "everyone".

| Tag | Message | Payload | Direction |
|---|---|---|---|
| 0x01 | Hello | `protocol_version u32, replay_version u32, app_version string, display_name string, publisher PublisherInfo` | client → host |
| 0x02 | Welcome | `your_peer_id u16, session_id u64, your_display_name string, participants vec<ParticipantInfo>` (the host included) | host → client |
| 0x03 | Refused | `reason u32, text string` (then the host closes) | host → client |
| 0x04 | PeerJoined | `participant ParticipantInfo` | host → clients |
| 0x05 | PeerLeft | `peer_id u16, reason u32` | host → clients |
| 0x06 | ResetAll | `race_id u32, countdown_millis u32` | host → clients |
| 0x10 | Stream | `from u16, first_frame u64, bytes bytes` | publisher → host → others |
| 0x11 | Snapshot | `from u16, target u16, frame u64, elapsed_millis u64, input bytes, speed u16, counters vec<(string, i64)>, encoding u8, state_len u64, state bytes` | publisher → host → target(s) |
| 0x12 | SyncHash | `from u16, frame u64, hash` | publisher → host → others |
| 0x13 | RequestSnapshot | `requester u16, target u16` | follower → host → publisher |
| 0x20 | Ping | `nonce u32, sent_unix_millis u64` | both |
| 0x21 | Pong | same as Ping | both |
| 0x22 | Goodbye | (empty) | both |
| 0x2F | Error | `text string` (then the sender closes) | both |

`PublisherInfo` is `metadata, initial_input bytes, speed u16, frame u64`, where `metadata` is
`console_type u32, rom_name string, rom_filename string, rom_checksum hash, bios_checksum hash,
emulator_core_name string, patch_format u32, patch_target_checksum hash` (the replay header's
fields; crops and the timer offset are not sent). `ParticipantInfo` is `peer_id u16,
display_name string, app_version string, publisher PublisherInfo`. A `speed` of 0 and a peer id
of 0 where one is required are errors.

`Refused.reason`: 1 protocol version, 2 replay version, 3 session full, 4 console unsupported
(Nintendo DS, or unknown), 5 patched ROM unsupported, 6 bad Hello, 7 host shutting down.
`PeerLeft.reason`: 1 left, 2 timeout, 3 too slow, 4 protocol error, 5 I/O error, 6 kicked,
7 host left.

### Streams

`Stream.bytes` is a run of whole replay packets in the replay file encoding (the `replay_version`
from `Hello`), and `first_frame` is the publisher's frame count before the first `NextFrame` in
it. Only these packets may appear: `NoOp`, `NextFrame`, `ChangeInput`, `WriteMemory`,
`ChangeSpeed`, `ResetConsole`, `LoadSaveState`, `IncrementCounter`. Keyframes, blobs and
bookmarks are file structure and are refused with a protocol error.

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
