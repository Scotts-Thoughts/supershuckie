# Comparing replays

Watch several replays side by side, lined up by their timers: two attempts at the same run, your
run against someone else's, or the same route on different games.

## Adding replays

1. Play a replay as usual (**Replays → Play replay**). This is the *main* replay: the timeline
   under the game controls it, and everything else follows it.
2. **Replays → Compare with another replay…** picks one of the same game's replays;
   **Replays → Compare with replay file…** picks any `.replay` file, from any game.

Each compared replay opens in a window of its own beside the main one, with a strip underneath:
its name and game, whether it is in sync, its timer (and, when both timers were stopped, its final
time and the difference from the main replay's), its counters and frame rate. Up to eight replays
can be compared at once.

A replay of another game needs that game's ROM. Super Shuckie looks for it by hash among the
loaded ROM, ROMs located before (here or in Play Together), recently opened ROMs and your
favorites; if it is not found you are asked for the file (it is remembered afterwards). Nintendo 3DS replays cannot be compared (only one
3DS game can run at a time).

## How they are lined up

Replays are lined up by the timer you start and stop while recording (the start and end markers,
including the time already on the timer when it was started). When the main timer reads 1:23.4,
every compared replay shows the moment its own timer read 1:23.4. A replay without a start marker
uses its replay time instead, so two replays without timers line up from their first frames.

Before a replay's timer starts it waits on its first frame ("Not started yet"); after its timer
stops it plays on, so a slower run keeps going after a faster one has finished.

## Controls

The main replay's timeline drives everything: play and pause, stop and resume, clicking or dragging
the bar, frame stepping and speed changes all apply to every compared replay at once, so they stay
in step. A compared replay that has to emulate its way to a new spot shows "Catching up…" for a
moment.

When the main replay reaches its end, the compared replays that are still going play on to their
end at the main replay's speed. Seeking the main replay brings them all back.

Closing a compared replay's window (or **Stop comparing this replay** in its right-click menu)
stops comparing it; **Replays → Stop comparing replays** stops them all. Closing the main replay
closes them too. Right-click a window for its scale.

Compared replays run below the main game's priority and without sound.
