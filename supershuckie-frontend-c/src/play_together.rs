//! C API for Play Together (see `include/supershuckie/play_together.h`).

use crate::frontend::{write_error, write_str_to_data};
use std::ffi::{c_char, CStr, CString};
use std::num::NonZeroU8;
use std::path::PathBuf;
use std::slice::from_raw_parts_mut;
use std::sync::Arc;
use supershuckie_core::AudioOutput;
use supershuckie_frontend::play_together::{color_entry, PeerId, COLOR_COUNT};
use supershuckie_frontend::SuperShuckieFrontend;

unsafe fn c_str<'a>(text: *const c_char) -> &'a str {
    if text.is_null() {
        return ""
    }
    unsafe { CStr::from_ptr(text) }.to_str().unwrap_or("")
}

fn into_c_string(text: String) -> *mut c_char {
    CString::new(text.replace('\0', "")).expect("no NUL").into_raw()
}

/// Copy `text` into `(out, out_len)`, NUL-terminated and truncated to fit; returns the bytes the
/// whole text needs including its NUL, so a caller can size a buffer and retry.
unsafe fn write_string(text: &str, out: *mut u8, out_len: usize) -> usize {
    if !out.is_null() && out_len > 0 {
        write_str_to_data(text, unsafe { from_raw_parts_mut(out, out_len) });
    }
    text.len() + 1
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_play_together_host(
    frontend: &mut SuperShuckieFrontend,
    port: u16,
    display_name: *const c_char,
    color: u8,
    code_out: *mut u8,
    code_out_len: usize,
    error: *mut u8,
    error_len: usize
) -> bool {
    let name = unsafe { c_str(display_name) };
    match frontend.play_together_host(port, name, color) {
        Ok(code) => {
            unsafe { write_string(code.as_str(), code_out, code_out_len) };
            true
        }
        Err(e) => {
            unsafe { write_error(e.as_str(), error, error_len) };
            false
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_play_together_join(
    frontend: &mut SuperShuckieFrontend,
    code: *const c_char,
    display_name: *const c_char,
    color: u8,
    error: *mut u8,
    error_len: usize
) -> bool {
    let code = unsafe { c_str(code) };
    let name = unsafe { c_str(display_name) };
    match frontend.play_together_join(code, name, color) {
        Ok(()) => true,
        Err(e) => {
            unsafe { write_error(e.as_str(), error, error_len) };
            false
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_play_together_leave(frontend: &mut SuperShuckieFrontend) {
    frontend.play_together_leave();
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_play_together_is_active(frontend: &SuperShuckieFrontend) -> bool {
    frontend.is_play_together_active()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_play_together_generation(frontend: &SuperShuckieFrontend) -> u64 {
    frontend.play_together_generation()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_play_together_state_json(frontend: &SuperShuckieFrontend) -> *mut c_char {
    into_c_string(serde_json::to_string(&frontend.play_together_state()).unwrap_or_else(|_| "{}".to_owned()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_play_together_reset_all(
    frontend: &mut SuperShuckieFrontend,
    countdown_seconds: u32,
    error: *mut u8,
    error_len: usize
) -> bool {
    match frontend.play_together_reset_all(countdown_seconds) {
        Ok(()) => true,
        Err(e) => {
            unsafe { write_error(e.as_str(), error, error_len) };
            false
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_play_together_reset_countdown_ms(frontend: &SuperShuckieFrontend) -> u32 {
    frontend.play_together_reset_countdown_ms()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_play_together_get_sync_pause(frontend: &SuperShuckieFrontend) -> bool {
    frontend.get_play_together_sync_pause()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_play_together_set_sync_pause(
    frontend: &mut SuperShuckieFrontend,
    enabled: bool,
    error: *mut u8,
    error_len: usize
) -> bool {
    match frontend.set_play_together_sync_pause(enabled) {
        Ok(()) => true,
        Err(e) => {
            unsafe { write_error(e.as_str(), error, error_len) };
            false
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_play_together_get_start_state(frontend: &SuperShuckieFrontend) -> bool {
    frontend.get_play_together_start_state()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_play_together_link_request(
    frontend: &mut SuperShuckieFrontend,
    peer: PeerId,
    error: *mut u8,
    error_len: usize
) -> bool {
    match frontend.play_together_link_request(peer) {
        Ok(()) => true,
        Err(e) => {
            unsafe { write_error(e.as_str(), error, error_len) };
            false
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_play_together_link_respond(
    frontend: &mut SuperShuckieFrontend,
    nonce: u32,
    accept: bool,
    error: *mut u8,
    error_len: usize
) -> bool {
    match frontend.play_together_link_respond(nonce, accept) {
        Ok(()) => true,
        Err(e) => {
            unsafe { write_error(e.as_str(), error, error_len) };
            false
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_play_together_unlink(frontend: &mut SuperShuckieFrontend) {
    frontend.play_together_unlink();
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_play_together_is_link_cable_plugged(frontend: &SuperShuckieFrontend) -> bool {
    frontend.is_link_cable_plugged()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_play_together_link_state_json(frontend: &SuperShuckieFrontend) -> *mut c_char {
    into_c_string(serde_json::to_string(&frontend.play_together_link_state()).unwrap_or_else(|_| "{}".to_owned()))
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_play_together_get_link_input_delay(frontend: &SuperShuckieFrontend) -> u8 {
    frontend.get_play_together_link_input_delay()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_play_together_set_link_input_delay(frontend: &mut SuperShuckieFrontend, frames: u8) {
    frontend.set_play_together_link_input_delay(frames);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_play_together_set_start_state(
    frontend: &mut SuperShuckieFrontend,
    enabled: bool,
    error: *mut u8,
    error_len: usize
) -> bool {
    match frontend.set_play_together_start_state(enabled) {
        Ok(()) => true,
        Err(e) => {
            unsafe { write_error(e.as_str(), error, error_len) };
            false
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_play_together_locate_rom(
    frontend: &mut SuperShuckieFrontend,
    peer: PeerId,
    path: *const c_char,
    error: *mut u8,
    error_len: usize
) -> bool {
    let path = PathBuf::from(unsafe { c_str(path) });
    match frontend.play_together_locate_rom(peer, &path) {
        Ok(()) => true,
        Err(e) => {
            unsafe { write_error(e.as_str(), error, error_len) };
            false
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_play_together_add_rom_candidates_json(
    frontend: &mut SuperShuckieFrontend,
    paths_json: *const c_char
) {
    let text = unsafe { c_str(paths_json) };
    if let Ok(paths) = serde_json::from_str::<Vec<String>>(text) {
        frontend.play_together_add_rom_candidates(paths.into_iter().map(PathBuf::from).collect());
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_play_together_set_video_scale(
    frontend: &mut SuperShuckieFrontend,
    peer: PeerId,
    scale: u8
) {
    let Some(scale) = NonZeroU8::new(scale) else {
        return
    };
    frontend.play_together_set_video_scale((peer != 0).then_some(peer), scale);
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_play_together_get_video_scale(frontend: &SuperShuckieFrontend) -> u8 {
    frontend.play_together_video_scale().get()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_play_together_set_peer_audio_enabled(
    frontend: &mut SuperShuckieFrontend,
    peer: PeerId,
    enabled: bool,
    error: *mut u8,
    error_len: usize
) -> bool {
    match frontend.play_together_set_peer_audio_enabled(peer, enabled) {
        Ok(()) => true,
        Err(e) => {
            unsafe { write_error(e.as_str(), error, error_len) };
            false
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_play_together_set_peer_pokeabyte_enabled(
    frontend: &mut SuperShuckieFrontend,
    peer: PeerId,
    enabled: bool,
    error: *mut u8,
    error_len: usize
) -> bool {
    match frontend.play_together_set_peer_pokeabyte_enabled(peer, enabled) {
        Ok(()) => true,
        Err(e) => {
            unsafe { write_error(e.as_str(), error, error_len) };
            false
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_play_together_get_peer_pokeabyte_port(
    frontend: &SuperShuckieFrontend,
    peer: PeerId
) -> u16 {
    frontend.play_together_peer_pokeabyte_port(peer).unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_play_together_retain_peer_audio_output(
    frontend: &SuperShuckieFrontend,
    peer: PeerId
) -> *const AudioOutput {
    match frontend.play_together_peer_audio_output(peer) {
        Some(ring) => Arc::into_raw(ring),
        None => std::ptr::null()
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_play_together_set_window_hidden(
    frontend: &mut SuperShuckieFrontend,
    peer: PeerId,
    hidden: bool
) {
    frontend.play_together_set_window_hidden(peer, hidden);
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_play_together_get_save_peer_replays(frontend: &SuperShuckieFrontend) -> bool {
    frontend.get_play_together_save_peer_replays()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_play_together_set_save_peer_replays(frontend: &mut SuperShuckieFrontend, save: bool) {
    frontend.set_play_together_save_peer_replays(save);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_play_together_get_display_name(
    frontend: &SuperShuckieFrontend,
    out: *mut u8,
    out_len: usize
) -> usize {
    unsafe { write_string(frontend.get_play_together_display_name(), out, out_len) }
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_play_together_get_color(frontend: &SuperShuckieFrontend) -> u8 {
    frontend.get_play_together_color()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_play_together_color_count() -> u8 {
    COLOR_COUNT
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_play_together_color(color: u8, rgb_out: *mut u32, name_out: *mut u8, name_out_len: usize) -> bool {
    let Some(entry) = color_entry(color) else {
        return false
    };
    if !rgb_out.is_null() {
        unsafe { *rgb_out = entry.rgb };
    }
    unsafe { write_string(entry.name, name_out, name_out_len) };
    true
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_play_together_get_host_port(frontend: &SuperShuckieFrontend) -> u16 {
    frontend.get_play_together_host_port()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_play_together_get_last_join_code(
    frontend: &SuperShuckieFrontend,
    out: *mut u8,
    out_len: usize
) -> usize {
    unsafe { write_string(frontend.get_play_together_last_join_code(), out, out_len) }
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_play_together_local_addresses_json(frontend: &SuperShuckieFrontend) -> *mut c_char {
    into_c_string(serde_json::to_string(&frontend.play_together_local_addresses()).unwrap_or_else(|_| "[]".to_owned()))
}
