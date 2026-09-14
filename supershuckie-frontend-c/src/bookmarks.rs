//! C API for replay bookmarks (see `include/supershuckie/bookmarks.h`).

use crate::frontend::write_str_to_data;
use std::ffi::{c_char, CStr, CString};
use std::slice::from_raw_parts_mut;
use supershuckie_frontend::bookmarks::{BookmarkError, BookmarkParams, BookmarkTypeUpsert};
use supershuckie_frontend::SuperShuckieFrontend;

/// Operation succeeded; `out` holds the result JSON (if any).
const RESULT_OK: u32 = 0;
/// Operation failed; `out` holds the message.
const RESULT_ERROR: u32 = 1;
/// The replay must be upgraded first; `out` holds the question to ask. Retry with `allow_upgrade`.
const RESULT_NEEDS_UPGRADE_CONFIRMATION: u32 = 2;

unsafe fn c_str<'a>(text: *const c_char) -> &'a str {
    if text.is_null() {
        return ""
    }
    unsafe { CStr::from_ptr(text) }.to_str().unwrap_or("")
}

unsafe fn write_out(text: &str, out: *mut u8, out_len: usize) {
    if !out.is_null() && out_len > 0 {
        write_str_to_data(text, unsafe { from_raw_parts_mut(out, out_len) });
    }
}

fn into_c_string(text: String) -> *mut c_char {
    CString::new(text.replace('\0', "")).expect("no NUL").into_raw()
}

/// Parse a JSON object argument; an empty string or null is an empty object.
unsafe fn parse_json<T: serde::de::DeserializeOwned + Default>(json: *const c_char) -> Result<T, BookmarkError> {
    let text = unsafe { c_str(json) }.trim();
    if text.is_empty() {
        return Ok(T::default())
    }
    serde_json::from_str(text).map_err(|e| BookmarkError::Invalid(format!("Bad request: {e}")))
}

/// Report `result` through `out` and the result code.
unsafe fn finish<T: serde::Serialize>(result: Result<Option<T>, BookmarkError>, out: *mut u8, out_len: usize) -> u32 {
    match result {
        Ok(value) => {
            let json = value.map(|v| serde_json::to_string(&v).unwrap_or_default()).unwrap_or_default();
            unsafe { write_out(&json, out, out_len) };
            RESULT_OK
        }
        Err(e) => {
            unsafe { write_out(&e.to_string(), out, out_len) };
            match e {
                BookmarkError::NeedsUpgradeConfirmation { .. } => RESULT_NEEDS_UPGRADE_CONFIRMATION,
                _ => RESULT_ERROR
            }
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_bookmark_generation(frontend: &SuperShuckieFrontend) -> u64 {
    frontend.bookmark_generation()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_bookmarks_json(frontend: &SuperShuckieFrontend) -> *mut c_char {
    into_c_string(serde_json::to_string(&frontend.bookmarks_view()).unwrap_or_else(|_| "{}".to_owned()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_bookmark_add_json(
    frontend: &mut SuperShuckieFrontend,
    request_json: *const c_char,
    allow_upgrade: bool,
    out: *mut u8,
    out_len: usize
) -> u32 {
    let result = unsafe { parse_json::<BookmarkParams>(request_json) }.and_then(|params| frontend.add_bookmark(params, allow_upgrade));
    unsafe { finish(result.map(Some), out, out_len) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_bookmark_update_json(
    frontend: &mut SuperShuckieFrontend,
    id: u64,
    patch_json: *const c_char,
    allow_upgrade: bool,
    out: *mut u8,
    out_len: usize
) -> u32 {
    let result = unsafe { parse_json::<BookmarkParams>(patch_json) }.and_then(|params| frontend.update_bookmark(id, params, allow_upgrade));
    unsafe { finish(result.map(Some), out, out_len) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_bookmark_delete(
    frontend: &mut SuperShuckieFrontend,
    id: u64,
    allow_upgrade: bool,
    out: *mut u8,
    out_len: usize
) -> u32 {
    let result = frontend.delete_bookmark(id, allow_upgrade).map(|()| None::<()>);
    unsafe { finish(result, out, out_len) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_bookmark_toggle_range_json(
    frontend: &mut SuperShuckieFrontend,
    request_json: *const c_char,
    allow_upgrade: bool,
    out: *mut u8,
    out_len: usize
) -> u32 {
    #[derive(serde::Serialize)]
    struct Toggled {
        bookmark: supershuckie_frontend::bookmarks::BookmarkView,
        started: bool
    }
    let result = unsafe { parse_json::<BookmarkParams>(request_json) }
        .and_then(|params| frontend.toggle_range_bookmark(params, allow_upgrade))
        .map(|(bookmark, started)| Some(Toggled { bookmark, started }));
    unsafe { finish(result, out, out_len) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_bookmark_go_to(
    frontend: &mut SuperShuckieFrontend,
    id: u64,
    out_point: bool,
    error: *mut u8,
    error_len: usize
) -> bool {
    match frontend.go_to_bookmark(id, out_point) {
        Ok(()) => true,
        Err(e) => {
            unsafe { write_out(&e.to_string(), error, error_len) };
            false
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_bookmark_flush(
    frontend: &mut SuperShuckieFrontend,
    error: *mut u8,
    error_len: usize
) -> bool {
    match frontend.flush_bookmarks() {
        Ok(()) => true,
        Err(e) => {
            unsafe { write_out(&e.to_string(), error, error_len) };
            false
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_bookmark_types_json(frontend: &SuperShuckieFrontend) -> *mut c_char {
    into_c_string(serde_json::to_string(&frontend.bookmark_types()).unwrap_or_else(|_| "[]".to_owned()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_bookmark_type_upsert_json(
    frontend: &mut SuperShuckieFrontend,
    type_json: *const c_char,
    out: *mut u8,
    out_len: usize
) -> bool {
    let result = unsafe { parse_json::<BookmarkTypeUpsert>(type_json) }.and_then(|upsert| frontend.upsert_bookmark_type(upsert));
    unsafe { finish(result.map(Some), out, out_len) == RESULT_OK }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_bookmark_type_delete(
    frontend: &mut SuperShuckieFrontend,
    type_id: *const c_char,
    error: *mut u8,
    error_len: usize
) -> bool {
    match frontend.delete_bookmark_type(unsafe { c_str(type_id) }) {
        Ok(()) => true,
        Err(e) => {
            unsafe { write_out(&e.to_string(), error, error_len) };
            false
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_bookmark_active_type(
    frontend: &SuperShuckieFrontend,
    out: *mut u8,
    out_len: usize
) -> usize {
    let id = frontend.get_active_bookmark_type().unwrap_or_default();
    unsafe { write_out(&id, out, out_len) };
    id.len() + 1
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_set_bookmark_active_type(
    frontend: &mut SuperShuckieFrontend,
    type_id: *const c_char
) -> bool {
    frontend.set_active_bookmark_type(Some(unsafe { c_str(type_id) })).is_ok()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_bookmark_confirm_upgrade(frontend: &SuperShuckieFrontend) -> bool {
    frontend.get_confirm_replay_upgrade()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_set_bookmark_confirm_upgrade(frontend: &mut SuperShuckieFrontend, confirm: bool) {
    frontend.set_confirm_replay_upgrade(confirm)
}
