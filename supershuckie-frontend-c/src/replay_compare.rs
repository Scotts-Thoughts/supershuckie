//! C API for compared replays (see `include/supershuckie/replay_compare.h`).

use crate::frontend::write_error;
use std::ffi::{c_char, CStr, CString};
use std::num::NonZeroU8;
use std::path::Path;
use supershuckie_frontend::replay_compare::{CompareAddError, CompareId};
use supershuckie_frontend::SuperShuckieFrontend;

/// `supershuckie_frontend_replay_compare_add*` results.
const ADD_OK: i32 = 0;
const ADD_NEEDS_ROM: i32 = 1;
const ADD_MISMATCH: i32 = 2;
const ADD_FAILED: i32 = 3;

unsafe fn c_str<'a>(text: *const c_char) -> Option<&'a str> {
    if text.is_null() {
        return None
    }
    unsafe { CStr::from_ptr(text) }.to_str().ok().filter(|s| !s.is_empty())
}

fn add_result(result: Result<CompareId, CompareAddError>, id_out: *mut u16, error: *mut u8, error_len: usize) -> i32 {
    match result {
        Ok(id) => {
            if !id_out.is_null() {
                unsafe { *id_out = id };
            }
            ADD_OK
        }
        Err(e) => {
            unsafe { write_error(&e.message(), error, error_len) };
            match e {
                CompareAddError::NeedsRom { .. } => ADD_NEEDS_ROM,
                CompareAddError::Mismatch(_) => ADD_MISMATCH,
                CompareAddError::Failed(_) => ADD_FAILED
            }
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_replay_compare_add(
    frontend: &mut SuperShuckieFrontend,
    replay_path: *const c_char,
    rom_path: *const c_char,
    override_errors: bool,
    id_out: *mut u16,
    error: *mut u8,
    error_len: usize
) -> i32 {
    let Some(replay_path) = (unsafe { c_str(replay_path) }) else {
        unsafe { write_error("No replay file given.", error, error_len) };
        return ADD_FAILED
    };
    let rom_path = unsafe { c_str(rom_path) }.map(Path::new);
    add_result(frontend.replay_compare_add(Path::new(replay_path), rom_path, override_errors), id_out, error, error_len)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_replay_compare_add_by_name(
    frontend: &mut SuperShuckieFrontend,
    replay_name: *const c_char,
    rom_path: *const c_char,
    override_errors: bool,
    id_out: *mut u16,
    error: *mut u8,
    error_len: usize
) -> i32 {
    let Some(name) = (unsafe { c_str(replay_name) }) else {
        unsafe { write_error("No replay given.", error, error_len) };
        return ADD_FAILED
    };
    let path = match frontend.replay_path_for_name(name) {
        Ok(path) => path,
        Err(e) => {
            unsafe { write_error(e.as_str(), error, error_len) };
            return ADD_FAILED
        }
    };
    let rom_path = unsafe { c_str(rom_path) }.map(Path::new);
    add_result(frontend.replay_compare_add(&path, rom_path, override_errors), id_out, error, error_len)
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_replay_compare_remove(frontend: &mut SuperShuckieFrontend, id: u16) {
    frontend.replay_compare_remove(id);
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_replay_compare_clear(frontend: &mut SuperShuckieFrontend) {
    frontend.replay_compare_clear();
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_replay_compare_count(frontend: &SuperShuckieFrontend) -> usize {
    frontend.replay_compare_count()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_replay_compare_generation(frontend: &SuperShuckieFrontend) -> u64 {
    frontend.replay_compare_generation()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_replay_compare_state_json(frontend: &SuperShuckieFrontend) -> *mut c_char {
    let json = serde_json::to_string(&frontend.replay_compare_state()).unwrap_or_else(|_| "{}".to_owned());
    CString::new(json.replace('\0', "")).expect("no NUL").into_raw()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_replay_compare_set_video_scale(frontend: &mut SuperShuckieFrontend, id: u16, scale: u8) {
    if let Some(scale) = NonZeroU8::new(scale) {
        frontend.replay_compare_set_video_scale(id, scale);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_replay_compare_set_window_hidden(frontend: &mut SuperShuckieFrontend, id: u16, hidden: bool) {
    frontend.replay_compare_set_window_hidden(id, hidden);
}
