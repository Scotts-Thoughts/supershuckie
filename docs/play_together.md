# Play Together

Play alongside friends, or race them, with everyone's game on your screen. Each player runs their
own game in their own Super Shuckie; what travels over the network is only what a replay file
holds (the buttons pressed each frame, memory edits, resets, save-state loads), and your copy of
Super Shuckie emulates each friend's game from that, so a friend's game costs a few kilobytes a
second and looks exactly like theirs.

Everything is in the **Play Together** menu.

## Starting a session

One player hosts:

1. Load your game.
2. **Play Together → Host or join a session…** (Ctrl+Shift+H), **Host** tab, pick a name and a
   colour, click **Start hosting**.
3. Share the code (`address:port`) it shows.

Everyone else joins: load *your own* game, **Join** tab, paste the code, pick a name and a colour,
**Join**.

Every player has a colour of their own in the session. If the one you picked is already taken (or
you left it on **Random**), the host gives you a free one; the session line in the dialog and the
table show who has which.

The code is the host's address on the local network. For friends elsewhere on the internet, the
host has to forward TCP port 30170 (or the port chosen in the dialog) on their router, or
everyone uses a VPN such as Tailscale, and the host shares the address that reaches them. Share
codes only with people you trust: there is no password yet.

## Friends' windows

Every friend's game opens in a window of its own, framed in their colour, with a strip underneath
in that colour: their name and ROM, how far behind their game is here (a few frames is normal),
their replay timer and counters, and frame rate. New windows cascade beside your game so they never
land on top of each other. Closing the window hides it; **Show friends' windows** brings them back. Right-click a
window for its scale, **Friend audio** (off unless you turn it on) and **Locate ROM…**.

To follow a friend you need their exact ROM file. Super Shuckie looks for it by hash among the
ROMs you have opened or favourited; if it is not found, it asks you to locate it, and checks that
the file you pick really is the same ROM.

Your own game keeps running exactly as it always does: your emulator's thread has priority over
the friends' ones, which follow at their own pace and jump forward with a fresh snapshot of the
friend's game if they ever fall far behind or drift out of step.

## Races

The host's **Reset everyone (race start)** counts down three seconds on every screen, then
everyone's console resets at once (and unpauses everyone).

### Starting from the same save state

To race from a point other than power-on, the host gets their game to that point and turns on
**Start from host's save state**. The host's game pauses, its save state goes to everyone, and
every player's own game loads it and pauses there, so all games sit at the exact same state.
Whoever joins afterwards gets the state too, and someone playing a different ROM (or a different
console, emulator core or BIOS: the state could not be loaded) cannot join while it is set; the
host cannot turn it on while such a player is already in the session. While it is set, **Reset
everyone** becomes **Restart everyone from the start state**: the countdown ends with everyone
loading the state again and unpausing together. Turning it off changes nobody's game. A loaded
start state is in your save-state history, so **Undo load save state** brings your own game back.

## Pausing together

With **Sync pause** on, when anyone pauses their game (the Pause menu item, a pause hotkey, or a
click on the screen), everyone's game pauses, and when anyone unpauses, everyone's does. The
session window and the status bar say who paused. The host's setting applies to the whole
session: it is what everyone sees in the menu, only the host can change it, and turning it on
makes everyone adopt the host's current pause state (a friend who joins while the session is
paused arrives paused). Outside a session the toggle is your own setting, used when you host.

## Link cable

Two players in a session can plug a link cable between their games and trade or battle, in the
Game Boy, Game Boy Color and Game Boy Advance Pokémon games (any two games of one family: Game
Boy / Game Boy Color / Super Game Boy 2, or Game Boy Advance). Right-click the friend's window
and choose **Plug in link cable**; they get a prompt and choose **Plug in** or **Decline**. Both
games pause for a moment while the cable goes in, then run in step: both players' games are on
both screens, and the game's own link menus (Cable Club, Union Room, the trade centre) work as
they do on real hardware. **Unplug link cable** (in the friend's window or the Play Together
menu) pulls it out on both sides; so does either player leaving the session.

How it works: nothing of the cable travels over the network. Your copy of Super Shuckie already
runs the friend's game (that is what their window shows), so it plugs your game into that copy
and runs the two in lockstep; what the two machines exchange is only each player's inputs, a
few frames ahead of time, so both machines feed both games the same buttons on the same frames
and stay identical. That is why:

