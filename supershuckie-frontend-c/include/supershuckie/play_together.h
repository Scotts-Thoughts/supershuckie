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
 *     "sync_pause": false,                (whether one player's pause pauses everyone: the host's setting in a session)
 *     "paused_by": "",                    (who paused everyone while sync pause holds the game paused: a name, "you", or empty)
 *     "start_state": false,               (whether the host's start state is set: everyone's game was loaded from it)
 *     "link": {                           (the link cable; see below)
 *       "phase": "none" | "requesting" | "incoming" | "starting" | "linked",
 *       "peer_id": 2, "peer_name": "Ash", "nonce": 7, "input_delay": 3, "stalled": false,
 *       "since_ms": 1200, "link_frame": 3600, "last_reason": ""
 *     },
 *     "participants": [
 *       {"peer_id": 2, "name": "Ash", "rom_name": "crystal.gbc", "console": "Game Boy Color",
 *        "status": "needs_rom" | "starting" | "following" | "waiting" | "resyncing" | "ended" | "error",
 *        "status_text": "",               (why, for needs_rom / error; a note otherwise)
 *        "frames_behind": 3, "waiting": false, "snapshots_applied": 1, "hash_mismatches": 0,
 *        "fps": 239.6, "elapsed_frames": 12345, "elapsed_ms": 205750, "counters": {"deaths": 2},
 *        "replay_file": "Ash - 2026-09-17 20.11.03.replay",   (null when not saved)
 *        "video_scale": 2, "audio": false, "window_hidden": false,
 *        "pokeabyte_port": 55357,          (the UDP port their game is served to Poke-A-Byte on; null while it is not)
 *        "linked_with": null,              (who they are linked with by cable, or null)
 *        "can_link": true}                 (whether a link cable can be plugged into their game right now)
 *     ],
 *     "errors": ["..."]
 *   }
 *
 * supershuckie_frontend_play_together_generation() changes whenever the roster, a participant's
 * status, the countdown, sync pause, the start state, the link cable's phase or the errors change;
 * the numeric fields (frames_behind, fps, elapsed...) change every frame and are meant to be
 * polled a few times a second.
 *
 * Everything here must be called from the thread that ticks the frontend.
 * ---------------------------------------------------------------------------------------------- */

typedef uint16_t SuperShuckiePeerId;

/**
 * Player colours: `color` values are 1-based indices into a fixed palette; 0 means "let the host
 * pick". Every participant in a session has a different colour. `supershuckie_play_together_color`
 * gives entry `color`'s 0xRRGGBB and name (returns false for 0 or out of range).
 */
uint8_t supershuckie_play_together_color_count(void);
bool supershuckie_play_together_color(uint8_t color, uint32_t *rgb_out, uint8_t *name_out, size_t name_out_len);

/**
 * Host a session on `port` (0 = the configured port) as `display_name` (NULL/empty = the saved
 * name) in `color` (0 = any), publishing the current game. Writes the code to share into
 * `code_out` (NUL-terminated, truncated to fit; may be NULL). Fails (with the reason in `error`)
 * without a game, with a replay loaded, with a Nintendo DS game, or when already in a session.
 */
bool supershuckie_frontend_play_together_host(struct SuperShuckieFrontendRaw *frontend, uint16_t port, const char *display_name, uint8_t color, uint8_t *code_out, size_t code_out_len, uint8_t *error, size_t error_len);

/**
 * Join the session at `code` ("host:port"; a bare host uses the default port) as `display_name`
 * in `color` (0 = any; the host gives another when it is taken). Returns at once: the outcome
 * shows up in the state (the role becomes "client", or an error is listed and the session ends).
 */
bool supershuckie_frontend_play_together_join(struct SuperShuckieFrontendRaw *frontend, const char *code, const char *display_name, uint8_t color, uint8_t *error, size_t error_len);

/** Leave the session (or stop hosting it). The other players' games are closed and their replay files finished. */
void supershuckie_frontend_play_together_leave(struct SuperShuckieFrontendRaw *frontend);

bool supershuckie_frontend_play_together_is_active(const struct SuperShuckieFrontendRaw *frontend);
uint64_t supershuckie_frontend_play_together_generation(const struct SuperShuckieFrontendRaw *frontend);

/** The state as JSON (see above). Free with supershuckie_string_free(). */
char *supershuckie_frontend_play_together_state_json(const struct SuperShuckieFrontendRaw *frontend);

/**
 * Host only: every participant (this one included) hard-resets its console after `countdown_seconds`
 * (at most 60), or, while the host's start state is set, loads that state instead. Everyone unpauses.
 */
bool supershuckie_frontend_play_together_reset_all(struct SuperShuckieFrontendRaw *frontend, uint32_t countdown_seconds, uint8_t *error, size_t error_len);

/** Milliseconds until the race-start reset fires; 0 when no countdown is running. */
uint32_t supershuckie_frontend_play_together_reset_countdown_ms(const struct SuperShuckieFrontendRaw *frontend);

/**
 * Sync pause: when anyone pauses, everyone's game pauses (and unpauses). The host's setting applies
 * to the whole session: in a session the getter gives the host's, and only the host can set it (a
 * client gets an error); outside one it is the local setting used when hosting. Turning it on makes
 * everyone adopt the host's current pause state.
 */
bool supershuckie_frontend_play_together_get_sync_pause(const struct SuperShuckieFrontendRaw *frontend);
bool supershuckie_frontend_play_together_set_sync_pause(struct SuperShuckieFrontendRaw *frontend, bool enabled, uint8_t *error, size_t error_len);

