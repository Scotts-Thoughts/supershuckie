//! End-to-end checks of replay bookmarks through the frontend and its REST API, against a real
//! threaded core: bookmarks placed while recording (plain, keyframe, range, REST with a new type),
//! read back from the closed file, seeking to them during playback, playback edits saved into the
//! file, and a pre-v5 replay upgraded on its first edit.
//!
//! ```text
//! bookmark_frontend_smoke <rom.gbc | rom.gba>
//! ```
//!
//! Needs 127.0.0.1:30158 free for the REST checks (they are skipped otherwise). Link it like
//! `supershuckie-core`'s `bookmark_smoke` (see that file's header).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::num::NonZeroU8;
use std::path::Path;
use std::time::{Duration, Instant};

use supershuckie_core::emulator::ScreenData;
use supershuckie_frontend::bookmarks::{BookmarkError, BookmarkParams, BookmarkTypeUpsert};
use supershuckie_frontend::{ScreenInfo, SuperShuckieFrontend, SuperShuckieFrontendCallbacks};
use supershuckie_replay_recorder::replay_file::playback::{BookmarkTableSource, ReplayFilePlayer};
use supershuckie_replay_recorder::replay_file::{ReplayHeaderRaw, REPLAY_VERSION};

struct NoScreen;

impl SuperShuckieFrontendCallbacks for NoScreen {
    fn refresh_screens(&mut self, _: &[ScreenData]) {}
    fn change_video_mode(&mut self, _: &[ScreenInfo], _: NonZeroU8) {}
}

