# Super Shuckie external commands

You can control Super Shuckie using REST requests. To enable this feature, you
will need the feature enabled inside the application itself.

The server is `127.0.0.1:30158`

## Table of contents

- [JS API](#js-api)
  - [Usage](#usage)
    - [JavaScript](#javascript)
    - [TypeScript](#typescript)
  - [Example code](#example-code)
- [Rest command reference](#rest-command-reference)
  - [add-bookmark](#add-bookmark)
  - [bookmarks](#bookmarks)
  - [delete-bookmark](#delete-bookmark)
  - [enumerate-replays](#enumerate-replays)
  - [go-to-bookmark](#go-to-bookmark)
  - [go-to-frame](#go-to-frame)
  - [increment-counter](#increment-counter)
  - [load-replay](#load-replay)
  - [mark-start](#mark-start)
  - [mark-end](#mark-end)
  - [set-paused](#set-paused)
  - [set-playback-speed](#set-playback-speed)
  - [stats](#stats)
  - [toggle-range-bookmark](#toggle-range-bookmark)
  - [update-bookmark](#update-bookmark)

## JS API

To make things simple, you can use the JavaScript API. You can use it from
Super Shuckie's source code (`/supershuckie-frontend-webserver/js/client.js`) or
load it from a Super Shuckie instance at `http://127.0.0.1:30158/client.js`

### Usage

First, you will need to somehow import it. There are a few ways you can do this.

#### JavaScript

You can import it in a JavaScript module by putting this at the top of your
module:

```javascript
import { SuperShuckieClient } from "http://127.0.0.1:30158/client.js"
```

You can alternatively import it dynamically from within an async function with
this:

```javascript
const { SuperShuckieClient } = await import("http://127.0.0.1:30158/client.js")
```

#### TypeScript

If you are using TypeScript, you can directly use the definitions and code from
Super Shuckie's source tree at `/supershuckie-frontend-webserver/js`

Ensure that client.d.ts and client.js are in the same directory. Then import it
like this:

```typescript
import { SuperShuckieClient } from "./somepath/client"
```

### Example code

```html
<script type="module">
    // Import the client...
    import { SuperShuckieClient } from "http://127.0.0.1:30158/client.js"
    
    // Next, create the client.
    const shuckie = new SuperShuckieClient();

    // Here's a function updating your overlay
    async function update() {
        const stats = await shuckie.stats()

        // Start the timer (at a 1 second offset) if it has not already been set
        if (stats.is_recording && stats.time_current == null) {
            await shuckie.mark_start(1000)
        }

        console.log(`Current timer: ${stats.time_current}`)
    }

    // We want to run that function 60 times a second. Since it's async, we want
    // to wrap it in something that prevents it from being called multiple times
    // at once (race conditions).
    let busy = false
    setInterval(
        () => {
            if (busy) { return; }
            busy = true
            update().finally(() => { busy = false })
        },
        1000 / 60 // update at 60 Hz
    )
</script>
```

Refer to [`client.d.ts`] for documentation on this API.

[`client.d.ts`]: ../supershuckie-frontend-webserver/js/client.d.ts

## REST command reference

If you are not using JS or you do not wish to use the above API, you can also
directly interact with Super Shuckie with these requests.

### add-bookmark

Add a bookmark to the replay being recorded or played back. Bookmarks are saved
inside the replay file. Returns the new bookmark as JSON (see
[bookmarks](#bookmarks)).

Usage:

- `http://127.0.0.1:30158/add-bookmark`
- `http://127.0.0.1:30158/add-bookmark?name=NAME&type=TYPE`
- `http://127.0.0.1:30158/add-bookmark?frame=FRAME&out=OUT`
- `http://127.0.0.1:30158/add-bookmark?keyframe=true`

Arguments:

| Argument   | Default             | Description                                                                                                                      |
|------------|---------------------|----------------------------------------------------------------------------------------------------------------------------------|
| `name`     | a generic name      | The name of the bookmark, e.g. `Bookmark 3` or `Deaths 2` (the type's name followed by a number).                                |
| `type`     | the active type     | The name of the bookmark's type. A type that does not exist yet is created with a color of its own. `none` makes it untyped.     |
| `type_id`  | (none)              | The id of the bookmark's type (16 hex digits), instead of `type`.                                                                |
| `frame`    | the current frame   | The frame the bookmark starts on. It must not be past the last frame recorded so far (or the last frame of the replay).          |
| `out`      | (none)              | The frame the bookmark ends on, making it a range: a frame number, or `now` for the current frame.                              |
| `keyframe` | `false`             | `true` to place a keyframe bookmark, which playback returns to without re-emulating. Only allowed without `frame` (see below). |

A keyframe bookmark made while recording writes a full keyframe at the current
frame and is placed 3 frames later; seeking to it loads that keyframe and
emulates those 3 frames. While playing back, no keyframe can be written, so it
is placed 3 frames after the replay's last keyframe at or before the current
frame (keyframes are 2 seconds apart by default).

Adding a bookmark to a replay made before format v5 upgrades the replay file,
after which older versions of Super Shuckie cannot open it. Replays older than
format v3 must be converted first.

### bookmarks

Get the bookmarks of the replay being recorded or played back, as JSON.

Usage:

- `http://127.0.0.1:30158/bookmarks`

| Field            | Type                                    | Description                                                                                            |
|------------------|-----------------------------------------|--------------------------------------------------------------------------------------------------------|
| `replay`         | `string \| null`                        | The name of the replay, or `null` if no replay is recording or playing back.                          |
| `state`          | `"none" \| "recording" \| "playback"`   | What the replay is doing.                                                                              |
| `generation`     | `number`                                | Changes whenever anything here changes (also in [stats](#stats) as `bookmark_generation`).            |
| `editable`       | `boolean`                               | `true` if bookmarks can be added and changed.                                                          |
| `needs_upgrade`  | `boolean`                               | `true` if the next change upgrades the replay file to format v5.                                      |
| `replay_version` | `number \| null`                        | The format version of the replay being played back.                                                    |
| `problem`        | `string \| null`                        | Why bookmarks cannot be changed, or why the last save failed.                                         |
| `open_range`     | `number \| null`                        | The id of the range started with [toggle-range-bookmark](#toggle-range-bookmark) and not ended yet.   |
| `active_type`    | `string \| null`                        | The id of the type new bookmarks get when none is given.                                               |
| `bookmarks`      | `Bookmark[]`                            | The bookmarks, in frame order (see below).                                                             |
| `types`          | `Type[]`                                | The user's bookmark types, then any types only known from the replay (see below).                     |

Each bookmark:

| Field        | Type             | Description                                                                         |
|--------------|------------------|-------------------------------------------------------------------------------------|
| `id`         | `number`         | Identifies the bookmark within its replay.                                          |
| `name`       | `string`         | The bookmark's name.                                                                |
| `type`       | `Type \| null`   | The bookmark's type, or `null` if untyped.                                          |
| `in_frame`   | `number`         | The frame the bookmark starts on.                                                   |
| `in_millis`  | `number`         | The replay time at `in_frame`, in milliseconds.                                     |
| `out_frame`  | `number \| null` | The frame the bookmark ends on, or `null` if it is a point.                         |
| `out_millis` | `number \| null` | The replay time at `out_frame`, in milliseconds.                                    |
| `keyframe`   | `boolean`        | `true` for a keyframe bookmark.                                                     |

Each type:

| Field   | Type      | Description                                                                               |
|---------|-----------|-------------------------------------------------------------------------------------------|
| `id`    | `string`  | 16 hex digits.                                                                            |
| `name`  | `string`  | The type's name.                                                                          |
| `color` | `string`  | The type's color, as `#RRGGBB`.                                                           |
| `saved` | `boolean` | `false` if the type is only known from the replay (not one of the user's types).          |

Replay times of bookmarks placed at a given `frame` (rather than the current
frame) are estimated.

### delete-bookmark

Delete a bookmark.

Usage:

- `http://127.0.0.1:30158/delete-bookmark?id=ID`

Arguments:

| Argument | Default    | Description          |
|----------|------------|----------------------|
| `id`     | (required) | The bookmark's id.   |

### enumerate-replays

List all replays for the currently loaded ROM as a JSON string array.

Usage:

- `http://127.0.0.1:30158/enumerate-replays`

### go-to-bookmark

Go to a bookmark. A replay must be playing back for this to work.

Usage:

- `http://127.0.0.1:30158/go-to-bookmark?id=ID`
- `http://127.0.0.1:30158/go-to-bookmark?id=ID&point=out`

Arguments:

| Argument | Default    | Description                                                            |
|----------|------------|------------------------------------------------------------------------|
| `id`     | (required) | The bookmark's id.                                                     |
| `point`  | `in`       | `in` to go to the bookmark's in frame, `out` to go to its out frame.  |

### go-to-frame

Go to the desired frame.

Usage:

- `http://127.0.0.1:30158/go-to-frame?frame=FRAME`

Arguments:

| Argument | Default    | Description     |
|----------|------------|-----------------|
| `frame`  | (required) | Frame to go to. |

### increment-counter

Increment a counter. A replay must be recording for this to work.

Usage:

- `http://127.0.0.1:30158/increment-counter?name=NAME`
- `http://127.0.0.1:30158/increment-counter?name=NAME&by=BY`

Arguments:

| Argument | Default    | Description                                                  |
|----------|------------|--------------------------------------------------------------|
| `name`   | (required) | The name of the counter.                                     |
| `by`     | 1          | Amount to increment (or decrement, if negative) the counter. |

### load-replay

Load a replay.

Usage:

- `http://127.0.0.1:30158/load-replay?name=NAME`

Arguments:

| Argument | Default    | Description             |
|----------|------------|-------------------------|
| `name`   | (required) | The name of the replay. |

### load-rom

Load a rom.

Usage:

- `http://127.0.0.1:30158/load-rom?path=PATH`

Arguments:

| Argument | Default    | Description          |
|----------|------------|----------------------|
| `path`   | (required) | The path to the ROM. |

### mark-start

Mark the start of a replay and enables the timer feature. A replay must be
recording for this to work.

Usage:

- `http://127.0.0.1:30158/mark-start`
- `http://127.0.0.1:30158/mark-start?offset=OFFSET`

Arguments:

| Argument | Default | Description                 |
|----------|---------|-----------------------------|
| `offset` | 0       | The offset in milliseconds. |

### mark-end

Mark the end of the replay's timed section (started with [mark-start](#mark-start)).
A replay must be recording for this to work.

Usage:

- `http://127.0.0.1:30158/mark-end`

### set-paused

Set whether or not playback is paused.

Usage:

- `http://127.0.0.1:30158/set-paused?paused=PAUSED`

Arguments:

| Argument | Default    | Description                         |
|----------|------------|-------------------------------------|
| `paused` | (required) | `true` to pause, `false` to unpause |


### set-playback-speed

Set playback speed. This speed setting does not persist, so any speed change
will override this.

Usage:

- `http://127.0.0.1:30158/set-playback-speed?speed=SPEED`

Arguments:

| Argument | Default    | Description                             |
|----------|------------|-----------------------------------------|
| `speed`  | (required) | The speed multiplier (i.e. 1.0 = 100%). |

### stats

Get the current stats in JSON format. All timestamps are in milliseconds and are
unsigned (non-negative) unless otherwise specified.

Usage:

- `http://127.0.0.1:30158/stats`

| Field                  | Type                     | Description                                                                                                         |
|------------------------|--------------------------|---------------------------------------------------------------------------------------------------------------------|
| `time_start`           | `number \| null`         | Time when the timer starts. This is the minimum value of `time_current` before `time_offset` is added.              |
| `time_end`             | `number \| null`         | Time when the timer ends. This is the maximum value of `time_current` before `time_offset` is added.                |
| `time_offset`          | `number \| null`         | Time to add to the timer AFTER clamping between `time_start` and `time_end`.                                        |
| `time_current`         | `number \| null`         | Current timer value. This has `time_offset` pre-added to it, and it is clamped between `time_start` and `time_end`. |
| `total_elapsed_time`   | `number`                 | Total time the core has been running. If in a replay, this is the elapsed time of the replay, instead.              |
| `total_elapsed_frames` | `number`                 | Total number of frames the core has been running. If in a replay, this is the frame counter of the replay, instead. |
| `is_recording`         | `boolean`                | `true` if currently recording a replay, `false` if not.                                                             |
| `is_playing_back`      | `boolean`                | `true` if a replay is loaded for playback (playing, or stopped: see `is_playback_stopped`), `false` if not.          |
| `is_playback_stopped`  | `boolean`                | `true` if the loaded replay is stopped: still loaded (seekable, resumable) but the game runs live under the user.     |
| `is_paused`            | `boolean`                | `true` if the user has manually paused, `false` if not.                                                             |
| `is_playback_finished` | `boolean`                | `true` if the current replay has reached the end, `false` if not (or no replay playing).                            |
| `current_speed`        | `number`                 | The current playback speed multiplier.                                                                              |
| `counters`             | `Record<string, number>` | The current values of all counters in the currently playing/recording replay.                                       |
| `emulation_fps`        | `number`                 | Emulated frames per second over the last second, drawn or not.                                                      |
| `frame_time_ms`        | `number`                 | Average time the emulator spent on one frame recently.                                                              |
| `frame_budget_ms`      | `number`                 | Time one frame may take at the current speed (0 if the emulator does not pace itself).                             |
| `frames_over_budget`   | `number`                 | Frames that took longer than the budget since the last speed change or ROM load.                                    |
| `bookmark_generation`  | `number`                 | Changes whenever the bookmarks (or bookmark types) change; fetch [bookmarks](#bookmarks) when it does.              |

### toggle-range-bookmark

Start a range bookmark at the current frame, or end the range started last at
the current frame. Returns `{"bookmark": Bookmark, "started": boolean}`.

If playback went back before the range's start, the range's ends are swapped.

Usage:

- `http://127.0.0.1:30158/toggle-range-bookmark`
- `http://127.0.0.1:30158/toggle-range-bookmark?name=NAME&type=TYPE`

Arguments (used when starting a range):

| Argument   | Default         | Description                                                           |
|------------|-----------------|-----------------------------------------------------------------------|
| `name`     | a generic name  | The name of the bookmark.                                             |
| `type`     | the active type | The name of the bookmark's type (see [add-bookmark](#add-bookmark)). |
| `type_id`  | (none)          | The id of the bookmark's type, instead of `type`.                     |
| `keyframe` | `false`         | `true` to start the range with a keyframe bookmark.                   |

### update-bookmark

Change a bookmark. Only the arguments given change. Returns the bookmark as JSON.

Usage:

- `http://127.0.0.1:30158/update-bookmark?id=ID&name=NAME`
- `http://127.0.0.1:30158/update-bookmark?id=ID&out=none`

Arguments:

| Argument  | Default    | Description                                                                                         |
|-----------|------------|-----------------------------------------------------------------------------------------------------|
| `id`      | (required) | The bookmark's id.                                                                                  |
| `name`    | (unchanged)| The bookmark's new name.                                                                            |
| `type`    | (unchanged)| The name of the bookmark's type (created if new); `none` makes it untyped.                          |
| `type_id` | (unchanged)| The id of the bookmark's type, instead of `type`.                                                   |
| `frame`   | (unchanged)| The frame the bookmark starts on. Moving a keyframe bookmark makes it an ordinary bookmark.         |
| `out`     | (unchanged)| The frame the bookmark ends on: a frame number, `now` for the current frame, or `none` to remove it.|

### Errors

Bookmark requests that fail return `{"error": "..."}` with status `400` (a bad
argument), `404` (no replay is recording or playing back, or there is no such
bookmark), `409` (not possible right now, for example seeking while recording or
a replay that must be converted first), `500` (saving the bookmarks into the
replay file failed) or `503` (the emulator is busy).

A request to an unknown route (not one of the ones listed above, or `/client.js`)
returns `404` with no body.
