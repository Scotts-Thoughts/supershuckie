//! C API for the RAM tools (see `include/supershuckie/memory.h`).

use crate::frontend::write_str_to_data;
use std::ffi::{c_char, CStr};
use std::ptr::null;
use std::slice::{from_raw_parts, from_raw_parts_mut};
use supershuckie_frontend::SuperShuckieFrontend;
use supershuckie_memory_tools::{format_address, format_region_address, format_value, parse_address, parse_hex_bytes, parse_value, DisplayBase, ValueFormat, ValueType};

#[repr(C)]
#[derive(Copy, Clone)]
pub struct SuperShuckieMemoryRegionC {
    pub name: *const c_char,
    pub short_name: *const c_char,
    pub base_address: u32,
    pub length: u32,
    pub default_big_endian: bool,
    pub writable: bool
}

/// Write `bytes` to a caller buffer, NUL-terminated if it fits. Returns the bytes needed
/// (including the NUL).
unsafe fn write_string(text: &str, out: *mut u8, out_len: usize) -> usize {
    if !out.is_null() && out_len > 0 {
        write_str_to_data(text, unsafe { from_raw_parts_mut(out, out_len) });
    }
    text.len() + 1
}

unsafe fn c_str<'a>(text: *const c_char) -> &'a str {
    if text.is_null() {
        return ""
    }
    unsafe { CStr::from_ptr(text) }.to_str().unwrap_or("")
}

unsafe fn write_error(message: &str, error: *mut u8, error_len: usize) {
    if !error.is_null() && error_len > 0 {
        write_str_to_data(message, unsafe { from_raw_parts_mut(error, error_len) });
    }
}

pub(crate) fn value_type_from_c(value_type: u32) -> ValueType {
    ValueType::ALL.get(value_type as usize).copied().unwrap_or(ValueType::U8)
}

