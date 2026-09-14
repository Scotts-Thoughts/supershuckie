/*!
 * client.d.ts
 *
 * Copyright 2026 SnowyMouse
 *
 * The Super Shuckie JavaScript API (client.js and client.d.ts) are licensed
 * under version 3 of the GNU GPL, just like the rest of Super Shuckie.
 *
 * HOWEVER, you may alternatively use, copy, link with, and/or distribute
 * the Super Shuckie JavaScript API under the terms of Version 2.0 of the
 * Apache License, obtainable at http://www.apache.org/licenses/LICENSE-2.0
 */

/**
 * Client interface
 */
export class SuperShuckieClient {
    /**
     * Create a client for the given server
     * @param server to use (default = "http://127.0.0.1:30158")
     */
    constructor(server?: string)

    /**
     * Mark the start of a replay
     * @param offset offset in milliseconds (default = 0)
     */
    mark_start(offset?: number): Promise<void>

    /**
     * Mark the end of a replay
     */
    mark_end(): Promise<void>

    /**
     * Increment/decrement the given counter
     * @param name name of counter
     * @param by amount to increment or decrement (default = 1)
     */
    increment_counter(name: string, by?: number): Promise<void>

    /**
     * Get the stats
     */
    stats(): Promise<SuperShuckieStats>

    /**
     * List all replays
     */
    enumerate_replays(): Promise<string[]>

    /**
     * Load the replay
     * @param name name of replay
     */
    load_replay(name: string): Promise<void>

    /**
     * Load the rom at the given path
     * @param path path of ROM
     */
    load_rom(path: string): Promise<void>

    /**
     * Set the speed
     * @param speed speed multiplayer (1 = 100%, 2 = 200%, 0.5 = 50%, etc.)
     */
    set_playback_speed(speed: number): Promise<void>

    /**
     * Go to the given frame index in a replay
     * @param frame frame index
     */
    go_to_frame(frame: number): Promise<void>

    /**
     * Set whether or not playback is paused.
     * @param paused if true, pause. otherwise, unpause
     */
    set_paused(paused: boolean): Promise<void>

    /**
     * Get the bookmarks of the replay being recorded or played back
     */
    bookmarks(): Promise<SuperShuckieBookmarks>

    /**
     * Add a bookmark (at the current frame unless a frame is given)
     * @param options name, type, frame, out and keyframe (all optional)
     */
    add_bookmark(options?: SuperShuckieBookmarkOptions): Promise<SuperShuckieBookmark>

    /**
     * Change a bookmark; only the options given change
     * @param id bookmark id
     * @param options name, type, frame and out (out: "none" removes it)
     */
    update_bookmark(id: number, options?: SuperShuckieBookmarkOptions): Promise<SuperShuckieBookmark>

    /**
     * Delete a bookmark
     * @param id bookmark id
     */
    delete_bookmark(id: number): Promise<void>

    /**
     * Start a range bookmark at the current frame, or end the one started last
     * @param options name, type and keyframe for a new range (all optional)
     */
    toggle_range_bookmark(options?: SuperShuckieBookmarkOptions): Promise<{ bookmark: SuperShuckieBookmark, started: boolean }>

    /**
     * Seek playback to a bookmark (playback only)
     * @param id bookmark id
     * @param point "in" (default) or "out"
     */
    go_to_bookmark(id: number, point?: "in" | "out"): Promise<void>
}

/**
 * Options for adding and changing bookmarks (see external_commands.md)
 */
export interface SuperShuckieBookmarkOptions {
    /** Name; a generic one is used when adding without it */
    name?: string,
    /** Type name (created if new); "none" for untyped */
    type?: string,
    /** Type id (hex), instead of type */
    type_id?: string,
    /** In frame; the current frame when adding without it */
    frame?: number,
    /** Out frame: a frame, "now", or "none" to remove it */
    out?: number | "now" | "none",
    /** Place a keyframe bookmark (adding only, without frame) */
    keyframe?: boolean
}

/**
 * A bookmark type
 */
export interface SuperShuckieBookmarkType {
    /** 16 hex digits */
    id: string,
    name: string,
    /** "#RRGGBB" */
    color: string,
    /** false if the type is only known from the replay, not the user's settings */
    saved: boolean
}

/**
 * A bookmark
 */
export interface SuperShuckieBookmark {
    id: number,
    name: string,
    type: SuperShuckieBookmarkType | null,
    in_frame: number,
    in_millis: number,
    out_frame: number | null,
    out_millis: number | null,
    keyframe: boolean
}

/**
 * The current replay's bookmarks (see external_commands.md)
 */
export interface SuperShuckieBookmarks {
    replay: string | null,
    state: "none" | "recording" | "playback",
    generation: number,
    editable: boolean,
    needs_upgrade: boolean,
    replay_version: number | null,
    problem: string | null,
    open_range: number | null,
    active_type: string | null,
    bookmarks: SuperShuckieBookmark[],
    types: SuperShuckieBookmarkType[]
}

/**
 * Stats (see external_commands.md)
 */
export interface SuperShuckieStats {
    time_start: number | null,
    time_end: number | null,
    time_offset: number | null,
    time_current: number | null,

    total_elapsed_time: number,
    total_elapsed_frames: number,

    is_recording: boolean,
    is_playing_back: boolean,
    is_paused: boolean,
    is_playback_finished: boolean,

    counters: Record<string, number>,

    current_speed: number,

    emulation_fps: number,
    frame_time_ms: number,
    frame_budget_ms: number,
    frames_over_budget: number,

    /** Changes whenever the bookmarks change; fetch bookmarks() when it does */
    bookmark_generation: number
}

/**
 * Error that may occur from a rest request
 */
export class SuperShuckieError extends Error {

}
