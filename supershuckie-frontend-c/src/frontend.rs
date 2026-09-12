use std::ffi::{c_char, c_int, c_void, CStr};
use std::mem::MaybeUninit;
use std::num::NonZeroU8;
use std::ptr::null;
use std::slice::from_raw_parts_mut;
use std::sync::Arc;
use supershuckie_core::emulator::{ScreenData, ScreenDataEncoding, AUDIO_SAMPLE_RATE};
use supershuckie_core::AudioOutput;
use supershuckie_frontend::{ConnectedControllerIndex, SuperShuckieEmulatorType, SuperShuckieFrontend, SuperShuckieFrontendCallbacks, SuperShuckieReplayState, UserInput};
use supershuckie_frontend::settings::{GameBoyMode, NintendoDSDate};
use supershuckie_frontend::util::UTF8CString;
use crate::control_settings::SuperShuckieControlSettings;
use crate::string_array::SuperShuckieStringArray;

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct SuperShuckieScreenDataC {
    pub width: u32,
    pub height: u32,
    pub screen_data_encoding: ScreenDataEncoding
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct SuperShuckieFrontendCallbacksC {
    pub userdata: *mut c_void,

    pub refresh_screens: Option<unsafe extern "C" fn(userdata: *mut c_void, screen_count: usize, screen_data: *const *const u32)>,
    pub change_video_mode: Option<unsafe extern "C" fn(userdata: *mut c_void, screen_count: usize, screen_data: *const SuperShuckieScreenDataC, screen_scale: NonZeroU8)>,
}

impl SuperShuckieFrontendCallbacks for SuperShuckieFrontendCallbacksC {
    fn refresh_screens(&mut self, screens: &[ScreenData]) {
        let Some(s) = self.refresh_screens else { return };

        let mut screens_buf = [null(); 4];
        for (index, screen) in screens.iter().enumerate() {
            screens_buf[index] = screen.pixels.as_ptr();
        }

        unsafe { s(self.userdata, screens.len(), screens_buf.as_ptr()) };
    }

    fn change_video_mode(&mut self, screens: &[ScreenData], scaling: NonZeroU8) {
        let Some(s) = self.change_video_mode else { return };

        let mut screens_buf = [MaybeUninit::<SuperShuckieScreenDataC>::uninit(); 4];
        for (index, screen) in screens.iter().enumerate() {
            screens_buf[index].write(SuperShuckieScreenDataC {
                width: screen.width as u32,
                height: screen.height as u32,
                screen_data_encoding: screen.encoding
            });
        }

        unsafe { s(self.userdata, screens.len(), screens_buf.as_ptr() as *const SuperShuckieScreenDataC, scaling) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_new(
    data_dir: *const c_char,
    config_dir: *const c_char,
    callbacks: &SuperShuckieFrontendCallbacksC
) -> *mut SuperShuckieFrontend {
    let data_dir = unsafe { CStr::from_ptr(data_dir) }
        .to_str()
        .expect("data_dir is not UTF-8");
    let config_file = unsafe { CStr::from_ptr(config_dir) }
        .to_str()
        .expect("config_file is not UTF-8");

    Box::into_raw(Box::new(SuperShuckieFrontend::new(data_dir, config_file, Box::new(*callbacks))))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_key_press(
    frontend: &mut SuperShuckieFrontend,
    keycode: i32,
    pressed: bool
) {
    frontend.on_user_input(UserInput::Keyboard { keycode }, pressed.then_some(1.0).unwrap_or(0.0));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_button_press(
    frontend: &mut SuperShuckieFrontend,
    controller: ConnectedControllerIndex,
    button: i32,
    pressed: bool
) {
    frontend.on_user_input(UserInput::Button { controller, button }, pressed.then_some(1.0).unwrap_or(0.0));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_axis(
    frontend: &mut SuperShuckieFrontend,
    controller: ConnectedControllerIndex,
    axis: i32,
    value: f64
) {
    frontend.on_user_input(UserInput::Axis { controller, axis }, value);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_set_paused(
    frontend: &mut SuperShuckieFrontend,
    paused: bool
) {
    frontend.set_paused(paused);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_tick(
    frontend: &mut SuperShuckieFrontend,
    error: *mut u8,
    error_len: usize
) -> bool {
    if let Err(e) = frontend.tick() {
        write_str_to_data(e.as_str(), unsafe { from_raw_parts_mut(error, error_len) });
        false
    }
    else {
        true
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_load_rom(
    frontend: &mut SuperShuckieFrontend,
    path: *const c_char,
    error: *mut u8,
    error_len: usize
) -> bool {
    let path = unsafe { CStr::from_ptr(path) };
    if error_len > 0 && let Err(e) = frontend.load_rom(path.to_str().expect("supershuckie_frontend_load_rom with non-UTF-8 path")) {
        write_str_to_data(e.as_str(), unsafe { from_raw_parts_mut(error, error_len) });
        false
    }
    else {
        true
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_close_rom(
    frontend: &mut SuperShuckieFrontend
) {
    let _ = frontend.close_rom();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_unload_rom(
    frontend: &mut SuperShuckieFrontend
) {
    frontend.unload_rom();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_load_or_create_save_file(
    frontend: &mut SuperShuckieFrontend,
    save_file: *const c_char,
    initialize: bool
) {
    let save_file = unsafe { CStr::from_ptr(save_file) }.to_str().expect("save file not utf-8");
    frontend.load_or_create_save_file(save_file, initialize);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_set_current_save_file(
    frontend: &mut SuperShuckieFrontend,
    save_file: *const c_char
) {
    let save_file = unsafe { CStr::from_ptr(save_file) }.to_str().expect("save file not utf-8");
    frontend.set_current_save_file(save_file);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_get_current_save_file(
    frontend: &mut SuperShuckieFrontend,
    length: *mut usize
) -> *const u8 {
    let Some(n) = frontend.get_current_save_name() else {
        return null()
    };
    if !length.is_null() {
        unsafe { *length = n.len(); }
    }
    n.as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_hard_reset_console(
    frontend: &mut SuperShuckieFrontend
) {
    frontend.hard_reset_console();
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_is_game_running(
    frontend: &SuperShuckieFrontend
) -> bool {
    frontend.is_game_running()
}

fn write_str_to_data(string: &str, buffer: &mut [u8]) {
    if buffer.is_empty() {
        return
    }
    buffer.fill(0);

    let buffer_length = buffer.len();
    let mut buffer_usable = &mut buffer[0..buffer_length - 1]; // need the last byte to be null terminated
    if buffer_usable.is_empty() {
        return
    }

    let mut char_data = [0u8; 4];
    for c in string.chars() {
        let bytes = c.encode_utf8(&mut char_data).as_bytes();
        let Some((a, b)) = buffer_usable.split_at_mut_checked(bytes.len()) else {
            return
        };
        a.copy_from_slice(bytes);
        buffer_usable = b;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_force_refresh_screens(
    frontend: &mut SuperShuckieFrontend
) {
    frontend.force_refresh_screens();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_set_video_scale(
    frontend: &mut SuperShuckieFrontend,
    scale: u8
) {
    frontend.set_video_scale(NonZeroU8::new(scale).unwrap_or(unsafe { NonZeroU8::new_unchecked(1) }));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_get_custom_setting(
    frontend: &SuperShuckieFrontend,
    setting: *const c_char
) -> *const c_char {
    frontend.get_custom_setting(unsafe { CStr::from_ptr(setting) }.to_str().expect("supershuckie_frontend_get_custom_setting bad setting"))
        .map(|i| i.as_c_str().as_ptr())
        .unwrap_or(null())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_start_recording_replay(
    frontend: &mut SuperShuckieFrontend,
    name: *const c_char,
    result: *mut u8,
    result_len: usize
) -> bool {
    let name = if !name.is_null() { Some(unsafe { CStr::from_ptr(name) }.to_str().expect("name not UTF-8")) } else { None };
    let (success, msg) = match frontend.start_recording_replay(name) {
        Ok(n) => (true, n),
        Err(n) => (false, n)
    };

    write_str_to_data(msg.as_str(), unsafe { from_raw_parts_mut(result, result_len) });
    success
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_resume_recording_from_replay(
    frontend: &mut SuperShuckieFrontend,
    source_name: *const c_char,
    resume_at_frame: u32,
    use_end: bool,
    new_name: *const c_char,
    result: *mut u8,
    result_len: usize
) -> bool {
    let source_name = unsafe { CStr::from_ptr(source_name) }.to_str().expect("source_name not UTF-8");
    let new_name = if !new_name.is_null() { Some(unsafe { CStr::from_ptr(new_name) }.to_str().expect("new_name not UTF-8")) } else { None };
    let frame = if use_end { None } else { Some(resume_at_frame) };

    let (success, msg) = match frontend.resume_recording_from_replay(source_name, frame, new_name) {
        Ok(n) => (true, n),
        Err(n) => (false, n)
    };

    write_str_to_data(msg.as_str(), unsafe { from_raw_parts_mut(result, result_len) });
    success
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_resume_recording_from_current_replay(
    frontend: &mut SuperShuckieFrontend,
    result: *mut u8,
    result_len: usize
) -> bool {
    let (success, msg) = match frontend.resume_recording_from_current_replay() {
        Ok(n) => (true, n),
        Err(n) => (false, n)
    };

    write_str_to_data(msg.as_str(), unsafe { from_raw_parts_mut(result, result_len) });
    success
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_stop_recording_replay(
    frontend: &mut SuperShuckieFrontend
) {
    frontend.stop_recording_replay();
}

/// Get the replays directory of the current ROM (a starting point for file dialogs).
///
/// Returns false (writing nothing) if no game is loaded.
///
/// Safety: `path` must be at least `path_len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_get_replays_dir_for_current_rom(
    frontend: &SuperShuckieFrontend,
    path: *mut u8,
    path_len: usize
) -> bool {
    match frontend.get_replays_dir_for_current_rom() {
        Some(dir) => {
            write_str_to_data(&dir.to_string_lossy(), unsafe { from_raw_parts_mut(path, path_len) });
            true
        }
        None => false
    }
}

/// Plan the conversion of `path` (a .replay file, or a folder searched recursively) to the current
/// replay format, and remember it for `supershuckie_frontend_start_replay_conversion`.
///
/// Returns true and writes a one-line description of the plan to `description`, or returns false
/// and writes the reason nothing can be converted (already the current format, being recorded,
/// unreadable, or a conversion already running).
///
/// Safety: `path` must be a valid UTF-8 C string; `description` must be at least
/// `description_len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_plan_replay_conversion(
    frontend: &mut SuperShuckieFrontend,
    path: *const c_char,
    description: *mut u8,
    description_len: usize
) -> bool {
    let path = unsafe { CStr::from_ptr(path) }.to_str().expect("path not UTF-8");
    let (ok, message) = match frontend.plan_replay_conversion(std::path::Path::new(path)) {
        Ok(description) => (true, UTF8CString::from(description)),
        Err(e) => (false, e)
    };
    write_str_to_data(message.as_str(), unsafe { from_raw_parts_mut(description, description_len) });
    ok
}

/// Start the planned replay conversion on a background thread. Each replay is converted into a
/// temporary file next to it and verified before the original is replaced; with `keep_backups` the
/// original is kept as `<name>.replay.bak`.
///
/// On failure, writes an error to `error` and returns false. Poll with
/// `supershuckie_frontend_replay_conversion_poll`, cancel with
/// `supershuckie_frontend_replay_conversion_cancel`, and collect the summary with
/// `supershuckie_frontend_replay_conversion_poll_finished`.
///
/// Safety: `error` must be at least `error_len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_start_replay_conversion(
    frontend: &mut SuperShuckieFrontend,
    keep_backups: bool,
    error: *mut u8,
    error_len: usize
) -> bool {
    match frontend.start_replay_conversion(keep_backups) {
        Ok(()) => true,
        Err(e) => {
            write_str_to_data(e.as_str(), unsafe { from_raw_parts_mut(error, error_len) });
            false
        }
    }
}

/// Poll the replay conversion in progress. Writes the 0-based index of the replay being worked on
/// and the number of replays, the phase (0 = converting, 1 = verifying), the frames done / total in
/// that phase, and the file name of the current replay (each when non-null).
///
/// Returns true if a conversion is currently active.
///
/// Safety: `current_name` must be at least `current_name_len` bytes when non-null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_replay_conversion_poll(
    frontend: &SuperShuckieFrontend,
    file_index: *mut u32,
    file_count: *mut u32,
    phase: *mut u32,
    frames_done: *mut u64,
    frames_total: *mut u64,
    current_name: *mut u8,
    current_name_len: usize
) -> bool {
    use supershuckie_frontend::replay_convert::ConvertPhase;

    let Some(status) = frontend.poll_replay_conversion() else {
        return false
    };

    if !file_index.is_null() { unsafe { *file_index = status.file_index as u32; } }
    if !file_count.is_null() { unsafe { *file_count = status.file_count as u32; } }
    if !phase.is_null() { unsafe { *phase = if status.phase == ConvertPhase::Verifying { 1 } else { 0 }; } }
    if !frames_done.is_null() { unsafe { *frames_done = status.done; } }
    if !frames_total.is_null() { unsafe { *frames_total = status.total; } }
    if !current_name.is_null() {
        write_str_to_data(status.current_name.as_str(), unsafe { from_raw_parts_mut(current_name, current_name_len) });
    }
    true
}

/// Request cancellation of the replay conversion in progress, if any. The replay being worked on
/// is left untouched.
#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_replay_conversion_cancel(frontend: &SuperShuckieFrontend) {
    frontend.cancel_replay_conversion();
}

/// Non-blocking check for the end of the replay conversion.
///
/// Returns 0 while it is still running (or none is active); 1 when it has finished, in which case
/// a multi-line summary (converted files, sizes, failures) is written to `summary` and the job is
/// cleared.
///
/// Safety: `summary` must be at least `summary_len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_replay_conversion_poll_finished(
    frontend: &mut SuperShuckieFrontend,
    summary: *mut u8,
    summary_len: usize
) -> u32 {
    match frontend.poll_replay_conversion_finished() {
        None => 0,
        Some(result) => {
            write_str_to_data(result.describe().as_str(), unsafe { from_raw_parts_mut(summary, summary_len) });
            1
        }
    }
}

/// Start a video export of a replay.
///
/// `preset`: 0 = MP4/H.264, 1 = lossless FFV1/MKV, 2 = custom (uses `custom_args`).
/// `layout`: 0 = vertical stack, 1 = horizontal stack, 2 = top only, 3 = bottom only (NDS).
/// `use_range`: if true, export frames [`start_frame`, `end_frame`); otherwise use the replay's
/// crop range (or the whole replay). `scale` is the integer upscale factor (min 1).
/// `custom_args` is only read when `preset == 2`.
///
/// On failure, writes an error to `error` and returns false. On success returns true; poll progress
/// with `..._export_poll` and completion with `..._export_poll_finished`.
///
/// Safety: `replay_name`/`output_path` must be valid UTF-8 C strings; `error` must be at least
/// `error_len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_export_replay_video(
    frontend: &mut SuperShuckieFrontend,
    replay_name: *const c_char,
    output_path: *const c_char,
    use_range: bool,
    start_frame: u32,
    end_frame: u32,
    preset: u32,
    custom_args: *const c_char,
    scale: u32,
    layout: u32,
    error: *mut u8,
    error_len: usize
) -> bool {
    use supershuckie_core::ScreenLayout;
    use supershuckie_frontend::settings::ExportPreset;

    let replay_name = unsafe { CStr::from_ptr(replay_name) }.to_str().expect("replay_name not UTF-8");
    let output_path = unsafe { CStr::from_ptr(output_path) }.to_str().expect("output_path not UTF-8");

    let preset = match preset {
        1 => ExportPreset::LosslessFfv1Mkv,
        2 => {
            let args = if !custom_args.is_null() {
                unsafe { CStr::from_ptr(custom_args) }.to_str().expect("custom_args not UTF-8").to_owned()
            } else {
                String::new()
            };
            ExportPreset::Custom(args)
        }
        _ => ExportPreset::Mp4H264,
    };

    let layout = match layout {
        1 => ScreenLayout::HorizontalStack,
        2 => ScreenLayout::TopOnly,
        3 => ScreenLayout::BottomOnly,
        _ => ScreenLayout::VerticalStack,
    };

    let scale = NonZeroU8::new(scale.clamp(1, u8::MAX as u32) as u8).unwrap_or(NonZeroU8::new(1).unwrap());
    let range = if use_range { Some((start_frame, end_frame)) } else { None };

    match frontend.start_replay_video_export(replay_name, range, std::path::Path::new(output_path), preset, scale, layout) {
        Ok(()) => true,
        Err(e) => {
            write_str_to_data(e.as_str(), unsafe { from_raw_parts_mut(error, error_len) });
            false
        }
    }
}

/// Poll the in-progress export's progress. Writes `frames_done`/`frames_total` (when non-null).
/// Returns true if an export is currently active.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_export_poll(
    frontend: &SuperShuckieFrontend,
    frames_done: *mut u64,
    frames_total: *mut u64
) -> bool {
    match frontend.poll_export_progress() {
        Some((done, total)) => {
            if !frames_done.is_null() { unsafe { *frames_done = done; } }
            if !frames_total.is_null() { unsafe { *frames_total = total; } }
            true
        }
        None => false
    }
}

/// Request cancellation of the in-progress export, if any.
#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_export_cancel(frontend: &SuperShuckieFrontend) {
    frontend.cancel_export();
}

/// Non-blocking check for export completion.
///
/// Returns: 0 = still running (or no export), 1 = finished successfully, 2 = finished with an error
/// (the error message is written to `error`). On 1 or 2 the export handle is cleared.
///
/// Safety: `error` must be at least `error_len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_export_poll_finished(
    frontend: &mut SuperShuckieFrontend,
    error: *mut u8,
    error_len: usize
) -> u32 {
    match frontend.poll_export_finished() {
        None => 0,
        Some(Ok(())) => 1,
        Some(Err(e)) => {
            write_str_to_data(e.as_str(), unsafe { from_raw_parts_mut(error, error_len) });
            2
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_get_recording_replay_file(
    frontend: &SuperShuckieFrontend
) -> *const c_char {
    frontend.get_replay_file_info().map(|i| i.final_replay_name.as_c_str().as_ptr()).unwrap_or(null())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_create_save_state(
    frontend: &mut SuperShuckieFrontend,
    name: *const c_char,
    result: *mut u8,
    result_len: usize
) -> bool {
    let name = if !name.is_null() { Some(unsafe { CStr::from_ptr(name) }.to_str().expect("name not UTF-8")) } else { None };
    let (success, msg) = match frontend.create_save_state(name) {
        Ok(n) => (true, n),
        Err(n) => (false, n)
    };

    write_str_to_data(msg.as_str(), unsafe { from_raw_parts_mut(result, result_len) });
    success
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_undo_load_save_state(
    frontend: &mut SuperShuckieFrontend
) -> bool {
    frontend.undo_load_save_state()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_redo_load_save_state(
    frontend: &mut SuperShuckieFrontend
) -> bool {
    frontend.redo_load_save_state()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_load_save_state(
    frontend: &mut SuperShuckieFrontend,
    name: *const c_char,
    error: *mut u8,
    error_len: usize
) -> bool {
    let name = unsafe { CStr::from_ptr(name) }.to_str().expect("name not UTF-8");
    match frontend.load_save_state_if_exists(name) {
        Ok(true) => true,
        Ok(false) => {
            if error_len >= 1 {
                unsafe { *error = 0 };
            }
            false
        }
        Err(_) if error_len == 0 => false,
        Err(e) => {
            write_str_to_data(e.as_str(), unsafe { from_raw_parts_mut(error, error_len) });
            false
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_is_pokeabyte_enabled(
    frontend: &mut SuperShuckieFrontend,
    error: *mut u8,
    error_len: usize
) -> bool {
    match frontend.is_pokeabyte_enabled() {
        Ok(n) => {
            unsafe { *error = 0 };
            n
        },
        Err(e) => {
            write_str_to_data(e.as_str(), unsafe { from_raw_parts_mut(error, error_len) });
            false
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_get_external_commands_enabled(
    frontend: &mut SuperShuckieFrontend,
    error: *mut u8,
    error_len: usize
) -> bool {
    match frontend.get_external_commands_enabled() {
        Ok(n) => {
            unsafe { *error = 0 };
            n
        },
        Err(e) => {
            write_str_to_data(e.as_str(), unsafe { from_raw_parts_mut(error, error_len) });
            false
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_is_paused(
    frontend: &SuperShuckieFrontend
) -> bool {
    frontend.is_paused()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_set_pokeabyte_enabled(
    frontend: &mut SuperShuckieFrontend,
    enabled: bool,
    error: *mut u8,
    error_len: usize
) -> bool {
    match frontend.set_pokeabyte_enabled(enabled) {
        Ok(_) => true,
        Err(e) => {
            write_str_to_data(e.as_str(), unsafe { from_raw_parts_mut(error, error_len) });
            false
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_set_external_commands_enabled(
    frontend: &mut SuperShuckieFrontend,
    enabled: bool,
    error: *mut u8,
    error_len: usize
) -> bool {
    match frontend.set_external_commands_enabled(enabled) {
        Ok(_) => true,
        Err(e) => {
            write_str_to_data(e.as_str(), unsafe { from_raw_parts_mut(error, error_len) });
            false
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_set_auto_stop_playback_on_input_setting(
    frontend: &mut SuperShuckieFrontend,
    new_setting: bool
) {
    frontend.set_auto_stop_playback_on_input_setting(new_setting);
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_get_auto_stop_playback_on_input_setting(frontend: &SuperShuckieFrontend) -> bool {
    frontend.get_auto_stop_playback_on_input_setting()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_set_auto_unpause_on_input_setting(
    frontend: &mut SuperShuckieFrontend,
    new_setting: bool
) {
    frontend.set_auto_unpause_on_input_setting(new_setting);
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_get_auto_unpause_on_input_setting(frontend: &SuperShuckieFrontend) -> bool {
    frontend.get_auto_unpause_on_input_setting()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_set_auto_pause_on_record_setting(
    frontend: &mut SuperShuckieFrontend,
    new_setting: bool
) {
    frontend.set_auto_pause_on_record_setting(new_setting);
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_get_auto_pause_on_record_setting(frontend: &SuperShuckieFrontend) -> bool {
    frontend.get_auto_pause_on_record_setting()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_set_auto_decompress_replays_upfront_setting(
    frontend: &mut SuperShuckieFrontend,
    new_setting: bool
) {
    frontend.set_auto_decompress_replays_upfront_setting(new_setting);
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_get_auto_decompress_replays_upfront_setting(frontend: &SuperShuckieFrontend) -> bool {
    frontend.get_auto_decompress_replays_upfront_setting()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_save_sram(
    frontend: &mut SuperShuckieFrontend,
    error: *mut u8,
    error_len: usize
) -> bool {
    match frontend.save_sram() {
        Ok(_) => true,
        Err(_) if error_len == 0 => false,
        Err(e) => {
            write_str_to_data(e.as_str(), unsafe { from_raw_parts_mut(error, error_len) });
            false
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_set_custom_setting(
    frontend: &mut SuperShuckieFrontend,
    setting: *const c_char,
    value: *const c_char
) {
    frontend.set_custom_setting(
        unsafe { CStr::from_ptr(setting) }.to_str().expect("supershuckie_frontend_set_custom_setting bad setting"),
        if value.is_null() {
            None
        }
        else {
            Some(UTF8CString::from_cstr(unsafe { CStr::from_ptr(value) }))
        }
    );
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_get_rom_name(
    frontend: &SuperShuckieFrontend
) -> *const c_char {
    frontend.get_current_rom_name_c_str().map(|i| i.as_ptr()).unwrap_or(null())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_write_settings(
    frontend: &SuperShuckieFrontend
) {
    frontend.write_config();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_get_speed_settings(
    frontend: &SuperShuckieFrontend,
    base: *mut f64,
    turbo: *mut f64
) {
    let base = unsafe { nullable_reference!(base) };
    let turbo = unsafe { nullable_reference!(turbo) };
    frontend.get_speed_settings(base, turbo);
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_set_speed_settings(
    frontend: &mut SuperShuckieFrontend,
    base: f64,
    turbo: f64
) {
    frontend.set_speed_settings(base, turbo);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_free(
    frontend: *mut SuperShuckieFrontend
) {
    if !frontend.is_null() {
        let _ = unsafe { Box::from_raw(frontend) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_get_all_replays_for_rom(
    frontend: &SuperShuckieFrontend,
    rom: *const c_char
) -> *mut SuperShuckieStringArray {
    let array = match unsafe { current_rom_or_null(frontend, rom) } {
        Some(rom) => SuperShuckieStringArray(frontend.get_all_replays_for_rom(rom)),
        None => SuperShuckieStringArray::default()
    };
    Box::into_raw(Box::new(array))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_get_all_saves_for_rom(
    frontend: &SuperShuckieFrontend,
    rom: *const c_char
) -> *mut SuperShuckieStringArray {
    let array = match unsafe { current_rom_or_null(frontend, rom) } {
        Some(rom) => SuperShuckieStringArray(frontend.get_all_saves_for_rom(rom)),
        None => SuperShuckieStringArray::default()
    };
    Box::into_raw(Box::new(array))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_get_all_save_states_for_rom(
    frontend: &SuperShuckieFrontend,
    rom: *const c_char
) -> *mut SuperShuckieStringArray {
    let array = match unsafe { current_rom_or_null(frontend, rom) } {
        Some(rom) => SuperShuckieStringArray(frontend.get_all_save_states_for_rom(rom)),
        None => SuperShuckieStringArray::default()
    };
    Box::into_raw(Box::new(array))
}

/// Emulated frames per second over roughly the last second, drawn or not (the true emulation
/// rate). Call it once per UI tick; it updates its window itself.
#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_get_emulation_fps(frontend: &mut SuperShuckieFrontend) -> f64 {
    frontend.get_emulation_fps()
}

/// Frame-time diagnostics from the core thread, in microseconds. Null pointers are skipped.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_get_frame_time_stats(
    frontend: &SuperShuckieFrontend,
    average_micros: *mut u32,
    last_micros: *mut u32,
    max_micros: *mut u32,
    budget_micros: *mut u32,
    frames_over_budget: *mut u64
) {
    let stats = frontend.get_frame_time_stats();
    // SAFETY: the caller passes either null or valid, writable pointers.
    unsafe {
        if !average_micros.is_null() { *average_micros = stats.average_frame_micros; }
        if !last_micros.is_null() { *last_micros = stats.last_frame_micros; }
        if !max_micros.is_null() { *max_micros = stats.max_frame_micros; }
        if !budget_micros.is_null() { *budget_micros = stats.budget_micros; }
        if !frames_over_budget.is_null() { *frames_over_budget = stats.frames_over_budget; }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_get_elapsed_time(
    frontend: &SuperShuckieFrontend,
    elapsed_frames: *mut u32,
    elapsed_milliseconds: *mut u32
) {
    let elapsed_frames = unsafe { nullable_reference!(elapsed_frames) };
    let elapsed_milliseconds = unsafe { nullable_reference!(elapsed_milliseconds) };

    *elapsed_milliseconds = frontend.get_elapsed_milliseconds();
    *elapsed_frames = frontend.get_elapsed_frames();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_get_replay_playback_time(
    frontend: &SuperShuckieFrontend,
    total_frames: *mut u32,
    total_milliseconds: *mut u32
) -> bool {
    let total_frames = unsafe { nullable_reference!(total_frames) };
    let total_milliseconds = unsafe { nullable_reference!(total_milliseconds) };

    match frontend.get_replay_playback_stats() {
        Some(n) => {
            *total_frames = n.total_frames;
            *total_milliseconds = n.total_milliseconds;
            true
        },
        None => {
            *total_frames = 0;
            *total_milliseconds = 0;
            false
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_load_replay(
    frontend: &mut SuperShuckieFrontend,
    name: *const c_char,
    override_errors: bool,
    error: *mut u8,
    error_len: usize
) -> bool {
    let name = unsafe { CStr::from_ptr(name).to_str().expect("replay name is not UTF-8") };

    match frontend.load_replay_if_exists(name, override_errors) {
        Ok(_) => true,
        Err(e) => {
            write_str_to_data(e.as_str(), unsafe { from_raw_parts_mut(error, error_len) });
            false
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_continue_last_replay(
    frontend: &mut SuperShuckieFrontend,
    error: *mut u8,
    error_len: usize
) -> bool {
    match frontend.continue_last_replay() {
        Ok(_) => true,
        Err(e) => {
            write_str_to_data(e.as_str(), unsafe { from_raw_parts_mut(error, error_len) });
            false
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_can_continue_last_replay(
    frontend: &SuperShuckieFrontend
) -> bool {
    frontend.can_continue_last_replay()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_stop_replay_playback(
    frontend: &mut SuperShuckieFrontend
) {
    frontend.stop_replay_playback();
}

unsafe fn current_rom_or_null(frontend: &SuperShuckieFrontend, rom: *const c_char) -> Option<&str> {
    if rom.is_null() {
        frontend.get_current_rom_name()
    }
    else {
        Some(unsafe { CStr::from_ptr(rom) }.to_str().expect("save file not utf-8"))
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_get_control_settings(
    frontend: &SuperShuckieFrontend,
    emulator_type: u8
) -> *mut SuperShuckieControlSettings {
    let Ok(emulator_type) = SuperShuckieEmulatorType::try_from(emulator_type) else { panic!("Unknown emulator_type {emulator_type}") };
    Box::into_raw(Box::new(SuperShuckieControlSettings(frontend.get_control_settings(emulator_type).clone())))
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_set_control_settings(
    frontend: &mut SuperShuckieFrontend,
    settings: &SuperShuckieControlSettings,
    emulator_type: u8
) {
    let Ok(emulator_type) = SuperShuckieEmulatorType::try_from(emulator_type) else { panic!("Unknown emulator_type {emulator_type}") };
    frontend.set_control_settings(settings.0.clone(), emulator_type)
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_get_connected_controllers(
    frontend: &SuperShuckieFrontend
) -> *mut SuperShuckieStringArray {
    Box::into_raw(Box::new(SuperShuckieStringArray(frontend.get_connected_controllers())))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_connect_controller(
    frontend: &mut SuperShuckieFrontend,
    controller: *const c_char
) -> ConnectedControllerIndex {
    let controller_name = unsafe { CStr::from_ptr(controller).to_str().expect("controller name not UTF-8") };
    frontend.connect_controller(controller_name)
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_disconnect_controller(
    frontend: &mut SuperShuckieFrontend,
    controller: ConnectedControllerIndex
) {
    frontend.disconnect_controller(controller);
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_get_name_of_controller(
    frontend: &SuperShuckieFrontend,
    controller: ConnectedControllerIndex
) -> *const c_char {
    frontend.name_of_controller_c_str(controller).map(|i| i.as_ptr()).unwrap_or(null())
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_set_playback_frame(
    frontend: &mut SuperShuckieFrontend,
    frame: u32
) {
    frontend.go_to_replay_frame(frame)
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_advance_playback_frames(
    frontend: &mut SuperShuckieFrontend,
    frames: i32
) {
    frontend.advance_playback_frames(frames)
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_set_playback_frozen(
    frontend: &mut SuperShuckieFrontend,
    paused: bool
) {
    frontend.set_playback_frozen(paused)
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_get_replay_state(
    frontend: &SuperShuckieFrontend
) -> SuperShuckieReplayState {
    frontend.get_replay_state()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_get_gbc_mode(frontend: &SuperShuckieFrontend) -> GameBoyMode {
    frontend.get_gbc_mode()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_set_gbc_mode(frontend: &mut SuperShuckieFrontend, mode: u32) {
    if let Ok(m) = GameBoyMode::try_from(mode) {
        frontend.set_gbc_mode(m)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_is_sgb_enabled(frontend: &SuperShuckieFrontend) -> bool {
    frontend.is_sgb_enabled()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_set_sgb_enabled(frontend: &mut SuperShuckieFrontend, enabled: bool) {
    frontend.set_sgb_enabled(enabled);
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_set_touch(frontend: &mut SuperShuckieFrontend, enabled: bool, x: u8, y: u8) {
    frontend.set_touch(enabled.then_some((x, y)))
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_get_nds_date(frontend: &SuperShuckieFrontend, date: &mut NintendoDSDate) {
    *date = frontend.get_nds_date().get_cleaned();
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_set_nds_date(frontend: &mut SuperShuckieFrontend, date: &NintendoDSDate) {
    frontend.set_nds_date(*date);
}


#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_get_nds_jit(frontend: &SuperShuckieFrontend) -> bool {
    frontend.get_jit_enabled()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_set_nds_jit(frontend: &mut SuperShuckieFrontend, enabled: bool) {
    frontend.set_jit_enabled(enabled)
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_get_audio_enabled(frontend: &SuperShuckieFrontend) -> bool {
    frontend.get_audio_enabled()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_set_audio_enabled(frontend: &mut SuperShuckieFrontend, enabled: bool) {
    frontend.set_audio_enabled(enabled)
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_get_audio_muted(frontend: &SuperShuckieFrontend) -> bool {
    frontend.get_audio_muted()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_set_audio_muted(frontend: &mut SuperShuckieFrontend, muted: bool) {
    frontend.set_audio_muted(muted)
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_get_audio_volume(frontend: &SuperShuckieFrontend) -> u8 {
    frontend.get_audio_volume()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_set_audio_volume(frontend: &mut SuperShuckieFrontend, percent: u8) {
    frontend.set_audio_volume(percent)
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_get_audio_mute_when_sped_up(frontend: &SuperShuckieFrontend) -> bool {
    frontend.get_audio_mute_when_sped_up()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_set_audio_mute_when_sped_up(frontend: &mut SuperShuckieFrontend, mute: bool) {
    frontend.set_audio_mute_when_sped_up(mute)
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_get_audio_latency_ms(frontend: &SuperShuckieFrontend) -> u16 {
    frontend.get_audio_latency_ms()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_set_audio_latency_ms(frontend: &mut SuperShuckieFrontend, latency_ms: u16) {
    frontend.set_audio_latency_ms(latency_ms)
}

/// Hand out a retained reference to the frontend's audio ring; the C side owns one strong count
/// per call and gives it back with `supershuckie_audio_output_release`.
#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_retain_audio_output(frontend: &SuperShuckieFrontend) -> *const AudioOutput {
    Arc::into_raw(frontend.audio_output().clone())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_audio_output_release(audio: *const AudioOutput) {
    if !audio.is_null() {
        drop(unsafe { Arc::from_raw(audio) });
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_audio_output_read(audio: &AudioOutput, out: *mut i16, frames: usize) -> usize {
    if out.is_null() || frames == 0 {
        return 0
    }
    audio.read(unsafe { from_raw_parts_mut(out, frames * 2) })
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_audio_output_clear(audio: &AudioOutput) {
    audio.clear()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_audio_output_sample_rate() -> u32 {
    AUDIO_SAMPLE_RATE
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_audio_output_queued_frames(audio: &AudioOutput) -> usize {
    audio.queued_frames()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_audio_output_speed(audio: &AudioOutput) -> f32 {
    audio.speed()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_audio_output_fast_forward_scales_pitch(audio: &AudioOutput) -> bool {
    audio.fast_forward_scales_pitch()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_get_swap_nds_screens(frontend: &SuperShuckieFrontend) -> bool {
    frontend.get_swap_nds_screens()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_set_swap_nds_screens(frontend: &mut SuperShuckieFrontend, swap: bool) {
    frontend.set_swap_nds_screens(swap)
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_get_recent_roms(frontend: &SuperShuckieFrontend) -> *mut SuperShuckieStringArray {
    Box::into_raw(Box::new(SuperShuckieStringArray(frontend.get_recent_roms())))
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_clear_recent_roms(frontend: &mut SuperShuckieFrontend) {
    frontend.clear_recent_roms()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_get_current_data_directory(
    frontend: &SuperShuckieFrontend,
    dir: *mut u8,
    dir_len: usize
) -> usize {
    let path = frontend.get_dir_for_current_rom().unwrap_or_else(|| frontend.get_user_dir());
    let path_bytes = path.as_c_str().to_bytes_with_nul();
    let path_bytes_len = path_bytes.len();

    if dir_len >= path_bytes_len {
        unsafe { from_raw_parts_mut(dir, path_bytes_len) }.copy_from_slice(path_bytes)
    }

    path_bytes_len
}

/// Get the screenshots directory for the current ROM, creating it if needed.
///
/// Writes a NUL-terminated path into `dir` (up to `dir_len` bytes) and returns the number of bytes
/// the path needs (including the NUL). Returns 0 if no ROM is loaded or the directory can't be made.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn supershuckie_frontend_get_screenshot_directory(
    frontend: &SuperShuckieFrontend,
    dir: *mut u8,
    dir_len: usize
) -> usize {
    let Some(path) = frontend.get_screenshots_dir_for_current_rom() else {
        return 0;
    };
    let path_bytes = path.as_c_str().to_bytes_with_nul();
    let path_bytes_len = path_bytes.len();

    if dir_len >= path_bytes_len {
        unsafe { from_raw_parts_mut(dir, path_bytes_len) }.copy_from_slice(path_bytes)
    }

    path_bytes_len
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_reload_core(
    frontend: &mut SuperShuckieFrontend
) {
    frontend.reload_core();
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_get_emulator_type(
    frontend: &SuperShuckieFrontend
) -> c_int {
    frontend.get_emulator_type().map(|i| i as c_int).unwrap_or(-1)
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_get_emulator_type_name(
    emulator_type: u8
) -> *const c_char {
    match SuperShuckieEmulatorType::try_from(emulator_type) {
        Ok(n) => n.name_cstr().as_ptr(),
        _ => null()
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_emulator_type_uses_shared_config(
    emulator_type: u8
) -> bool {
    match SuperShuckieEmulatorType::try_from(emulator_type) {
        Ok(n) => n.uses_shared_config(),
        _ => false
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_get_ignore_speed_changes_in_replay(
    frontend: &SuperShuckieFrontend
) -> bool {
    frontend.get_ignore_speed_changes_in_replays()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_set_ignore_speed_changes_in_replay(
    frontend: &mut SuperShuckieFrontend,
    ignored: bool
) {
    frontend.set_ignore_speed_changes_in_replays(ignored)
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_get_auto_resync_keyframes_in_replay(
    frontend: &SuperShuckieFrontend
) -> bool {
    frontend.get_auto_resync_keyframes_in_replay()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_set_auto_resync_keyframes_in_replay(
    frontend: &mut SuperShuckieFrontend,
    ignored: bool
) {
    frontend.set_auto_resync_keyframes_in_replay(ignored)
}

/// Get the zstd compression level used for new recordings and replay conversions.
#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_get_replay_compression_level(frontend: &SuperShuckieFrontend) -> i32 {
    frontend.get_replay_compression_level()
}

/// Set the zstd compression level used for new recordings and replay conversions (clamped to 1..=22).
#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_set_replay_compression_level(frontend: &mut SuperShuckieFrontend, level: i32) {
    frontend.set_replay_compression_level(level)
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_get_disable_save_states_when_recording(
    frontend: &SuperShuckieFrontend
) -> bool {
    frontend.get_disable_save_states_when_recording()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_set_disable_save_states_when_recording(
    frontend: &mut SuperShuckieFrontend,
    disabled: bool
) {
    frontend.set_disable_save_states_when_recording(disabled)
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_get_disable_speed_changes_when_recording(
    frontend: &SuperShuckieFrontend
) -> bool {
    frontend.get_disable_speed_changes_when_recording()
}

#[unsafe(no_mangle)]
pub extern "C" fn supershuckie_frontend_set_disable_speed_changes_when_recording(
    frontend: &mut SuperShuckieFrontend,
    disabled: bool
) {
    frontend.set_disable_speed_changes_when_recording(disabled)
}
