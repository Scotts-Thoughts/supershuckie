//! `--probe <replay> [--layout N]`: describe a recording as JSON without a ROM or a core.

use std::path::Path;

use serde_json::{json, Map, Value};
use supershuckie_core::emulator::AUDIO_SAMPLE_RATE;

use crate::source::{frame_rate, geometry, layout_from_byte, open_replay, ReplaySummary};

/// The JSON object for `replay`, or the error message to report.
pub fn probe(replay: &Path, layout: u8) -> Result<Value, String> {
    let player = open_replay(replay)?;
    let summary = ReplaySummary::of(&player);
    let layout = layout_from_byte(layout);
    let (width, height) = geometry(summary.console, layout);
    let (fps_num, fps_den) = frame_rate(summary.console);

    let bookmarks: Vec<Value> = summary
        .bookmarks
        .iter()
        .map(|(name, frame)| json!({ "name": name, "frame": frame }))
        .collect();

    let mut counters = Map::new();
    for (name, value) in &summary.counters {
        counters.insert(name.clone(), json!(value));
    }

    Ok(json!({
        "console": summary.console.name(),
        "width": width,
        "height": height,
        "fps_num": fps_num,
        "fps_den": fps_den,
        "frames": summary.frames,
        "sample_rate": AUDIO_SAMPLE_RATE,
        "rom_hash": summary.rom_hash_hex(),
        "rom_filename": summary.rom_filename,
        "rom_name": summary.rom_name,
        "core_recorded": summary.core_recorded,
        "keyframes": summary.keyframes.len(),
        "bookmarks": bookmarks,
        "crop": summary.crop.map(|(s, e)| json!([s, e])).unwrap_or(Value::Null),
        "counters": Value::Object(counters),
        "format_version": summary.format_version,
    }))
}
