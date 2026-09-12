#ifndef __SUPERSHUCKIE_MEMORY_H_
#define __SUPERSHUCKIE_MEMORY_H_

#ifdef __cplusplus
extern "C" {
#endif

#include <stdlib.h>
#include <stdint.h>
#include <stdbool.h>

struct SuperShuckieFrontendRaw;

/**
 * RAM tools: viewing, searching, watching, editing and freezing the running game's memory.
 *
 * Addresses are the ones Poke-A-Byte uses (EmulatorCore::read_ram). The frontend services the tools
 * from supershuckie_frontend_tick(); the emulator thread only ever copies what was asked for, at a
 * frame boundary, without waiting for the UI.
 */

/** Viewer windows that can be open at once. */
#define SUPERSHUCKIE_MEMORY_MAX_VIEWERS 4

/** Most bytes one viewer window can sample. */
#define SUPERSHUCKIE_MEMORY_MAX_VIEWER_BYTES 16384

/** Longest value (bytes or text) the tools handle. */
#define SUPERSHUCKIE_MEMORY_MAX_VALUE_SIZE 64

enum SuperShuckieMemoryValueType {
    SuperShuckieMemoryValueType__U8 = 0,
    SuperShuckieMemoryValueType__I8 = 1,
    SuperShuckieMemoryValueType__U16 = 2,
    SuperShuckieMemoryValueType__I16 = 3,
    SuperShuckieMemoryValueType__U32 = 4,
    SuperShuckieMemoryValueType__I32 = 5,
    SuperShuckieMemoryValueType__F32 = 6,
    /** Binary-coded decimal; size is 1-4 bytes (2 digits each). */
    SuperShuckieMemoryValueType__BCD = 7,
    /** Raw bytes; size is 1-64. */
    SuperShuckieMemoryValueType__Bytes = 8,
    /** Text through a character table; size is 1-64. */
    SuperShuckieMemoryValueType__Text = 9
};

enum SuperShuckieMemoryDisplay {
    SuperShuckieMemoryDisplay__Decimal = 0,
    SuperShuckieMemoryDisplay__Hex = 1,
    SuperShuckieMemoryDisplay__Binary = 2
};

struct SuperShuckieMemoryRegion {
    /** Human-readable name. Valid until supershuckie_frontend_memory_regions_generation() changes. */
    const char *name;
    /** Short name for "SHORT:offset" addresses. Same lifetime as name. */
    const char *short_name;
    uint32_t base_address;
    uint32_t length;
    bool default_big_endian;
    bool writable;
};

/**
 * Get the running game's memory regions. Writes up to `capacity` entries to `out` (which may be null
 * when `capacity` is 0) and returns how many regions there are. Zero when no game is loaded.
 */
size_t supershuckie_frontend_memory_get_regions(const struct SuperShuckieFrontendRaw *frontend, struct SuperShuckieMemoryRegion *out, size_t capacity);

/** A number that changes whenever the region list (or the game) changes. */
uint64_t supershuckie_frontend_memory_regions_generation(const struct SuperShuckieFrontendRaw *frontend);

/**
 * Parse an address typed by the user: "0x02024284", "2024284" (hexadecimal), "EWRAM:24284" or
 * "EWRAM+0x24284". On failure a message is written to `error`.
 */
bool supershuckie_frontend_memory_parse_address(const struct SuperShuckieFrontendRaw *frontend, const char *text, uint32_t *address, char *error, size_t error_len);

/**
 * Format an address ("0x02024284", or "EWRAM:24284" when `region_relative`). Returns the bytes
 * needed including the NUL.
 */
size_t supershuckie_frontend_memory_format_address(const struct SuperShuckieFrontendRaw *frontend, uint32_t address, bool region_relative, char *out, size_t out_len);

/** Samples per second the tools refresh at while the game runs (default 30). */
uint8_t supershuckie_frontend_memory_get_refresh_rate(const struct SuperShuckieFrontendRaw *frontend);
void supershuckie_frontend_memory_set_refresh_rate(struct SuperShuckieFrontendRaw *frontend, uint8_t hz);

/**
 * Set what viewer slot `viewer` (0 to SUPERSHUCKIE_MEMORY_MAX_VIEWERS - 1) shows. Disable it while the
 * viewer is closed or hidden so nothing is copied for it.
 */
void supershuckie_frontend_memory_set_viewer_window(struct SuperShuckieFrontendRaw *frontend, uint8_t viewer, bool enabled, uint32_t address, uint32_t length);

/**
 * If a sample newer than `*generation` exists for viewer slot `viewer`, copy its bytes (up to
 * `capacity`) to `bytes`, update `*generation` and return true. `*address` is the first byte's
 * address (it may lag behind the window just set), `*length` the bytes copied and `*valid_length`
 * how many of them are mapped memory. Null out-parameters are skipped (except `generation`, which
 * is then treated as 0).
 */
bool supershuckie_frontend_memory_read_viewer(
    const struct SuperShuckieFrontendRaw *frontend,
    uint8_t viewer,
    uint64_t *generation,
    uint64_t *frame,
    uint32_t *address,
    uint8_t *bytes,
    uint32_t capacity,
    uint32_t *length,
    uint32_t *valid_length
);

/** Number of character tables loaded (table 0 is always ASCII). */
size_t supershuckie_frontend_memory_table_count(const struct SuperShuckieFrontendRaw *frontend);

/** Name of table `table`, or null. Valid until the tables are reloaded. */
const char *supershuckie_frontend_memory_table_name(const struct SuperShuckieFrontendRaw *frontend, size_t table);

/**
 * Reload the character tables from the tables directory (every *.tbl file). Returns false and
 * describes the files that failed to load in `error` if any did (the others are still loaded).
 */
bool supershuckie_frontend_memory_reload_tables(struct SuperShuckieFrontendRaw *frontend, char *error, size_t error_len);

/** The directory character tables are loaded from. Returns the bytes needed including the NUL. */
size_t supershuckie_frontend_memory_tables_directory(const struct SuperShuckieFrontendRaw *frontend, char *out, size_t out_len);

/** What `byte` stands for in table `table` (UTF-8), or false if nothing. */
bool supershuckie_frontend_memory_table_glyph(const struct SuperShuckieFrontendRaw *frontend, size_t table, uint8_t byte, char *out, size_t out_len);

/**
 * Format `length` bytes as a value (see SuperShuckieMemoryValueType; `size` is ignored for fixed-size
 * types). Returns the bytes needed including the NUL.
 */
size_t supershuckie_frontend_memory_format_value(
    const struct SuperShuckieFrontendRaw *frontend,
    size_t table,
    uint32_t value_type,
    uint8_t size,
    bool big_endian,
    uint32_t display,
    const uint8_t *bytes,
    size_t length,
    char *out,
    size_t out_len
);

/**
 * Parse text typed by the user into the bytes of a value. Numbers accept decimal, 0x/$ hexadecimal
 * and 0b binary, range-checked for the size. On success the bytes are written to `out` (up to
 * `capacity`) and their count to `*out_length`; on failure a message is written to `error`.
 */
bool supershuckie_frontend_memory_parse_value(
    const struct SuperShuckieFrontendRaw *frontend,
    size_t table,
    uint32_t value_type,
    uint8_t size,
    bool big_endian,
    const char *text,
    uint8_t *out,
    size_t capacity,
    size_t *out_length,
    char *error,
    size_t error_len
);

/** Parse hexadecimal bytes ("12 34 AB", "1234AB", "0x12, 0x34"). */
bool supershuckie_memory_parse_hex_bytes(const char *text, uint8_t *out, size_t capacity, size_t *out_length, char *error, size_t error_len);


/* ----------------------------------------------------------------------------------------------
 * RAM search
 *
 * A search scans a snapshot of every memory region taken at a frame boundary, then narrows its
 * results with further scans. Scans run on a worker thread; poll supershuckie_frontend_search_status().
 * ------------------------------------------------------------------------------------------- */

enum SuperShuckieSearchComparison {
    /* Compared with a value (operand_a; Between also uses operand_b; InSet takes a comma-separated list). */
    SuperShuckieSearchComparison__Equal = 0,
    SuperShuckieSearchComparison__NotEqual = 1,
    SuperShuckieSearchComparison__Less = 2,
    SuperShuckieSearchComparison__LessOrEqual = 3,
    SuperShuckieSearchComparison__Greater = 4,
    SuperShuckieSearchComparison__GreaterOrEqual = 5,
    SuperShuckieSearchComparison__Between = 6,
    SuperShuckieSearchComparison__InSet = 7,
    /* Every aligned address (new searches only). */
    SuperShuckieSearchComparison__Unknown = 8,
    /* Bytes: a pattern like "12 ?? 3F" in operand_a. Text: the text in operand_a (through the table). */
    SuperShuckieSearchComparison__Pattern = 9,
    /* Compared with the previous scan (refinements only). */
    SuperShuckieSearchComparison__Changed = 10,
    SuperShuckieSearchComparison__Unchanged = 11,
    SuperShuckieSearchComparison__Increased = 12,
    SuperShuckieSearchComparison__Decreased = 13,
    SuperShuckieSearchComparison__IncreasedBy = 14,
    SuperShuckieSearchComparison__DecreasedBy = 15,
    SuperShuckieSearchComparison__ChangedBy = 16,
    SuperShuckieSearchComparison__ChangedByAtLeast = 17,
    /* Compared with the first scan (refinements only). */
    SuperShuckieSearchComparison__EqualToFirst = 18,
    SuperShuckieSearchComparison__NotEqualToFirst = 19,
    SuperShuckieSearchComparison__IncreasedSinceFirst = 20,
    SuperShuckieSearchComparison__DecreasedSinceFirst = 21
};

struct SuperShuckieSearchParams {
    uint32_t value_type;
    /** BCD: 1-4 bytes. Bytes and text: taken from the pattern for pattern searches. */
    uint8_t size;
    bool big_endian;
    /** 1, 2 or 4. */
    uint8_t alignment;
    /** Character table for text searches. */
    size_t table;
    /** Region indices to search; none (null or 0) searches all. */
    const uint32_t *regions;
    size_t region_count;
    /** Only addresses in [range_start, range_end). */
    bool use_range;
    uint32_t range_start;
    uint32_t range_end;
    /** f32 values within this of each other are equal (0 for the default, 0.01). */
    double epsilon;
};

struct SuperShuckieSearchStatus {
    bool active;
    bool busy;
    uint32_t progress_per_mille;
    uint64_t result_count;
    uint32_t steps;
    bool can_undo;
    bool can_redo;
    /** Frame of the last scan. */
    uint64_t frame;
    /** Memory was replaced wholesale (a state load or seek) between the last two scans. */
    bool state_changed;
    /** Changes whenever the results change. */
    uint64_t generation;
    /** The active search's value type, size, byte order and alignment. */
    uint32_t value_type;
    uint8_t size;
    bool big_endian;
    uint8_t alignment;
};

struct SuperShuckieSearchRow {
    uint32_t address;
    uint32_t region;
    uint8_t length;
    /** The value at the last scan. */
    uint8_t previous[64];
    /** The value at the first scan. */
    uint8_t first[64];
};

/** Start a new search (replacing any other) at the next frame boundary. `pause` pauses emulation until it is done. */
bool supershuckie_frontend_search_new(
    struct SuperShuckieFrontendRaw *frontend,
    const struct SuperShuckieSearchParams *params,
    uint32_t comparison,
    const char *operand_a,
    const char *operand_b,
    bool pause,
    char *error,
    size_t error_len
);

/** Narrow the active search at the next frame boundary. */
bool supershuckie_frontend_search_refine(
    struct SuperShuckieFrontendRaw *frontend,
    uint32_t comparison,
    const char *operand_a,
    const char *operand_b,
    size_t table,
    bool pause,
    char *error,
    size_t error_len
);

/** The search's status. Returns true (and writes it to `message`) if the last operation left a message. */
bool supershuckie_frontend_search_status(const struct SuperShuckieFrontendRaw *frontend, struct SuperShuckieSearchStatus *status, char *message, size_t message_len);

/** Up to `capacity` results starting with the `offset`th. Returns how many were written (0 while scanning). */
size_t supershuckie_frontend_search_results(const struct SuperShuckieFrontendRaw *frontend, uint64_t offset, struct SuperShuckieSearchRow *out, size_t capacity);

/** Which result rows are on screen, so their current values are sampled (at most 512). */
void supershuckie_frontend_search_set_visible_rows(struct SuperShuckieFrontendRaw *frontend, uint64_t offset, uint32_t count);

/**
 * The current values of the visible rows: `values` holds `capacity` * 64 bytes (row i at i * 64),
 * `ok[i]` whether row i's value is known. Returns the number of visible rows; `*first_row` is the
 * first visible row and `*generation` changes with every sample.
 */
size_t supershuckie_frontend_search_read_visible(const struct SuperShuckieFrontendRaw *frontend, uint64_t *first_row, uint64_t *generation, uint8_t *values, bool *ok, size_t capacity);

void supershuckie_frontend_search_undo(const struct SuperShuckieFrontendRaw *frontend);
void supershuckie_frontend_search_redo(const struct SuperShuckieFrontendRaw *frontend);
void supershuckie_frontend_search_cancel(const struct SuperShuckieFrontendRaw *frontend);
void supershuckie_frontend_search_reset(struct SuperShuckieFrontendRaw *frontend);


/* ----------------------------------------------------------------------------------------------
 * RAM watch
 *
 * Watches are exchanged as JSON objects:
 *
 *   {
 *     "id": 3,                                   (0 when adding)
 *     "label": "Money",
 *     "address": {"base": "0x02024284", "offsets": [12]},   (offsets: pointer path, optional)
 *     "format": {"type": "u16", "size": 2, "big_endian": false},
 *     "display": "decimal" | "hex" | "binary",
 *     "table": "",                               (character table name for text)
 *     "group": "", "notes": "",
 *     "trace": false,                            (log every change, frame-accurately)
 *     "pause_when": {"when": "equals", "value": 5},   (optional; "changes" has no value)
 *     "freeze": {"value": "63 00", "active": true}     (optional)
 *   }
 *
 * The list is saved per ROM (ram-watch.json in the ROM's data directory); freezes always load inactive.
 * ------------------------------------------------------------------------------------------- */

enum SuperShuckieWatchLogKind {
    SuperShuckieWatchLogKind__Changed = 0,
    SuperShuckieWatchLogKind__Discontinuity = 1,
    SuperShuckieWatchLogKind__Paused = 2,
    SuperShuckieWatchLogKind__Edited = 3,
    SuperShuckieWatchLogKind__EditFailed = 4
};

struct SuperShuckieWatchValue {
    uint32_t id;
    /** The value could be read. */
    bool ok;
    /** The address resolved (pointers included). */
    bool resolved;
    uint32_t resolved_address;
    /** Frames since the value last changed (UINT64_MAX when not seen changing). */
    uint64_t frames_since_change;
    uint8_t length;
    uint8_t value[64];
    char text[128];
    char previous_text[128];
};

struct SuperShuckieWatchLogEntry {
    uint64_t frame;
    uint32_t watch_id;
    uint32_t kind;
    char text[256];
};

/** Whether any watch logs changes or pauses (so the UI should keep polling while its windows are hidden). */
bool supershuckie_frontend_memory_has_traces(const struct SuperShuckieFrontendRaw *frontend);

/** Free a string that a function says must be freed with this. */
void supershuckie_string_free(char *string);

/** The watch list as a JSON array. Free with supershuckie_string_free(). */
char *supershuckie_frontend_watch_list_json(const struct SuperShuckieFrontendRaw *frontend);

/** Changes whenever the watch list changes. */
uint64_t supershuckie_frontend_watch_generation(const struct SuperShuckieFrontendRaw *frontend);

/** Add a watch (id 0) or replace the one with the same id. Returns its id, or 0 with a message in `error`. */
uint32_t supershuckie_frontend_watch_upsert_json(struct SuperShuckieFrontendRaw *frontend, const char *json, char *error, size_t error_len);

void supershuckie_frontend_watch_remove(struct SuperShuckieFrontendRaw *frontend, uint32_t id);

/** The watches on screen (their values are sampled). */
void supershuckie_frontend_watch_set_visible(struct SuperShuckieFrontendRaw *frontend, const uint32_t *ids, size_t count);

/** The latest values of the visible watches. Returns how many were written. */
size_t supershuckie_frontend_watch_read_values(const struct SuperShuckieFrontendRaw *frontend, struct SuperShuckieWatchValue *out, size_t capacity);

/** Take up to `capacity` change log lines (oldest first); `*dropped` counts lines lost meanwhile. */
size_t supershuckie_frontend_watch_drain_log(struct SuperShuckieFrontendRaw *frontend, struct SuperShuckieWatchLogEntry *out, size_t capacity, uint64_t *dropped);

/** Problems found loading or importing watches (cleared when read). Returns false if there are none. */
bool supershuckie_frontend_watch_problems(struct SuperShuckieFrontendRaw *frontend, char *out, size_t out_len);

/** Import a watch list file (replacing the list or adding to it). */
bool supershuckie_frontend_watch_import(struct SuperShuckieFrontendRaw *frontend, const char *path, bool replace, char *error, size_t error_len);

/** Export the watch list to a file. */
bool supershuckie_frontend_watch_export(const struct SuperShuckieFrontendRaw *frontend, const char *path, char *error, size_t error_len);

/** Save the watch list now (it is also saved a second after each change and when the ROM closes). */
void supershuckie_frontend_watch_save(struct SuperShuckieFrontendRaw *frontend);

/** Parse "0x…", "EWRAM:…" or "[…]+off" into address JSON. Free with supershuckie_string_free(); null on error. */
char *supershuckie_frontend_watch_parse_address(const struct SuperShuckieFrontendRaw *frontend, const char *text, char *error, size_t error_len);

/** Format address JSON as text. Returns the bytes needed including the NUL. */
size_t supershuckie_frontend_watch_format_address(const struct SuperShuckieFrontendRaw *frontend, const char *json, char *out, size_t out_len);


/* ----------------------------------------------------------------------------------------------
 * Editing and freezing
 *
 * Edits are applied at the next frame boundary and frozen values are restored after every frame
 * on which the game changed them. Both go through the emulator's recorded write path: while
 * recording they are written into the replay (a freeze only on frames where it had to restore the
 * value). They are refused during replay playback, where freezes are suspended.
 * ------------------------------------------------------------------------------------------- */

/** Whether memory can be written right now; otherwise the reason is written to `reason`. */
bool supershuckie_frontend_memory_can_write(const struct SuperShuckieFrontendRaw *frontend, char *reason, size_t reason_len);

/** Whether to ask the user before the next write (a recording is running and they have not confirmed yet). */
bool supershuckie_frontend_memory_needs_record_confirmation(const struct SuperShuckieFrontendRaw *frontend);

/** The user agreed to write into the current recording. `dont_ask_again` turns the question off. */
void supershuckie_frontend_memory_confirm_record_writes(struct SuperShuckieFrontendRaw *frontend, bool dont_ask_again);

bool supershuckie_frontend_memory_get_confirm_writes_while_recording(const struct SuperShuckieFrontendRaw *frontend);
void supershuckie_frontend_memory_set_confirm_writes_while_recording(struct SuperShuckieFrontendRaw *frontend, bool confirm);

/** Edits and freeze restores written into the current recording (0 when not recording). */
uint64_t supershuckie_frontend_memory_writes_this_recording(const struct SuperShuckieFrontendRaw *frontend);

/**
 * Write `length` bytes (at most 4096) at `address`, reached through `offset_count` pointer offsets
 * (null/0 for a plain address). Writing exactly over an active freeze changes the frozen value.
 * The result arrives later: failures show up in supershuckie_frontend_memory_edit_message().
 */
bool supershuckie_frontend_memory_write(struct SuperShuckieFrontendRaw *frontend, uint32_t address, const int32_t *offsets, size_t offset_count, const uint8_t *data, size_t length, char *error, size_t error_len);

/**
 * Freeze a value at `address` (with pointer offsets) as a watch in `group` (the existing watch for
 * that address and size is used if there is one). Returns the watch id, or 0 with a message.
 */
uint32_t supershuckie_frontend_memory_freeze(
    struct SuperShuckieFrontendRaw *frontend,
    uint32_t address,
    const int32_t *offsets,
    size_t offset_count,
    uint32_t value_type,
    uint8_t size,
    bool big_endian,
    const uint8_t *value,
    size_t length,
    const char *group,
    char *error,
    size_t error_len
);

/** Freeze watch `id` at `value` (null: the value it was last frozen at) or unfreeze it. */
bool supershuckie_frontend_watch_set_frozen(struct SuperShuckieFrontendRaw *frontend, uint32_t id, bool frozen, const uint8_t *value, size_t length, char *error, size_t error_len);

void supershuckie_frontend_memory_unfreeze_all(struct SuperShuckieFrontendRaw *frontend);

uint32_t supershuckie_frontend_memory_frozen_count(const struct SuperShuckieFrontendRaw *frontend);

/** Addresses and lengths of active freezes (for highlighting). Returns how many (with null arrays: the total). */
size_t supershuckie_frontend_memory_frozen_ranges(const struct SuperShuckieFrontendRaw *frontend, uint32_t *starts, uint32_t *lengths, size_t capacity);

/** How many frames watch `id`'s freeze had to restore its value on, and whether its address resolves. False if not frozen. */
bool supershuckie_frontend_watch_freeze_status(const struct SuperShuckieFrontendRaw *frontend, uint32_t id, uint32_t *restores, bool *resolved);

bool supershuckie_frontend_memory_can_undo(const struct SuperShuckieFrontendRaw *frontend);
bool supershuckie_frontend_memory_can_redo(const struct SuperShuckieFrontendRaw *frontend);

/** Undo/redo the last edit or freeze change (one history shared by all tool windows). */
bool supershuckie_frontend_memory_undo(struct SuperShuckieFrontendRaw *frontend, char *error, size_t error_len);
bool supershuckie_frontend_memory_redo(struct SuperShuckieFrontendRaw *frontend, char *error, size_t error_len);

/** A message about an edit that failed after it was sent (cleared when read). */
bool supershuckie_frontend_memory_edit_message(struct SuperShuckieFrontendRaw *frontend, char *out, size_t out_len);

#ifdef __cplusplus
}
#endif

#endif