/**
 * Start state: everyone starts from the host's save state. Setting it (host only) pauses the host's
 * game, takes its save state and has every participant's own game load it (paused); a player who
 * joins afterwards gets it too, and one whose console, ROM, core or BIOS differs from the host's is
 * refused. It fails while someone already in the session plays a different game. While it is set,
 * "reset everyone" loads the state again instead of resetting consoles. Clearing it changes nobody's
 * game. The getter says whether one is set (for a client: whether the host set one).
 */
bool supershuckie_frontend_play_together_get_start_state(const struct SuperShuckieFrontendRaw *frontend);
bool supershuckie_frontend_play_together_set_start_state(struct SuperShuckieFrontendRaw *frontend, bool enabled, uint8_t *error, size_t error_len);

/**
 * Link cable: plug a cable between this player's game and `peer`'s, so the two can trade and battle
 * (Game Boy / Game Boy Color / Game Boy Advance games of the same family; `peer` must be followed
 * here and in sync: "can_link" in the state). The other player is asked; the answer shows up in the
 * state's "link" object: "requesting" until they answer, then "starting" (both games hold while
 * the cores plug in) and "linked", or back to "none" with "last_reason" set. While linked both games
 * run at 1x with an input delay of "input_delay" frames; pausing either pauses both ("stalled" is
 * set while waiting for the other side); save-state loads, replays, exports and core reloads are
 * refused ("Unplug the link cable first."); resets go through, delayed like inputs.
 *
 * An incoming request puts "incoming" in the state with the other player's "peer_name" and the
 * request's "nonce": answer with supershuckie_frontend_play_together_link_respond (accept plugs in).
 * Unanswered requests expire after 30 seconds. supershuckie_frontend_play_together_unlink pulls the
 * cable (or withdraws / declines a pending request); the other player is told, and both games go on
 * on their own. A departure, a desync or a stall pulls it too, with "last_reason" saying why.
 *
 * The input delay setting (0 = automatic from the round-trip times, else 1..15 frames) applies to
 * the next cable; the larger of the two players' settings wins.
 */
bool supershuckie_frontend_play_together_link_request(struct SuperShuckieFrontendRaw *frontend, SuperShuckiePeerId peer, uint8_t *error, size_t error_len);
bool supershuckie_frontend_play_together_link_respond(struct SuperShuckieFrontendRaw *frontend, uint32_t nonce, bool accept, uint8_t *error, size_t error_len);
void supershuckie_frontend_play_together_unlink(struct SuperShuckieFrontendRaw *frontend);
bool supershuckie_frontend_play_together_is_link_cable_plugged(const struct SuperShuckieFrontendRaw *frontend);
/** The state's "link" object alone, as JSON. Free with supershuckie_string_free(). */
char *supershuckie_frontend_play_together_link_state_json(const struct SuperShuckieFrontendRaw *frontend);
uint8_t supershuckie_frontend_play_together_get_link_input_delay(const struct SuperShuckieFrontendRaw *frontend);
void supershuckie_frontend_play_together_set_link_input_delay(struct SuperShuckieFrontendRaw *frontend, uint8_t frames);

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

/**
 * Serve (or stop serving) `peer`'s game to Poke-A-Byte on the lowest free UDP port above the player's
 * own (supershuckie_frontend_get_pokeabyte_port()), whatever the "serve friends" setting says. The
 * port is shown as pokeabyte_port in the state.
 */
bool supershuckie_frontend_play_together_set_peer_pokeabyte_enabled(struct SuperShuckieFrontendRaw *frontend, SuperShuckiePeerId peer, bool enabled, uint8_t *error, size_t error_len);

/** The port `peer`'s game is served to Poke-A-Byte on, or 0 while it is not. */
uint16_t supershuckie_frontend_play_together_get_peer_pokeabyte_port(const struct SuperShuckieFrontendRaw *frontend, SuperShuckiePeerId peer);

/** `peer`'s audio ring (NULL unless enabled); release with supershuckie_audio_output_release(). */
struct SuperShuckieAudioOutputRaw *supershuckie_frontend_play_together_retain_peer_audio_output(const struct SuperShuckieFrontendRaw *frontend, SuperShuckiePeerId peer);

/** Remember whether `peer`'s window is hidden (shown in the state as window_hidden; not persisted). */
void supershuckie_frontend_play_together_set_window_hidden(struct SuperShuckieFrontendRaw *frontend, SuperShuckiePeerId peer, bool hidden);

/** Whether other players' games are written to replay files of their own (applies to players who join from now on). */
bool supershuckie_frontend_play_together_get_save_peer_replays(const struct SuperShuckieFrontendRaw *frontend);
void supershuckie_frontend_play_together_set_save_peer_replays(struct SuperShuckieFrontendRaw *frontend, bool save);

/** The saved display name / colour / host port / last join code, to prefill a dialog. The string getters return the bytes needed including the NUL. */
size_t supershuckie_frontend_play_together_get_display_name(const struct SuperShuckieFrontendRaw *frontend, uint8_t *out, size_t out_len);
uint8_t supershuckie_frontend_play_together_get_color(const struct SuperShuckieFrontendRaw *frontend);
uint16_t supershuckie_frontend_play_together_get_host_port(const struct SuperShuckieFrontendRaw *frontend);
size_t supershuckie_frontend_play_together_get_last_join_code(const struct SuperShuckieFrontendRaw *frontend, uint8_t *out, size_t out_len);

/** This machine's network address(es) as a JSON array of strings (the primary one; a router's public address is not known here). Free with supershuckie_string_free(). */
char *supershuckie_frontend_play_together_local_addresses_json(const struct SuperShuckieFrontendRaw *frontend);

#ifdef __cplusplus
}
#endif

#endif