fn tick_for(frontend: &mut SuperShuckieFrontend, duration: Duration) {
    let until = Instant::now() + duration;
    while Instant::now() < until {
        frontend.tick().expect("tick");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn tick_until(frontend: &mut SuperShuckieFrontend, what: &str, mut done: impl FnMut(&mut SuperShuckieFrontend) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done(frontend) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        frontend.tick().expect("tick");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// GET `path` from the REST API while ticking the frontend (which serves the request). Returns the
/// status and body.
fn rest(frontend: &mut SuperShuckieFrontend, path: &str) -> (u16, String) {
    let request = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    let worker = std::thread::spawn(move || {
        let mut stream = TcpStream::connect("127.0.0.1:30158").expect("connect");
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    });
    while !worker.is_finished() {
        frontend.tick().expect("tick");
        std::thread::sleep(Duration::from_millis(1));
    }
    let response = worker.join().unwrap();
    let status = response.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let body = response.split_once("\r\n\r\n").map(|(_, b)| b.to_owned()).unwrap_or_default();
    // Chunked bodies are not expected from rouille for these small replies.
    (status, body)
}

fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("bad JSON {body:?}: {e}"))
}

fn file_table(path: &Path) -> supershuckie_replay_recorder::BookmarkTable {
    try_file_table(path).unwrap_or_else(|| {
        let bytes = std::fs::read(path).unwrap();
        let header = ReplayHeaderRaw::from_bytes(bytes[..2048].try_into().unwrap());
        let version = header.replay_version;
        let player = ReplayFilePlayer::new(&bytes, false);
        panic!(
            "{} has no valid bookmark section: {} bytes, version {version}, stream end {:?}, player {:?}",
            path.display(),
            bytes.len(),
            header.packet_stream_end(),
            player.map(|p| (p.bookmark_table_source(), p.bookmark_section_error().cloned(), p.get_total_frames()))
        )
    })
}

/// The bookmark section of the file, if it is complete (a write may be in progress).
fn try_file_table(path: &Path) -> Option<supershuckie_replay_recorder::BookmarkTable> {
    let player = ReplayFilePlayer::new(std::fs::read(path).ok()?, false).ok()?;
    (player.bookmark_table_source() == BookmarkTableSource::Section).then(|| player.bookmark_table().clone())
}

fn main() {
    let rom = std::path::absolute(std::env::args().nth(1).expect("usage: bookmark_frontend_smoke <rom>")).unwrap();
    let rom_file_name = rom.file_name().unwrap().to_str().unwrap().to_owned();

    let dir = std::env::temp_dir().join("supershuckie-frontend-bookmark-smoke");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut frontend = SuperShuckieFrontend::new(dir.join("data"), dir.join("config"), Box::new(NoScreen));
    let rest_available = frontend.get_external_commands_enabled().is_ok_and(|enabled| enabled);
    if !rest_available {
        println!("REST API unavailable (port 30158 in use?); skipping the REST checks");
    }

    frontend.load_rom(&rom).expect("load rom");
    frontend.set_auto_pause_on_record_setting(false);
    frontend.set_paused(false);
    tick_for(&mut frontend, Duration::from_millis(500));

    // Without a replay, bookmarks are unavailable.
    assert_eq!(frontend.add_bookmark(BookmarkParams::default(), false).unwrap_err(), BookmarkError::NoReplay);

    // --- Recording. ---
    let replay_name = frontend.start_recording_replay(Some("smoke")).expect("record").to_string();
    frontend.set_paused(false);
    tick_until(&mut frontend, "300 recorded frames", |f| f.get_elapsed_frames() >= 300);

    let first = frontend.add_bookmark(BookmarkParams { name: Some("First".into()), ..Default::default() }, false).unwrap();
    assert_eq!(first.name, "First");
    let generic = frontend.add_bookmark(BookmarkParams::default(), false).unwrap();
    assert_eq!(generic.name, "Bookmark 1");
    let fast = frontend.add_bookmark(BookmarkParams { keyframe: Some(true), ..Default::default() }, false).unwrap();
    assert!(fast.keyframe);
    println!("recording: placed {:?} at {}, {:?} at {}, keyframe bookmark at {}", first.name, first.in_frame, generic.name, generic.in_frame, fast.in_frame);

    let (range, started) = frontend.toggle_range_bookmark(BookmarkParams::default(), false).unwrap();
    assert!(started && range.out_frame.is_none());
    tick_until(&mut frontend, "the range to grow", |f| f.get_elapsed_frames() as u64 >= range.in_frame + 60);
    let (range, started) = frontend.toggle_range_bookmark(BookmarkParams::default(), false).unwrap();
    assert!(!started && range.out_frame.unwrap() >= range.in_frame + 60);

    // Seeking is for playback.
    assert!(matches!(frontend.go_to_bookmark(first.id, false), Err(BookmarkError::WrongState(_))));

    let mut deaths_id = None;
    if rest_available {
        let (status, body) = rest(&mut frontend, "/add-bookmark?name=Oops&type=Deaths");
        assert_eq!(status, 200, "{body}");
        let added = json(&body);
        assert_eq!(added["type"]["name"], "Deaths");
        deaths_id = Some(added["type"]["id"].as_str().unwrap().to_owned());

        let (status, body) = rest(&mut frontend, "/add-bookmark?frame=10&keyframe=true");
        assert_eq!(status, 400, "a keyframe bookmark at an explicit frame is refused: {body}");

        let (status, body) = rest(&mut frontend, &format!("/update-bookmark?id={}&name=Renamed", first.id));
        assert_eq!((status, json(&body)["name"].as_str()), (200, Some("Renamed")));

        let (status, body) = rest(&mut frontend, "/bookmarks");
        assert_eq!(status, 200);
        let view = json(&body);
        assert_eq!(view["state"], "recording");
        assert_eq!(view["bookmarks"].as_array().unwrap().len(), 5);

        let (status, body) = rest(&mut frontend, "/stats");
        assert_eq!(status, 200);
        assert_eq!(json(&body)["bookmark_generation"].as_u64(), Some(frontend.bookmark_generation()));
        println!("REST while recording ok");
    }

    tick_for(&mut frontend, Duration::from_millis(300));
    let expected = frontend.bookmarks_view();
    frontend.stop_recording_replay().expect("stop recording");

    let replay_path = dir.join("data").join(format!("{rom_file_name}-data")).join("replays").join(&replay_name);
    let on_disk = file_table(&replay_path);
    assert_eq!(on_disk.bookmarks.iter().map(|b| (b.id, b.name.as_str(), b.in_frame, b.out.map(|o| o.0), b.keyframe)).collect::<Vec<_>>(),
        expected.bookmarks.iter().map(|b| (b.id, b.name.as_str(), b.in_frame, b.out_frame, b.keyframe)).collect::<Vec<_>>());
    if let Some(id) = deaths_id.as_ref() {
        assert!(on_disk.types.iter().any(|t| format!("{:016x}", t.id) == *id && t.name == "Deaths"), "the replay records the type");
    }
    println!("recording closed with {} bookmarks in its section", on_disk.bookmarks.len());

    // --- Playback. ---
    let replay_stem = replay_name.trim_end_matches(".replay");
    assert!(frontend.load_replay_if_exists(replay_stem, false).expect("load replay"));
    let view = frontend.bookmarks_view();
    assert_eq!((view.state, view.editable, view.replay_version), ("playback", true, Some(REPLAY_VERSION)));
    assert_eq!(view.bookmarks.len(), on_disk.bookmarks.len());

    frontend.go_to_bookmark(fast.id, false).unwrap();
    tick_until(&mut frontend, "the seek to the keyframe bookmark", |f| f.get_elapsed_frames() as u64 == fast.in_frame);
    frontend.go_to_bookmark(range.id, true).unwrap();
    tick_until(&mut frontend, "the seek to the range's out frame", |f| Some(f.get_elapsed_frames() as u64) == range.out_frame);
    println!("seeks to bookmarks ok");

    // A keyframe bookmark while watching snaps to a keyframe; plain edits are written to the file.
    let snapped = frontend.add_bookmark(BookmarkParams { name: Some("Snapped".into()), keyframe: Some(true), ..Default::default() }, false).unwrap();
    assert!(snapped.keyframe && snapped.in_frame <= range.out_frame.unwrap());
    frontend.update_bookmark(generic.id, BookmarkParams { out: Some("now".into()), ..Default::default() }, false).unwrap();
    frontend.delete_bookmark(first.id, false).unwrap();
    tick_until(&mut frontend, "the playback edits to be written", |_| try_file_table(&replay_path).is_some_and(|t| t.get(snapped.id).is_some() && t.get(first.id).is_none()));
    let on_disk = file_table(&replay_path);
    assert!(on_disk.get(first.id).is_none());
    assert!(on_disk.get(generic.id).unwrap().out.is_some());

    if rest_available {
        let (status, body) = rest(&mut frontend, &format!("/go-to-bookmark?id={}", generic.id));
        assert_eq!(status, 204, "{body}");
        tick_until(&mut frontend, "the REST seek", |f| f.get_elapsed_frames() as u64 == generic.in_frame);
        let (status, _) = rest(&mut frontend, "/delete-bookmark?id=99999");
        assert_eq!(status, 404);
        println!("REST during playback ok");
    }

    // Renaming a type shows everywhere at once, and the replay's record follows on its next save.
    if let Some(id) = deaths_id.as_ref() {
        frontend.upsert_bookmark_type(BookmarkTypeUpsert { id: Some(id.clone()), name: Some("Faints".into()), color: Some("#E53935".into()) }).unwrap();
        assert!(frontend.bookmarks_view().bookmarks.iter().any(|b| b.kind.as_ref().is_some_and(|k| k.name == "Faints")));
        frontend.update_bookmark(generic.id, BookmarkParams { name: Some("Generic, renamed".into()), ..Default::default() }, false).unwrap();
    }

    // Stopping playback saves what is still pending.
    let before_stop = frontend.bookmarks_view();
    frontend.close_replay();
    let on_disk = file_table(&replay_path);
    assert_eq!(on_disk.bookmarks.iter().map(|b| (b.id, b.name.as_str())).collect::<Vec<_>>(), before_stop.bookmarks.iter().map(|b| (b.id, b.name.as_str())).collect::<Vec<_>>());
    if deaths_id.is_some() {
        assert!(on_disk.types.iter().any(|t| t.name == "Faints" && t.color == 0xE53935), "{:?}", on_disk.types);
    }
    println!("playback edits saved");

    // --- Resuming carries the bookmarks up to the resume frame, keyframes included. ---
    let source_table = file_table(&replay_path);
    let resume_at = range.in_frame + 10;
    let resumed_name = frontend.resume_recording_from_replay(replay_stem, Some(resume_at as u32), None).expect("resume").to_string();
    frontend.set_paused(false);
    tick_for(&mut frontend, Duration::from_millis(300));
    assert_eq!(frontend.bookmarks_view().state, "recording");
    frontend.stop_recording_replay().expect("stop recording");
    let resumed_path = replay_path.with_file_name(&resumed_name);
    let resumed = file_table(&resumed_path);
    assert_eq!(resumed, source_table.truncated_to(resume_at), "resumed bookmarks");
    assert!(resumed.get(range.id).is_some_and(|b| b.out.is_none()), "the range crossing the resume frame lost its out frame");
    let mut resumed_player = ReplayFilePlayer::new(std::fs::read(&resumed_path).unwrap(), false).unwrap();
    for frame in resumed.keyframe_anchor_frames().collect::<Vec<_>>() {
        assert!(resumed_player.keyframe_is_stored_full(frame).unwrap(), "keyframe {frame} of a keyframe bookmark is full in the resumed replay");
    }
    println!("resumed replay carries {} bookmarks", resumed.bookmarks.len());

    // --- A pre-v5 replay is upgraded on its first edit, after confirmation. ---
    let bytes = std::fs::read(&replay_path).unwrap();
    let header = ReplayHeaderRaw::from_bytes(bytes[..2048].try_into().unwrap());
    let mut old = bytes[..header.packet_stream_end().unwrap() as usize].to_vec();
    old[4..8].copy_from_slice(&4u32.to_le_bytes());
    old[0x3A8..0x3B0].fill(0);
    let old_path = replay_path.with_file_name("old-format.replay");
    std::fs::write(&old_path, &old).unwrap();

    assert!(frontend.load_replay_if_exists("old-format", false).expect("load old replay"));
    let view = frontend.bookmarks_view();
    assert!(view.needs_upgrade && view.bookmarks.is_empty(), "an old replay's in-stream snapshots are not read");
    assert_eq!(frontend.add_bookmark(BookmarkParams::default(), false).unwrap_err(), BookmarkError::NeedsUpgradeConfirmation { version: 4 });
    frontend.add_bookmark(BookmarkParams { name: Some("After upgrade".into()), ..Default::default() }, true).unwrap();
    frontend.flush_bookmarks().unwrap();
    let upgraded = std::fs::read(&old_path).unwrap();
    let version = ReplayHeaderRaw::from_bytes(upgraded[..2048].try_into().unwrap()).replay_version;
    assert_eq!(version, REPLAY_VERSION);
    assert_eq!(file_table(&old_path).bookmarks[0].name, "After upgrade");
    frontend.close_replay();
    println!("old replay upgraded on its first bookmark");

    frontend.unload_rom();
    println!("all frontend bookmark checks passed");
    if std::env::var_os("SUPERSHUCKIE_SMOKE_KEEP").is_some() {
        println!("kept {}", dir.display());
    }
    else {
        let _ = std::fs::remove_dir_all(&dir);
    }
}
