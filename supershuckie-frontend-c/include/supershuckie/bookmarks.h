#ifndef __SUPERSHUCKIE_BOOKMARKS_H_
#define __SUPERSHUCKIE_BOOKMARKS_H_

#ifdef __cplusplus
extern "C" {
#endif

#include <stdlib.h>
#include <stdint.h>
#include <stdbool.h>

struct SuperShuckieFrontendRaw;

/* ----------------------------------------------------------------------------------------------
 * Replay bookmarks
 *
 * The bookmarks of the replay being recorded or played back. While recording they are written into
 * the recording; during playback, changes are saved into the replay file shortly after they are made
 * (a pre-v5 replay is upgraded on its first change, see SuperShuckieBookmarkResult__NeedsUpgrade).
 *
 * The whole state is exchanged as JSON (supershuckie_frontend_bookmarks_json):
 *
 *   {
 *     "replay": "run-3",                 (null without a replay)
 *     "state": "none" | "recording" | "playback",
 *     "generation": 42,                  (same as supershuckie_frontend_bookmark_generation)
 *     "editable": true,
 *     "needs_upgrade": false,            (the next change upgrades the replay file)
 *     "replay_version": 5,               (null unless playing back)
 *     "problem": null,                   (why not editable, or why the last save failed)
 *     "open_range": null,                (id of the range started by toggle_range and not ended)
 *     "active_type": "9f3c01a2b4c5d6e7", (type given to new bookmarks; null for untyped)
 *     "bookmarks": [
 *       {"id": 7, "name": "Split 1",
 *        "type": {"id": "9f3c01a2b4c5d6e7", "name": "Split", "color": "#B87B0C", "saved": true},  (null for untyped)
 *        "in_frame": 18402, "in_millis": 306700, "out_frame": null, "out_millis": null, "keyframe": true}
 *     ],
 *     "types": [{"id": "...", "name": "...", "color": "#RRGGBB", "saved": true}]
 *   }
 *
 * Types with "saved": false are only known from the replay (not in the user's settings).
 *
 * Requests (add, update, toggle range) are JSON objects with optional fields, the same as the REST
 * API's parameters:
 *
 *   {"name": "Death", "type": "Deaths", "type_id": "9f3c01a2b4c5d6e7", "frame": 1200,
 *    "out": "now" | "none" | 1300, "keyframe": false}
 *
 * "type" names a type (created if new; "none" for untyped), "type_id" picks one by id. Leaving out
 * "frame" places the bookmark at the current frame; leaving out "name" gives a generic one; leaving
 * out the type gives the active type. "keyframe" is only allowed without "frame".
 * ------------------------------------------------------------------------------------------- */

enum SuperShuckieBookmarkResult {
    /** Done; the output buffer holds the resulting JSON (empty for delete). */
    SuperShuckieBookmarkResult__Ok = 0,
    /** Failed; the output buffer holds the message. */
    SuperShuckieBookmarkResult__Error = 1,
    /**
     * The change would upgrade the replay file to format v5 (older builds cannot open it); the
     * output buffer holds the question to ask. Retry with allow_upgrade = true if the user agrees.
     */
    SuperShuckieBookmarkResult__NeedsUpgrade = 2
};

/** Changes whenever the bookmarks, the replay they belong to, or the bookmark types change. */
uint64_t supershuckie_frontend_bookmark_generation(const struct SuperShuckieFrontendRaw *frontend);

/** The current state as JSON (see above). Free with supershuckie_string_free. */
char *supershuckie_frontend_bookmarks_json(const struct SuperShuckieFrontendRaw *frontend);

/** Add a bookmark. On success `out` receives the bookmark as JSON. Returns a SuperShuckieBookmarkResult. */
uint32_t supershuckie_frontend_bookmark_add_json(struct SuperShuckieFrontendRaw *frontend, const char *request_json, bool allow_upgrade, char *out, size_t out_len);

/**
 * Change a bookmark: only the fields given change. Moving "frame" makes a keyframe bookmark an
 * ordinary one; "out": "none" removes the out frame. On success `out` receives the bookmark as JSON.
 */
uint32_t supershuckie_frontend_bookmark_update_json(struct SuperShuckieFrontendRaw *frontend, uint64_t id, const char *patch_json, bool allow_upgrade, char *out, size_t out_len);

/** Delete a bookmark. */
uint32_t supershuckie_frontend_bookmark_delete(struct SuperShuckieFrontendRaw *frontend, uint64_t id, bool allow_upgrade, char *out, size_t out_len);

/**
 * Start a range bookmark at the current frame, or end the one started last. On success `out`
 * receives {"bookmark": {...}, "started": true|false}.
 */
uint32_t supershuckie_frontend_bookmark_toggle_range_json(struct SuperShuckieFrontendRaw *frontend, const char *request_json, bool allow_upgrade, char *out, size_t out_len);

/** Seek playback to a bookmark's in frame (or out frame). Fails unless a replay is playing back. */
bool supershuckie_frontend_bookmark_go_to(struct SuperShuckieFrontendRaw *frontend, uint64_t id, bool out_point, char *error, size_t error_len);

/** Save unsaved changes to the replay being played back now (call before exiting). */
bool supershuckie_frontend_bookmark_flush(struct SuperShuckieFrontendRaw *frontend, char *error, size_t error_len);

/** The user's bookmark types, then types only known from the current replay, as a JSON array. Free with supershuckie_string_free. */
char *supershuckie_frontend_bookmark_types_json(const struct SuperShuckieFrontendRaw *frontend);

/**
 * Add or change a bookmark type: {"id": "..." (absent to add), "name": "...", "color": "#RRGGBB"}.
 * Saved to the settings right away. On success `out` receives the type as JSON, otherwise the message.
 */
bool supershuckie_frontend_bookmark_type_upsert_json(struct SuperShuckieFrontendRaw *frontend, const char *type_json, char *out, size_t out_len);

/** Remove a bookmark type (hex id) from the user's types. Bookmarks keep the name and color recorded in their replay. */
bool supershuckie_frontend_bookmark_type_delete(struct SuperShuckieFrontendRaw *frontend, const char *type_id, char *error, size_t error_len);

/** The type new bookmarks get, as a hex id ("" for untyped). Returns the bytes needed including the NUL. */
size_t supershuckie_frontend_bookmark_active_type(const struct SuperShuckieFrontendRaw *frontend, char *out, size_t out_len);

/** Set the type new bookmarks get ("" or NULL for untyped). */
bool supershuckie_frontend_set_bookmark_active_type(struct SuperShuckieFrontendRaw *frontend, const char *type_id);

/** Whether to ask before a bookmark change upgrades a pre-v5 replay. */
bool supershuckie_frontend_bookmark_confirm_upgrade(const struct SuperShuckieFrontendRaw *frontend);
void supershuckie_frontend_set_bookmark_confirm_upgrade(struct SuperShuckieFrontendRaw *frontend, bool confirm);

#ifdef __cplusplus
}
#endif

#endif
