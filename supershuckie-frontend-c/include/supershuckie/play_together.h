#ifndef __SUPERSHUCKIE_PLAY_TOGETHER_H_
#define __SUPERSHUCKIE_PLAY_TOGETHER_H_

#ifdef __cplusplus
extern "C" {
#endif

#include <stdlib.h>
#include <stdint.h>
#include <stdbool.h>

struct SuperShuckieFrontendRaw;
struct SuperShuckieAudioOutputRaw;

/* ----------------------------------------------------------------------------------------------
 * Play Together
 *
 * Play alongside other players over the network: one player hosts, the others join with a code
 * ("host:port"). Everyone publishes their own game as a live replay stream and follows everyone
 * else's on an emulator of its own; a friend's game is shown through the peer_refresh_screens /
 * peer_change_video_mode callbacks (see frontend.h) and identified by its peer id (never 0).
 *
 * The whole state is exchanged as JSON (supershuckie_frontend_play_together_state_json):
 *
 *   {
 *     "active": true,
 *     "role": "host" | "client" | "connecting" | "none",
 *     "code": "192.168.1.7:30170",        (what to share, when hosting; what was joined otherwise)
 *     "local_name": "Scott",
 *     "local_peer_id": 1,
 *     "reset_countdown_ms": 0,            (> 0 while a race-start countdown is running)
 *     "save_peer_replays": true,
 *     "participants": [
 *       {"peer_id": 2, "name": "Ash", "rom_name": "crystal.gbc", "console": "Game Boy Color",
 *        "status": "needs_rom" | "starting" | "following" | "waiting" | "resyncing" | "ended" | "error",
 *        "status_text": "",               (why, for needs_rom / error; a note otherwise)
 *        "frames_behind": 3, "waiting": false, "snapshots_applied": 1, "hash_mismatches": 0,
 *        "fps": 239.6, "elapsed_frames": 12345, "elapsed_ms": 205750, "counters": {"deaths": 2},
 *        "replay_file": "Ash - 2026-09-17 20.11.03.replay",   (null when not saved)
 *        "video_scale": 2, "audio": false, "window_hidden": false}
 *     ],
 *     "errors": ["..."]
 *   }
 *
 * supershuckie_frontend_play_together_generation() changes whenever the roster, a participant's
 * status, the countdown or the errors change; the numeric fields (frames_behind, fps, elapsed...)
 * change every frame and are meant to be polled a few times a second.
 *
 * Everything here must be called from the thread that ticks the frontend.
 * ---------------------------------------------------------------------------------------------- */

typedef uint16_t SuperShuckiePeerId;

/**
 * Host a session on `port` (0 = the configured port) as `display_name` (NULL/empty = the saved
 * name), publishing the current game. Writes the code to share into `code_out` (NUL-terminated,
 * truncated to fit; may be NULL). Fails (with the reason in `error`) without a game, with a
 * replay loaded, with a Nintendo DS game, or when already in a session.
 */
bool supershuckie_frontend_play_together_host(struct SuperShuckieFrontendRaw *frontend, uint16_t port, const char *display_name, uint8_t *code_out, size_t code_out_len, uint8_t *error, size_t error_len);

/**
 * Join the session at `code` ("host:port"; a bare host uses the default port) as `display_name`.
 * Returns at once: the outcome shows up in the state (the role becomes "client", or an error is
 * listed and the session ends).
 */
bool supershuckie_frontend_play_together_join(struct SuperShuckieFrontendRaw *frontend, const char *code, const char *display_name, uint8_t *error, size_t error_len);

/** Leave the session (or stop hosting it). The other players' games are closed and their replay files finished. */
void supershuckie_frontend_play_together_leave(struct SuperShuckieFrontendRaw *frontend);

bool supershuckie_frontend_play_together_is_active(const struct SuperShuckieFrontendRaw *frontend);
uint64_t supershuckie_frontend_play_together_generation(const struct SuperShuckieFrontendRaw *frontend);

/** The state as JSON (see above). Free with supershuckie_string_free(). */
char *supershuckie_frontend_play_together_state_json(const struct SuperShuckieFrontendRaw *frontend);

/** Host only: every participant (this one included) hard-resets its console after `countdown_seconds` (at most 60). */
bool supershuckie_frontend_play_together_reset_all(struct SuperShuckieFrontendRaw *frontend, uint32_t countdown_seconds, uint8_t *error, size_t error_len);

/** Milliseconds until the race-start reset fires; 0 when no countdown is running. */
uint32_t supershuckie_frontend_play_together_reset_countdown_ms(const struct SuperShuckieFrontendRaw *frontend);

/**
 * Use the ROM file at `path` for `peer` (whose status is "needs_rom"). Fails if it is not the same
 * ROM they are playing (its blake3 differs); the error says which hashes were compared.
 */
bool supershuckie_frontend_play_together_locate_rom(struct SuperShuckieFrontendRaw *frontend, SuperShuckiePeerId peer, const char *path, uint8_t *error, size_t error_len);

/** Extra ROM paths (a JSON array of strings) to try when looking for another player's ROM by hash, e.g. the UI's favourites. */
void supershuckie_frontend_play_together_add_rom_candidates_json(struct SuperShuckieFrontendRaw *frontend, const char *paths_json);

/** Display scale of `peer`'s window (peer 0 = every window and the default for new ones). Triggers peer_change_video_mode. */
void supershuckie_frontend_play_together_set_video_scale(struct SuperShuckieFrontendRaw *frontend, SuperShuckiePeerId peer, uint8_t scale);
uint8_t supershuckie_frontend_play_together_get_video_scale(const struct SuperShuckieFrontendRaw *frontend);

/** Hear (or stop hearing) `peer`'s game; read its ring with supershuckie_frontend_play_together_retain_peer_audio_output(). */
bool supershuckie_frontend_play_together_set_peer_audio_enabled(struct SuperShuckieFrontendRaw *frontend, SuperShuckiePeerId peer, bool enabled, uint8_t *error, size_t error_len);

/** `peer`'s audio ring (NULL unless enabled); release with supershuckie_audio_output_release(). */
struct SuperShuckieAudioOutputRaw *supershuckie_frontend_play_together_retain_peer_audio_output(const struct SuperShuckieFrontendRaw *frontend, SuperShuckiePeerId peer);

/** Remember whether `peer`'s window is hidden (shown in the state as window_hidden; not persisted). */
void supershuckie_frontend_play_together_set_window_hidden(struct SuperShuckieFrontendRaw *frontend, SuperShuckiePeerId peer, bool hidden);

/** Whether other players' games are written to replay files of their own (applies to players who join from now on). */
bool supershuckie_frontend_play_together_get_save_peer_replays(const struct SuperShuckieFrontendRaw *frontend);
void supershuckie_frontend_play_together_set_save_peer_replays(struct SuperShuckieFrontendRaw *frontend, bool save);

/** The saved display name / host port / last join code, to prefill a dialog. The string getters return the bytes needed including the NUL. */
size_t supershuckie_frontend_play_together_get_display_name(const struct SuperShuckieFrontendRaw *frontend, uint8_t *out, size_t out_len);
uint16_t supershuckie_frontend_play_together_get_host_port(const struct SuperShuckieFrontendRaw *frontend);
size_t supershuckie_frontend_play_together_get_last_join_code(const struct SuperShuckieFrontendRaw *frontend, uint8_t *out, size_t out_len);

/** This machine's network address(es) as a JSON array of strings (the primary one; a router's public address is not known here). Free with supershuckie_string_free(). */
char *supershuckie_frontend_play_together_local_addresses_json(const struct SuperShuckieFrontendRaw *frontend);

#ifdef __cplusplus
}
#endif

#endif
