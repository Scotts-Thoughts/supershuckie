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

#ifdef __cplusplus
}
#endif

#endif
