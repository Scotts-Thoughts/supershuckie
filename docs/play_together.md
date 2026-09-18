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
2. **Play Together → Host or join a session…** (Ctrl+Shift+P), **Host** tab, pick a name, click
   **Start hosting**.
3. Share the code (`address:port`) it shows.

Everyone else joins: load *your own* game, **Join** tab, paste the code, pick a name, **Join**.

The code is the host's address on the local network. For friends elsewhere on the internet, the
host has to forward TCP port 30170 (or the port chosen in the dialog) on their router, or
everyone uses a VPN such as Tailscale, and the host shares the address that reaches them. Share
codes only with people you trust: there is no password yet.

## Friends' windows

Every friend's game opens in a window of its own with a strip underneath: their name and ROM,
how far behind their game is here (a few frames is normal), their replay timer and counters, and
frame rate. Closing the window hides it; **Show friends' windows** brings them back. Right-click a
window for its scale, **Friend audio** (off unless you turn it on) and **Locate ROM…**.

To follow a friend you need their exact ROM file. Super Shuckie looks for it by hash among the
ROMs you have opened or favourited; if it is not found, it asks you to locate it, and checks that
the file you pick really is the same ROM.

Your own game keeps running exactly as it always does: your emulator's thread has priority over
the friends' ones, which follow at their own pace and jump forward with a fresh snapshot of the
friend's game if they ever fall far behind or drift out of step.

## Races

The host's **Reset everyone (race start)** counts down three seconds on every screen, then
everyone's console resets at once.

## Replays of friends' games

With **Save friends' games as replays** on (the default), every friend's game is also written to
a replay file named `<friend> - <date>.replay` in that ROM's replay folder, starting from the
moment their game first appeared here. They play back like any replay.

## What you cannot do in a session

* Load a replay, or a different Game Boy model (leave the session first). Recording your own
  replay is fine.
* Open another ROM without leaving (Super Shuckie asks).
* Play a Nintendo DS game together (not yet: a DS snapshot is 20 MB).

## Checking it works

`supershuckie-frontend/examples/play_together_smoke.rs` runs two or three complete frontends in
one process on the loopback interface, hosts, joins, follows, resets everyone, leaves, and plays
a saved friend replay back:

```text
play_together_smoke <rom.gbc|rom.gba> [--players 3] [--speed 4] [--seconds 20]
```

It fails when any player's own game drops below 90% of the requested speed, when a follower falls
more than a second behind or desyncs, when a friend's screen never arrives, or when a saved
friend replay does not play to its end. The wire format is
[play-together-protocol.md](play-together-protocol.md).
