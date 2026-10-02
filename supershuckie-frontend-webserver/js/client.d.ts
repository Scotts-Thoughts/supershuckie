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

    /**
     * Bot control (real time): replace what the bot holds; nothing given releases everything.
     * Needs Settings > Allow bot control.
     * @param input buttons, touch point and sticks to hold
     */
    input(input?: SuperShuckieBotInput): Promise<{ frame: number }>

    /**
     * Bot control (real time): hold for exactly `frames` frames, then release
     * @param input buttons or touch point to press
     * @param frames how long (1 to 3600, default 4)
     */
    press(input: SuperShuckieBotInput, frames?: number): Promise<{ frame: number }>

    /**
     * Bot control (lockstep): pause the game if it is running, then run exactly `frames` frames
     * @param frames how many (0 to 3600, default 1)
     * @param input replaces what the bot holds first (it stays held); omit to keep it
     * @param reads [address, length] ranges to read after the last frame
     */
    step(frames?: number, input?: SuperShuckieBotInput, reads?: [number, number][]): Promise<SuperShuckieStepResult>

    /**
     * Read console memory
     * @param address address
     * @param length bytes (1 to 65536)
     */
    read_memory(address: number, length: number): Promise<{ address: number, data: string }>

    /**
     * The screens as a PNG (stacked top to bottom)
     */
    screenshot(): Promise<Blob>
}

/**
 * What a bot holds (see external_commands.md)
 */
export interface SuperShuckieBotInput {
    buttons?: ("a" | "b" | "x" | "y" | "l" | "r" | "zl" | "zr" | "start" | "select" | "up" | "down" | "left" | "right")[],
    /** Bottom-screen pixels */
    touch?: [number, number],
    /** -127 to 127 each, positive = right / up */
    circle?: [number, number],
    cstick?: [number, number]
}

/**
 * The outcome of a step (see external_commands.md)
 */
export interface SuperShuckieStepResult {
    /** Frame count once the step ended */
    frame: number,
    frames_run: number,
    /** The game was running when the step arrived: lockstep was broken since the last step */
    was_running: boolean,
    cancelled: boolean,
    cancel_reason: string | null,
    /** Hex, or null where unmapped */
    reads: { address: number, data: string | null }[]
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