* Both games run with a small **input delay** while linked — about a frame more than the
  one-way trip through the host, 1 on a LAN, typically 2–5 over the internet. It is chosen from
  the ping automatically; **Play Together → Link cable input delay** sets a minimum (the larger
  of the two players' settings wins) if a connection is jittery.
* Both games run at **the host's game speed** (the session host's base speed and turbo, whether
  or not the host is one of the linked players), on both machines, so a fast-forwarding host
  fast-forwards every linked pair. A client's own speed controls do nothing while its cable is
  in and come back when it is unplugged. The input delay is sized for the trip at the host's
  speed as the cable goes in; if the host then speeds up a lot, the pair may stall on late
  frames until it slows down or the cable is re-plugged. Pausing either game pauses both (the
  friend's window says "Waiting for…" while the other side is paused or its frames are late).
* **Save-state loads, replays, video exports and core reloads are off** while linked ("Unplug
  the link cable first"). Resets go through, delayed like inputs, and so do RAM edits from the
  RAM tools and Poke-A-Byte.
* If the two machines ever disagree about the pair (a desync), the other player leaves, or one
  stops hearing from the other for two minutes, the cable comes out by itself and both games go
  on alone; a message says why.

Third players keep following both linked games, and everyone's replay files — your own, and
the friends' — record the link traffic, so a replay of a trade or a battle plays back on its own
(replay format 7).

## Replays of friends' games

With **Save friends' games as replays** on (the default), every friend's game is also written to
a replay file named `<friend> - <date>.replay` in that ROM's replay folder, starting from the
moment their game first appeared here. They play back like any replay.

To have every replay start at the same moment instead — everyone gets ready, then the files
begin together — use **Record everyone's replay now**: it starts a recording of your own game
(as **Record replay** would) and a fresh file for every friend's game followed here, finishing
any file already being written for them first. Turn **Save friends' games as replays** off if
you only want the files that start this way. The same item becomes **Stop recording everyone's
replay** while everyone is being recorded, and stops all the files together; your own recording
can still be stopped on its own from the Replays menu. A friend whose ROM is not on this
machine has no file (nothing runs here to record).

## Poke-A-Byte and friends' games

Your own game is served to Poke-A-Byte the way it always was: on UDP port 55356 (change it under
**Settings → Poke-A-Byte → Port…**; Poke-A-Byte's `POKEAPROTOCOL_PORT` setting must match if you
do). With **Settings → Poke-A-Byte → Serve friends' games** on (the default), every friend's game
that runs on your machine is served too, each on the lowest free port above yours: the first
friend on 55357, the next on 55358, and so on. Right-click a friend's window to see their port
(**Poke-A-Byte integration (port N)**), or to turn it off or on for that one friend.

One Poke-A-Byte reads all of them. Its usual routes read your game; the same routes prefixed with
`/instances/<port>/` read a friend's, and its SignalR hub at `/instances/<port>/updates` streams
only that game's changes:

```text
PUT  http://localhost:8085/mapper-service/change-mapper                    ← your game
PUT  http://localhost:8085/instances/55357/mapper-service/change-mapper    ← the friend on 55357
GET  http://localhost:8085/instances/55357/mapper
GET  http://localhost:8085/instances                                       ← who is loaded, by port
```

Poke-A-Byte only ever reads a friend's game: writes and freezes are refused there (their game is
a replay of theirs, not something to edit). The external-commands server tells a frontend which
port belongs to whom: `GET http://127.0.0.1:30158/play-together` returns the session state, with a
`pokeabyte_port` per participant (see [external_commands.md](external_commands.md#play-together)).

## What you cannot do in a session

* Load a replay, or a different Game Boy model (leave the session first). Recording your own
  replay is fine.
* Open another ROM without leaving (Super Shuckie asks).
* Play a Nintendo DS game together (not yet: a DS snapshot is 20 MB).
* While a link cable is in: load a save state, load or seek a replay, export video, reload the
  core, or (as a client) run at another speed than the host's (unplug it first).

## Checking it works

`supershuckie-frontend/examples/play_together_smoke.rs` runs two or three complete frontends in
one process on the loopback interface, hosts, joins, follows, resets everyone, leaves, and plays
a saved friend replay back:

```text
play_together_smoke <rom.gbc|rom.gba> [--players 3] [--speed 4] [--seconds 20] [--link]
play_together_smoke --link [--gba] [--players 3]
```

It fails when any player's own game drops below 90% of the requested speed, when a follower falls
more than a second behind or desyncs, when a friend's screen never arrives, or when a saved
friend replay does not play to its end. With `--link` it also plugs a link cable between the
host and the first joiner (a declined request, the handshake, both games in lockstep at the host's
speed, the host's speed changes reaching both while the joiner's do nothing, the refusals, a pause
stalling the other side, an unplug from the other side, then a second cable
that stays in through a race start and the leave); with no ROM given it runs the link test ROM
(`supershuckie_core::link::test_rom`, Game Boy or with `--gba` Game Boy Advance), which sends
256 bytes each way over the cable, and checks the bytes on both machines' copies of both games.
The wire format is [play-together-protocol.md](play-together-protocol.md).