pub(crate) fn display_from_c(display: u32) -> DisplayBase {
    match display {
        1 => DisplayBase::Hex,
        2 => DisplayBase::Binary,
        _ => DisplayBase::Decimal
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_memory_get_regions(
    frontend: &SuperShuckieFrontend,
    out: *mut SuperShuckieMemoryRegionC,
    capacity: usize
) -> usize {
    let tools = frontend.memory_tools();
    let regions = tools.regions();
    if !out.is_null() {
        let out = unsafe { from_raw_parts_mut(out, capacity) };
        for ((region, names), slot) in regions.iter().zip(tools.region_names()).zip(out.iter_mut()) {
            *slot = SuperShuckieMemoryRegionC {
                name: names.0.as_c_str().as_ptr(),
                short_name: names.1.as_c_str().as_ptr(),
                base_address: region.base,
                length: region.len,
                default_big_endian: region.big_endian,
                writable: region.writable
            };
        }
    }
    regions.len()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_memory_regions_generation(frontend: &SuperShuckieFrontend) -> u64 {
    frontend.memory_tools().regions_generation()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_memory_parse_address(
    frontend: &SuperShuckieFrontend,
    text: *const c_char,
    address: *mut u32,
    error: *mut u8,
    error_len: usize
) -> bool {
    match parse_address(unsafe { c_str(text) }, frontend.memory_tools().regions()) {
        Ok(a) => {
            if !address.is_null() {
                unsafe { *address = a };
            }
            true
        }
        Err(e) => {
            unsafe { write_error(&e, error, error_len) };
            false
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_memory_format_address(
    frontend: &SuperShuckieFrontend,
    address: u32,
    region_relative: bool,
    out: *mut u8,
    out_len: usize
) -> usize {
    let regions = frontend.memory_tools().regions();
    let text = if region_relative { format_region_address(address, regions) } else { format_address(address, regions) };
    unsafe { write_string(&text, out, out_len) }
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_memory_get_refresh_rate(frontend: &SuperShuckieFrontend) -> u8 {
    frontend.memory_tools().refresh_hz()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_memory_set_refresh_rate(frontend: &mut SuperShuckieFrontend, hz: u8) {
    let (tools, core) = frontend.memory_tools_mut();
    tools.set_refresh_hz(core, hz);
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_memory_set_viewer_window(
    frontend: &mut SuperShuckieFrontend,
    viewer: u8,
    enabled: bool,
    address: u32,
    length: u32
) {
    let (tools, core) = frontend.memory_tools_mut();
    tools.set_viewer_window(core, viewer as usize, (enabled && length > 0).then_some((address, length)));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_memory_read_viewer(
    frontend: &SuperShuckieFrontend,
    viewer: u8,
    generation: *mut u64,
    frame: *mut u64,
    address: *mut u32,
    bytes: *mut u8,
    capacity: u32,
    length: *mut u32,
    valid_length: *mut u32
) -> bool {
    let last = if generation.is_null() { 0 } else { unsafe { *generation } };
    let Some(sample) = frontend.memory_tools().read_viewer(viewer as usize, last) else {
        return false
    };
    let copied = sample.bytes.len().min(capacity as usize);
    unsafe {
        if !bytes.is_null() {
            from_raw_parts_mut(bytes, copied).copy_from_slice(&sample.bytes[..copied]);
        }
        if !generation.is_null() { *generation = sample.generation; }
        if !frame.is_null() { *frame = sample.frame; }
        if !address.is_null() { *address = sample.address; }
        if !length.is_null() { *length = copied as u32; }
        if !valid_length.is_null() { *valid_length = sample.valid_len.min(copied as u32); }
    }
    true
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_memory_table_count(frontend: &SuperShuckieFrontend) -> usize {
    frontend.memory_tools().tables().len()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_memory_table_name(frontend: &SuperShuckieFrontend, table: usize) -> *const c_char {
    frontend.memory_tools().table_names().get(table).map(|n| n.as_c_str().as_ptr()).unwrap_or(null())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_memory_reload_tables(
    frontend: &mut SuperShuckieFrontend,
    error: *mut u8,
    error_len: usize
) -> bool {
    let (tools, _) = frontend.memory_tools_mut();
    tools.reload_tables();
    let errors = tools.table_errors();
    if errors.is_empty() {
        return true
    }
    unsafe { write_error(&errors.join("\n"), error, error_len) };
    false
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_memory_tables_directory(
    frontend: &SuperShuckieFrontend,
    out: *mut u8,
    out_len: usize
) -> usize {
    let dir = frontend.memory_tools().tables_dir().to_string_lossy().into_owned();
    unsafe { write_string(&dir, out, out_len) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_memory_table_glyph(
    frontend: &SuperShuckieFrontend,
    table: usize,
    byte: u8,
    out: *mut u8,
    out_len: usize
) -> bool {
    match frontend.memory_tools().table(table).glyph(byte) {
        Some(glyph) => {
            unsafe { write_string(glyph, out, out_len) };
            true
        }
        None => false
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_memory_format_value(
    frontend: &SuperShuckieFrontend,
    table: usize,
    value_type: u32,
    size: u8,
    big_endian: bool,
    display: u32,
    bytes: *const u8,
    length: usize,
    out: *mut u8,
    out_len: usize
) -> usize {
    let format = ValueFormat::new(value_type_from_c(value_type), size, big_endian);
    let bytes = if bytes.is_null() { &[][..] } else { unsafe { from_raw_parts(bytes, length) } };
    let text = format_value(&format, display_from_c(display), bytes, frontend.memory_tools().table(table));
    unsafe { write_string(&text, out, out_len) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_memory_parse_value(
    frontend: &SuperShuckieFrontend,
    table: usize,
    value_type: u32,
    size: u8,
    big_endian: bool,
    text: *const c_char,
    out: *mut u8,
    capacity: usize,
    out_length: *mut usize,
    error: *mut u8,
    error_len: usize
) -> bool {
    let format = ValueFormat::new(value_type_from_c(value_type), size, big_endian);
    let result = parse_value(&format, unsafe { c_str(text) }, frontend.memory_tools().table(table))
        .and_then(|bytes| if bytes.len() > capacity { Err("value too long".to_owned()) } else { Ok(bytes) });
    match result {
        Ok(bytes) => {
            unsafe {
                if !out.is_null() {
                    from_raw_parts_mut(out, bytes.len()).copy_from_slice(&bytes);
                }
                if !out_length.is_null() { *out_length = bytes.len(); }
            }
            true
        }
        Err(e) => {
            unsafe { write_error(&e, error, error_len) };
            false
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_memory_parse_hex_bytes(
    text: *const c_char,
    out: *mut u8,
    capacity: usize,
    out_length: *mut usize,
    error: *mut u8,
    error_len: usize
) -> bool {
    match parse_hex_bytes(unsafe { c_str(text) }) {
        Ok(bytes) if bytes.len() <= capacity => {
            unsafe {
                if !out.is_null() {
                    from_raw_parts_mut(out, bytes.len()).copy_from_slice(&bytes);
                }
                if !out_length.is_null() { *out_length = bytes.len(); }
            }
            true
        }
        Ok(_) => {
            unsafe { write_error("too many bytes", error, error_len) };
            false
        }
        Err(e) => {
            unsafe { write_error(&e, error, error_len) };
            false
        }
    }
}
