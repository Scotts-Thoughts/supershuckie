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

// ---------------------------------------------------------------------------------------------
// RAM search

use supershuckie_memory_tools::search::{Comparison, SearchSettings};
use supershuckie_memory_tools::{parse_number, parse_pattern, Number, PatternByte};

#[repr(C)]
#[derive(Copy, Clone)]
pub struct SuperShuckieSearchParamsC {
    pub value_type: u32,
    pub size: u8,
    pub big_endian: bool,
    pub alignment: u8,
    pub table: usize,
    pub regions: *const u32,
    pub region_count: usize,
    pub use_range: bool,
    pub range_start: u32,
    pub range_end: u32,
    pub epsilon: f64
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct SuperShuckieSearchStatusC {
    pub active: bool,
    pub busy: bool,
    pub progress_per_mille: u32,
    pub result_count: u64,
    pub steps: u32,
    pub can_undo: bool,
    pub can_redo: bool,
    pub frame: u64,
    pub state_changed: bool,
    pub generation: u64,
    pub value_type: u32,
    pub size: u8,
    pub big_endian: bool,
    pub alignment: u8
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct SuperShuckieSearchRowC {
    pub address: u32,
    pub region: u32,
    pub length: u8,
    pub previous: [u8; 64],
    pub first: [u8; 64]
}

/// Build a comparison from its C kind and the operands typed by the user.
fn comparison_from_c(kind: u32, format: &ValueFormat, a: &str, b: &str, table: &supershuckie_memory_tools::CharTable) -> Result<Comparison, String> {
    let number = |text: &str| -> Result<Number, String> {
        if text.trim().is_empty() {
            return Err("enter a value to compare with".to_owned())
        }
        parse_number(format, text)
    };
    Ok(match kind {
        0 => Comparison::Equal(number(a)?),
        1 => Comparison::NotEqual(number(a)?),
        2 => Comparison::Less(number(a)?),
        3 => Comparison::LessOrEqual(number(a)?),
        4 => Comparison::Greater(number(a)?),
        5 => Comparison::GreaterOrEqual(number(a)?),
        6 => {
            let (low, high) = (number(a)?, number(b)?);
            if low.as_f64() > high.as_f64() { Comparison::Between(high, low) } else { Comparison::Between(low, high) }
        }
        7 => Comparison::InSet(a.split(',').filter(|t| !t.trim().is_empty()).map(|t| parse_number(format, t)).collect::<Result<Vec<_>, _>>()?),
        8 => Comparison::Unknown,
        9 => match format.ty {
            ValueType::Text => Comparison::Pattern(table.encode(a)?.into_iter().map(|value| PatternByte { mask: 0xFF, value }).collect()),
            _ => Comparison::Pattern(parse_pattern(a)?)
        },
        10 => Comparison::Changed,
        11 => Comparison::Unchanged,
        12 => Comparison::Increased,
        13 => Comparison::Decreased,
        14 => Comparison::IncreasedBy(number(a)?),
        15 => Comparison::DecreasedBy(number(a)?),
        16 => Comparison::ChangedBy(number(a)?),
        17 => Comparison::ChangedByAtLeast(number(a)?),
        18 => Comparison::EqualToFirst,
        19 => Comparison::NotEqualToFirst,
        20 => Comparison::IncreasedSinceFirst,
        21 => Comparison::DecreasedSinceFirst,
        _ => return Err("unknown comparison".to_owned())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_search_new(
    frontend: &mut SuperShuckieFrontend,
    params: &SuperShuckieSearchParamsC,
    comparison: u32,
    operand_a: *const c_char,
    operand_b: *const c_char,
    pause: bool,
    error: *mut u8,
    error_len: usize
) -> bool {
    let (tools, core) = frontend.memory_tools_mut();
    let mut format = ValueFormat::new(value_type_from_c(params.value_type), params.size, params.big_endian);
    let (a, b) = unsafe { (c_str(operand_a), c_str(operand_b)) };

    let result = (|| {
        let table = tools.table(params.table);
        // A pattern's length is the value's size.
        if comparison == 9 && !format.ty.is_numeric() {
            let len = match format.ty {
                ValueType::Text => table.encode(a)?.len(),
                _ => parse_pattern(a)?.len()
            };
            if len == 0 {
                return Err("enter something to search for".to_owned())
            }
            format = ValueFormat::new(format.ty, len as u8, false);
        }
        let comparison = comparison_from_c(comparison, &format, a, b, table)?;
        let regions = if params.regions.is_null() { Vec::new() } else { unsafe { from_raw_parts(params.regions, params.region_count) }.iter().map(|r| *r as usize).collect() };
        let settings = SearchSettings {
            format,
            alignment: params.alignment.clamp(1, 4),
            regions,
            range: params.use_range.then_some((params.range_start, params.range_end)),
            epsilon: if params.epsilon > 0.0 { params.epsilon } else { 0.01 }
        };
        tools.search_scan(core, Some(settings), comparison, pause)
    })();

    match result {
        Ok(()) => true,
        Err(e) => {
            unsafe { write_error(&e, error, error_len) };
            false
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_search_refine(
    frontend: &mut SuperShuckieFrontend,
    comparison: u32,
    operand_a: *const c_char,
    operand_b: *const c_char,
    table: usize,
    pause: bool,
    error: *mut u8,
    error_len: usize
) -> bool {
    let (tools, core) = frontend.memory_tools_mut();
    let (a, b) = unsafe { (c_str(operand_a), c_str(operand_b)) };
    let result = (|| {
        let settings = tools.search_status().settings.ok_or_else(|| "There is no search to refine; start a new search".to_owned())?;
        let comparison = comparison_from_c(comparison, &settings.format, a, b, tools.table(table))?;
        tools.search_scan(core, None, comparison, pause)
    })();
    match result {
        Ok(()) => true,
        Err(e) => {
            unsafe { write_error(&e, error, error_len) };
            false
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_search_status(
    frontend: &SuperShuckieFrontend,
    out: *mut SuperShuckieSearchStatusC,
    message: *mut u8,
    message_len: usize
) -> bool {
    let status = frontend.memory_tools().search_status();
    if !out.is_null() {
        let settings = status.settings.as_ref();
        unsafe {
            *out = SuperShuckieSearchStatusC {
                active: status.active,
                busy: status.busy,
                progress_per_mille: status.progress,
                result_count: status.result_count,
                steps: status.steps,
                can_undo: status.can_undo,
                can_redo: status.can_redo,
                frame: status.frame,
                state_changed: status.state_changed,
                generation: status.generation,
                value_type: settings.map(|s| ValueType::ALL.iter().position(|t| *t == s.format.ty).unwrap_or(0) as u32).unwrap_or(0),
                size: settings.map(|s| s.format.size).unwrap_or(0),
                big_endian: settings.map(|s| s.format.big_endian).unwrap_or(false),
                alignment: settings.map(|s| s.alignment).unwrap_or(1)
            };
        }
    }
    match status.message {
        Some(m) => {
            unsafe { write_error(&m, message, message_len) };
            true
        }
        None => {
            unsafe { write_error("", message, message_len) };
            false
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_search_results(
    frontend: &SuperShuckieFrontend,
    offset: u64,
    out: *mut SuperShuckieSearchRowC,
    capacity: usize
) -> usize {
    if out.is_null() || capacity == 0 {
        return 0
    }
    let rows = frontend.memory_tools().search_results(offset, capacity);
    let out = unsafe { from_raw_parts_mut(out, capacity) };
    for (row, slot) in rows.iter().zip(out.iter_mut()) {
        let len = row.previous.len().min(64);
        let mut previous = [0u8; 64];
        let mut first = [0u8; 64];
        previous[..len].copy_from_slice(&row.previous[..len]);
        first[..row.first.len().min(64)].copy_from_slice(&row.first[..row.first.len().min(64)]);
        *slot = SuperShuckieSearchRowC { address: row.address, region: row.region as u32, length: len as u8, previous, first };
    }
    rows.len()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_search_set_visible_rows(frontend: &mut SuperShuckieFrontend, offset: u64, count: u32) {
    let (tools, core) = frontend.memory_tools_mut();
    tools.search_set_visible_rows(core, offset, count);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_search_read_visible(
    frontend: &SuperShuckieFrontend,
    first_row: *mut u64,
    generation: *mut u64,
    values: *mut u8,
    ok: *mut bool,
    capacity: usize
) -> usize {
    let tools = frontend.memory_tools();
    let (first, rows) = tools.search_visible_values();
    unsafe {
        if !first_row.is_null() { *first_row = first; }
        if !generation.is_null() { *generation = tools.sample_generation(); }
    }
    let count = rows.len().min(capacity);
    if values.is_null() || ok.is_null() {
        return count
    }
    let values = unsafe { from_raw_parts_mut(values, capacity * 64) };
    let ok = unsafe { from_raw_parts_mut(ok, capacity) };
    for (i, row) in rows.iter().take(count).enumerate() {
        match row {
            Some(bytes) => {
                let len = bytes.len().min(64);
                values[i * 64..i * 64 + len].copy_from_slice(&bytes[..len]);
                ok[i] = true;
            }
            None => ok[i] = false
        }
    }
    count
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_search_undo(frontend: &SuperShuckieFrontend) {
    frontend.memory_tools().search_undo();
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_search_redo(frontend: &SuperShuckieFrontend) {
    frontend.memory_tools().search_redo();
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_search_cancel(frontend: &SuperShuckieFrontend) {
    frontend.memory_tools().search_cancel();
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_search_reset(frontend: &mut SuperShuckieFrontend) {
    let (tools, core) = frontend.memory_tools_mut();
    tools.search_reset(core);
}
