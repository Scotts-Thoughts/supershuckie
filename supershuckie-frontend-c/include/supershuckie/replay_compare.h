#ifndef __SUPERSHUCKIE_REPLAY_COMPARE_H_
#define __SUPERSHUCKIE_REPLAY_COMPARE_H_

#ifdef __cplusplus
extern "C" {
#endif

#include <stdlib.h>
#include <stdint.h>
#include <stdbool.h>

struct SuperShuckieFrontendRaw;

/* ----------------------------------------------------------------------------------------------
 * Compared replays
 *
 * Other replays played beside the loaded (main) one, each on an emulator of its own, kept on the
 * same timer: every tick each compared replay is sent to the moment its start/stop timer reads
 * what the main replay's does (a replay without a start marker uses its replay time instead).
 * Pausing, stopping, seeking, frame stepping and speed changes of the main replay therefore carry
 * over by themselves; when the main replay ends, the compared ones still going play on to their
 * end at its speed. Closing the main replay (or unloading the ROM) closes them all.
 *
 * A compared replay is shown through the compare_refresh_screens / compare_change_video_mode
 * callbacks (see frontend.h) and identified by its id (never 0). Nintendo 3DS replays cannot be
 * compared (only one 3DS game can run at a time).
 *
 * The state as JSON (supershuckie_frontend_replay_compare_state_json):
 *
 *   {
 *     "main_has_timer": true,          (else the main replay's time lines things up)
 *     "main_final_ms": 3600000,        (the main timer's final reading, or null if it never stopped)
 *     "main_timer_ms": 125000,         (what the main timer reads now; negative before it starts)
 *     "overrun": false,                (the main replay has ended; the compared ones are playing on)
 *     "replays": [
 *       {
 *         "id": 1,
 *         "name": "my run",            (the file name without its extension)
 *         "path": "C:\\...\\my run.replay",
 *         "rom_name": "POKEMON CRYSTAL",
 *         "console": "Game Boy Color",
 *         "status": "not_started" | "in_sync" | "catching_up" | "ended" | "error",
 *         "has_timer": true,
 *         "timer_ms": 125010,          (what its timer reads now; its replay time without a timer)
 *         "final_ms": 3590000,         (its timer's final reading, or null)
 *         "replay_ms": 140000,
 *         "total_ms": 3700000,
 *         "fps": 60.0,
 *         "counters": { "Resets": 2 },
 *         "video_scale": 3,
 *         "window_hidden": false
 *       }
 *     ]
 *   }
 * ---------------------------------------------------------------------------------------------- */

/** Results of supershuckie_frontend_replay_compare_add / _add_by_name. */
#define SUPERSHUCKIE_REPLAY_COMPARE_ADDED 0
/** The replay's ROM is not on this computer (as far as the frontend knows): ask for it and call again with rom_path. */
#define SUPERSHUCKIE_REPLAY_COMPARE_NEEDS_ROM 1
/** The replay does not quite match its ROM or emulator: call again with override_errors to add it anyway. */
#define SUPERSHUCKIE_REPLAY_COMPARE_MISMATCH 2
/** It cannot be compared (the message says why). */
#define SUPERSHUCKIE_REPLAY_COMPARE_FAILED 3

/**
 * Compare the replay file at `replay_path` with the main replay. `rom_path` (may be NULL) is the
 * ROM to play it with; without it the loaded ROM and ROMs played before are tried. On success
 * the new replay's id is written to `id_out` (may be NULL) and compare_change_video_mode is
 * called for it; otherwise the message is written to `error`.
 */
int32_t supershuckie_frontend_replay_compare_add(struct SuperShuckieFrontendRaw *frontend, const char *replay_path, const char *rom_path, bool override_errors, uint16_t *id_out, char *error, size_t error_len);

/** Like supershuckie_frontend_replay_compare_add, for the current ROM's replay called `replay_name`. */
int32_t supershuckie_frontend_replay_compare_add_by_name(struct SuperShuckieFrontendRaw *frontend, const char *replay_name, const char *rom_path, bool override_errors, uint16_t *id_out, char *error, size_t error_len);

/** Stop comparing replay `id`. */
void supershuckie_frontend_replay_compare_remove(struct SuperShuckieFrontendRaw *frontend, uint16_t id);

/** Stop comparing every replay. */
void supershuckie_frontend_replay_compare_clear(struct SuperShuckieFrontendRaw *frontend);

/** How many replays are being compared. */
size_t supershuckie_frontend_replay_compare_count(const struct SuperShuckieFrontendRaw *frontend);

/** Changes whenever a replay is added or removed or one's status changes. */
uint64_t supershuckie_frontend_replay_compare_generation(const struct SuperShuckieFrontendRaw *frontend);

/** The state as JSON (see above). Free with supershuckie_string_free. */
char *supershuckie_frontend_replay_compare_state_json(const struct SuperShuckieFrontendRaw *frontend);

/** Display scale of replay `id`'s window. Triggers compare_change_video_mode. */
void supershuckie_frontend_replay_compare_set_video_scale(struct SuperShuckieFrontendRaw *frontend, uint16_t id, uint8_t scale);

/** Remember that replay `id`'s window is hidden (its screen is not handed over while it is). */
void supershuckie_frontend_replay_compare_set_window_hidden(struct SuperShuckieFrontendRaw *frontend, uint16_t id, bool hidden);

#ifdef __cplusplus
}
#endif

#endif
