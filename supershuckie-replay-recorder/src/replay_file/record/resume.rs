//! Resume-from-replay support.
//!
//! See [`build_resumed_recorder`]. A resumed file is an ordinary v3 file; no format change is
//! required. Timing is preserved by carrying the absolute `elapsed_millis` forward and re-emitting
//! identical `NextFrame` deltas.

use alloc::borrow::Cow;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use super::ReplayFileRecorder;
use super::ReplayFileRecorderSettings;
use super::ReplayFileSink;
use super::ReplayFileWriteError;
use super::super::playback::ReplayFilePlayer;
use super::super::playback::ReplaySeekError;
use crate::{ByteVec, Counter, InputBuffer, Packet, Speed, TimestampMillis, UnsignedInteger};

/// Counter snapshot + position + input/speed at the resume point.
#[derive(Clone, Debug, Default)]
#[allow(missing_docs)]
pub struct ResumeInfo {
    pub elapsed_frames: UnsignedInteger,
    pub elapsed_millis: TimestampMillis,
    pub speed: Speed,
    /// Encoded input bytes in effect at the resume frame.
    pub input: InputBuffer,
    /// Exact counter values at the resume frame.
    pub counters: Vec<Counter>,
}

/// How to carry the header crop / timing markers into the resumed file.
#[derive(Copy, Clone, PartialEq, Debug, Default)]
pub enum ResumeCropPolicy {
    /// Carry `crop_start` + `crop_timer_offset` forward when `crop_start_frame <= resume_frame`;
    /// always drop `crop_end` (the run is being extended and will be re-marked).
    #[default]
    PreserveStartDropEnd,
    /// Drop all crop markers; the user re-marks start and end.
    DropAll,
    /// Carry all markers forward verbatim. Only meaningful when `resume_frame >= crop_end_frame`.
    PreserveAll,
}

/// Error that can occur when building a resumed recorder.
#[derive(Clone, Debug)]
#[allow(missing_docs)]
pub enum ReplayResumeError {
    /// The source player produced no usable frame-0 keyframe.
    BadSource { explanation: Cow<'static, str> },
    /// Writing the resumed prefix failed.
    Write(ReplayFileWriteError),
    /// Reading/seeking the source failed.
    Read(ReplaySeekError),
}

/// Build a recorder primed to continue from `resume_at_frame` (`None` = end of replay).
///
/// Writes header + patch + the prefix `[0 ..= target]` into the two sinks and returns the OPEN
/// recorder (do not close it — the caller continues recording) along with a [`ResumeInfo`]
/// describing the resume point.
///
/// # How the prefix is built
///
/// For a finished `.replay` file (every top-level packet is a completed compressed blob) this takes
/// a **blob-copy fast path**: every blob that ends *before* the resume boundary is copied into the
/// output **verbatim** — no decompression, no recompression — and only the single blob that
/// straddles the boundary is decompressed and re-fed packet-by-packet up to `target`. The resume
/// cost is therefore O(one blob), not O(whole file). This is what makes resuming a long Nintendo DS
/// replay tractable: NDS save states (hence blobs) are megabytes each, so decompressing and
/// recompressing the entire prefix — as a naive re-feed would — costs gigabytes of work and RAM.
///
/// If the source is not laid out as all-blobs (e.g. it has a trailing uncompressed region), this
/// falls back to re-feeding the whole prefix from frame 0, which is always correct.
///
/// `source` should be a player instance dedicated to this call (its cursor is consumed).
pub fn build_resumed_recorder<FS: ReplayFileSink, TS: ReplayFileSink>(
    source: &mut ReplayFilePlayer,
    resume_at_frame: Option<UnsignedInteger>,
    settings: ReplayFileRecorderSettings,
    crop_policy: ResumeCropPolicy,
    final_sink: FS,
    temp_sink: TS,
) -> Result<(ReplayFileRecorder<FS, TS>, ResumeInfo), ReplayResumeError> {
    let total = source.get_total_frames();
    let target = resume_at_frame.unwrap_or(total).min(total);

    // Apply the crop policy to the source metadata and carry the patch forward.
    let metadata = source
        .get_replay_metadata()
        .clone()
        .with_resume_crop(target, crop_policy);

    let mut patch = ByteVec::new();
    if let Some(patch_bytes) = source.get_patch_data() {
        patch.extend_from_slice(patch_bytes);
    }

    // Build a blank recorder: header + patch written to both sinks, but NO frame-0 keyframe. The
    // leading data is filled in below — either copied verbatim from the source's blobs, or re-fed.
    // The starting input/speed are placeholders; they are overwritten by `prime_for_resume` from the
    // keyframe we actually resume the re-feed from.
    let mut recorder = ReplayFileRecorder::new_blank(
        metadata,
        patch,
        settings,
        InputBuffer::new(),
        Speed::default(),
        final_sink,
        temp_sink,
    )
    .map_err(ReplayResumeError::Write)?;

    let all_blobs = source
        .all_uncompressed_packets()
        .iter()
        .all(|p| matches!(p, Packet::CompressedBlob { .. }));

    // Determine where the re-fed (decompressed) portion begins. On the fast path we first copy every
    // completed blob that ends before the boundary; the re-feed then starts at the first keyframe of
    // the straddling blob. On the fallback path we re-feed everything from frame 0.
    let start_frame = if all_blobs {
        copy_completed_blobs_before_boundary(&mut recorder, source, target)?
    } else {
        0
    };

    let resume_info = prime_and_refeed(&mut recorder, source, total, start_frame, target)?;

    Ok((recorder, resume_info))
}

/// Copy every completed compressed blob that ends *before* the resume boundary into `recorder`
/// verbatim, and return the frame index at which the re-feed of the straddling boundary blob should
/// begin (the first keyframe of that blob).
///
/// The boundary blob is the first whose `elapsed_frames_end >= target`; it always exists because
/// `target <= total` (the last blob ends at `total`). Since `target` lies within that blob, the
/// stop condition in [`prime_and_refeed`] fires before the cursor can spill into any later blob, so
/// later blobs are correctly dropped from the prefix.
fn copy_completed_blobs_before_boundary<FS: ReplayFileSink, TS: ReplayFileSink>(
    recorder: &mut ReplayFileRecorder<FS, TS>,
    source: &ReplayFilePlayer,
    target: UnsignedInteger,
) -> Result<UnsignedInteger, ReplayResumeError> {
    // Fallback: if no blob reaches the target (shouldn't happen given clamping), re-feed from the
    // last frame we copied up to.
    let mut start_frame = 0u64;

    for packet in source.all_uncompressed_packets() {
        let Packet::CompressedBlob { elapsed_frames_start, elapsed_frames_end, keyframes, .. } = packet else {
            // Not the all-blobs layout after all; stop copying and let the re-feed take over from
            // here.
            break;
        };

        if *elapsed_frames_end >= target {
            // This blob straddles (or ends exactly at) the boundary: re-feed it instead of copying.
            return Ok(keyframes
                .first()
                .map(|k| k.elapsed_frames)
                .unwrap_or(*elapsed_frames_start));
        }

        recorder
            .append_compressed_blob_verbatim(packet)
            .map_err(ReplayResumeError::Write)?;
        start_frame = *elapsed_frames_end;
    }

    Ok(start_frame)
}

/// Position `source` at the keyframe `start_frame`, prime `recorder` to continue from it, and re-feed
/// the source's flat packet stream into `recorder` up to and including frame `target`, returning the
/// resulting [`ResumeInfo`].
///
/// The keyframe at `start_frame` is written first as a full (undiffed) keyframe — required as the
/// first packet of the in-progress blob — and its metadata seeds the running input/speed/counter
/// state, so continuity holds whether `start_frame` is 0 (fallback) or the start of a boundary blob
/// (fast path).
fn prime_and_refeed<FS: ReplayFileSink, TS: ReplayFileSink>(
    recorder: &mut ReplayFileRecorder<FS, TS>,
    source: &mut ReplayFilePlayer,
    total: UnsignedInteger,
    start_frame: UnsignedInteger,
    target: UnsignedInteger,
) -> Result<ResumeInfo, ReplayResumeError> {
    let end_mode = target == total;

    source.go_to_keyframe(start_frame).map_err(ReplayResumeError::Read)?;

    // The first packet must be the keyframe at `start_frame`. Clone everything out before any
    // further next_packet() calls (which borrow the player mutably).
    let (state0, kf0) = {
        let first = source
            .next_packet()
            .map_err(|error| ReplayResumeError::Read(ReplaySeekError::ReadError { error }))?;

        match first {
            Some(Packet::Keyframe { metadata, state }) if metadata.elapsed_frames == start_frame => {
                (state.clone(), metadata.clone())
            }
            _ => {
                return Err(ReplayResumeError::BadSource {
                    explanation: Cow::Borrowed("resume start was not a keyframe at the expected frame"),
                })
            }
        }
    };

    // Prime the recorder's running state to this keyframe, then write it as the first full keyframe
    // of the in-progress blob.
    recorder.prime_for_resume(&kf0);
    recorder
        .insert_keyframe(state0, kf0.elapsed_millis)
        .map_err(ReplayResumeError::Write)?;

    let mut running_ms: u64 = kf0.elapsed_millis.0;
    let mut cur_frames: u64 = kf0.elapsed_frames;
    let mut cur_input: InputBuffer = kf0.input.clone();
    let mut cur_speed: Speed = kf0.speed;
    // Counters are cumulative in keyframe metadata, so the boundary keyframe already reflects every
    // increment in the verbatim-copied blobs before it; we only fold in increments re-fed afterward.
    let mut counter_map: BTreeMap<String, i64> =
        kf0.counters.iter().map(|c| (c.name.clone(), c.value)).collect();

    loop {
        // Each iteration must finish using the borrowed packet (cloning out owned data) before the
        // next next_packet() call. We compute an "action" closure-free by extracting owned data.
        enum Action {
            Stop,
            NextFrame(u64),
            SetInput(InputBuffer),
            SetSpeed(Speed),
            WriteMemory(u64, ByteVec),
            ResetConsole,
            LoadSaveState(ByteVec),
            Bookmark(String),
            Keyframe(ByteVec, TimestampMillis),
            IncrementCounter(String, i64),
            Skip,
        }

        let action = match source.next_packet() {
            Ok(None) => break,
            Ok(Some(packet)) => match packet {
                Packet::NextFrame { timestamp_delta } => {
                    if cur_frames + 1 > target {
                        Action::Stop
                    } else {
                        Action::NextFrame(timestamp_delta.0)
                    }
                }
                Packet::ChangeInput { data } => Action::SetInput(data.clone()),
                Packet::ChangeSpeed { speed } => Action::SetSpeed(*speed),
                Packet::WriteMemory { address, data } => {
                    Action::WriteMemory(*address, data.clone())
                }
                Packet::ResetConsole => Action::ResetConsole,
                Packet::LoadSaveState { state } => Action::LoadSaveState(state.clone()),
                Packet::Bookmark { metadata } => Action::Bookmark(metadata.name.clone()),
                Packet::Keyframe { metadata, state } => {
                    Action::Keyframe(state.clone(), metadata.elapsed_millis)
                }
                Packet::IncrementCounter { name, delta } => {
                    Action::IncrementCounter(name.clone(), *delta)
                }
                Packet::NoOp => Action::Skip,
                Packet::DeltaKeyframe { .. } | Packet::CompressedBlob { .. } => {
                    return Err(ReplayResumeError::BadSource {
                        explanation: Cow::Borrowed(
                            "unexpected DeltaKeyframe/CompressedBlob from player",
                        ),
                    })
                }
            },
            Err(error) => {
                if end_mode {
                    // Graceful end (corruption tolerance in end mode).
                    break;
                }
                return Err(ReplayResumeError::Read(ReplaySeekError::ReadError { error }));
            }
        };

        match action {
            Action::Stop => break,
            Action::NextFrame(delta) => {
                running_ms += delta;
                recorder
                    .next_frame(running_ms.into())
                    .map_err(ReplayResumeError::Write)?;
                cur_frames += 1;
            }
            Action::SetInput(data) => {
                recorder
                    .set_input(data.clone())
                    .map_err(ReplayResumeError::Write)?;
                cur_input = data;
            }
            Action::SetSpeed(speed) => {
                recorder.set_speed(speed).map_err(ReplayResumeError::Write)?;
                cur_speed = speed;
            }
            Action::WriteMemory(address, data) => {
                recorder
                    .write_memory(address, data)
                    .map_err(ReplayResumeError::Write)?;
            }
            Action::ResetConsole => {
                recorder.reset_console().map_err(ReplayResumeError::Write)?;
            }
            Action::LoadSaveState(state) => {
                recorder
                    .load_save_state(state)
                    .map_err(ReplayResumeError::Write)?;
            }
            Action::Bookmark(name) => {
                recorder.add_bookmark(name).map_err(ReplayResumeError::Write)?;
            }
            Action::Keyframe(state, elapsed_millis) => {
                recorder
                    .insert_keyframe(state, elapsed_millis)
                    .map_err(ReplayResumeError::Write)?;
            }
            Action::IncrementCounter(name, delta) => {
                recorder
                    .change_counter(name.clone(), delta)
                    .map_err(ReplayResumeError::Write)?;
                let entry = counter_map.entry(name).or_insert(0);
                *entry = entry.wrapping_add(delta);
            }
            Action::Skip => {}
        }
    }

    let counters: Vec<Counter> = counter_map
        .into_iter()
        .map(|(name, value)| Counter { name, value })
        .collect();

    Ok(ResumeInfo {
        elapsed_frames: cur_frames,
        elapsed_millis: running_ms.into(),
        speed: cur_speed,
        input: cur_input,
        counters,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay_file::playback::ReplayFilePlayer;
    use crate::replay_file::record::{ReplayFileRecorder, ReplayFileRecorderSettings};
    use crate::replay_file::{ReplayConsoleType, ReplayFileMetadata};
    use crate::{ByteVec, InputBuffer, Packet, Speed};
    use alloc::string::ToString;
    use alloc::vec::Vec;

    fn small_settings() -> ReplayFileRecorderSettings {
        ReplayFileRecorderSettings {
            // Small enough to force multiple blob splits during the source build & re-feed.
            minimum_uncompressed_bytes_per_blob: 256,
            compression_level: 1,
        }
    }

    fn make_metadata() -> ReplayFileMetadata {
        ReplayFileMetadata {
            console_type: ReplayConsoleType::GameBoy,
            rom_name: "TEST".to_string(),
            rom_filename: "test.gb".to_string(),
            emulator_core_name: "test-core 1.0".to_string(),
            ..Default::default()
        }
    }

    fn ib(bytes: &[u8]) -> InputBuffer {
        let mut v = InputBuffer::new();
        v.extend_from_slice(bytes);
        v
    }

    fn state_for(frame: u64) -> ByteVec {
        // Distinct-but-similar states so diff/de-diff exercises the path.
        let mut v = ByteVec::new();
        for i in 0..32u8 {
            v.push(i.wrapping_add(frame as u8));
        }
        v
    }

    /// The deltas used per frame in the source, indexed by frame number (1-based for the i-th
    /// NextFrame). We use a deterministic varying pattern.
    fn delta_for(frame_one_based: u64) -> u64 {
        // Vary: 16, 17, 16, 17, ...  with occasional spikes.
        let base = 16 + (frame_one_based % 2);
        if frame_one_based % 7 == 0 {
            base + 50
        } else {
            base
        }
    }

    const TOTAL_FRAMES: u64 = 30;
    const KEYFRAME_INTERVAL: u64 = 5;

    /// Build a source replay: frame-0 keyframe, then TOTAL_FRAMES frames with varying deltas,
    /// periodic keyframes, input/speed changes, a bookmark, and counters.
    fn build_source() -> Vec<u8> {
        let mut recorder = ReplayFileRecorder::new_with_metadata(
            make_metadata(),
            ByteVec::new(),
            small_settings(),
            0u64.into(),
            ib(&[0]),
            Speed::default(),
            state_for(0),
            Vec::<u8>::new(),
            Vec::<u8>::new(),
        )
        .unwrap();

        let mut running: u64 = 0;
        for frame in 1..=TOTAL_FRAMES {
            // Input change on frame 3.
            if frame == 3 {
                recorder.set_input(ib(&[0xAB, 0xCD])).unwrap();
            }
            // Speed change on frame 4.
            if frame == 4 {
                recorder
                    .set_speed(Speed::from_multiplier_float(2.0))
                    .unwrap();
            }
            // Counter on frame 6.
            if frame == 6 {
                recorder.change_counter("deaths".to_string(), 1).unwrap();
            }
            // Counter again on frame 10.
            if frame == 10 {
                recorder.change_counter("deaths".to_string(), 2).unwrap();
            }
            // Bookmark on frame 8.
            if frame == 8 {
                recorder.add_bookmark("checkpoint").unwrap();
            }

            running += delta_for(frame);
            recorder.next_frame(running.into()).unwrap();

            if frame % KEYFRAME_INTERVAL == 0 {
                recorder
                    .insert_keyframe(state_for(frame), running.into())
                    .unwrap();
            }
        }

        let (final_sink, _temp_sink) = recorder.close().unwrap();
        final_sink
    }

    /// Replay the source's NextFrame deltas in order (flat stream).
    fn source_deltas(bytes: &[u8]) -> Vec<u64> {
        let mut player = ReplayFilePlayer::new(bytes, false).unwrap();
        player.go_to_keyframe(0).unwrap();
        let mut deltas = Vec::new();
        while let Some(packet) = player.next_packet().unwrap() {
            if let Packet::NextFrame { timestamp_delta } = packet {
                deltas.push(timestamp_delta.0);
            }
        }
        deltas
    }

    fn source_time_at(bytes: &[u8], n: u64) -> u64 {
        source_deltas(bytes).iter().take(n as usize).sum()
    }

    fn resume_to_bytes(source: &[u8], n: Option<u64>) -> (Vec<u8>, ResumeInfo) {
        let mut feed = ReplayFilePlayer::new(source, false).unwrap();
        let (mut recorder, info) = build_resumed_recorder(
            &mut feed,
            n,
            small_settings(),
            ResumeCropPolicy::PreserveStartDropEnd,
            Vec::<u8>::new(),
            Vec::<u8>::new(),
        )
        .unwrap();
        let (final_sink, _temp) = recorder.close().unwrap();
        (final_sink, info)
    }

    #[test]
    fn round_trip_prefix_equivalence() {
        let source = build_source();
        let total = TOTAL_FRAMES;
        let src_deltas = source_deltas(&source);
        assert_eq!(src_deltas.len() as u64, total);

        // 0, a non-keyframe mid value (13), a keyframe value (15), total.
        for &n in &[0u64, 13, 15, total] {
            let (bytes, info) = resume_to_bytes(&source, Some(n));

            let player = ReplayFilePlayer::new(&bytes, false).unwrap();
            assert_eq!(player.get_total_frames(), n, "total frames for N={n}");
            assert_eq!(
                player.get_total_milliseconds().0,
                source_time_at(&source, n),
                "total ms for N={n}"
            );

            assert_eq!(info.elapsed_frames, n, "info frames for N={n}");
            assert_eq!(
                info.elapsed_millis.0,
                source_time_at(&source, n),
                "info ms for N={n}"
            );

            // Delta sequence for [0..n] matches the source.
            let resumed_deltas = source_deltas(&bytes);
            assert_eq!(
                resumed_deltas,
                src_deltas[..n as usize].to_vec(),
                "delta sequence for N={n}"
            );
        }
    }

    #[test]
    fn keyframe_states_match() {
        let source = build_source();
        let n = 20u64;
        let (bytes, _info) = resume_to_bytes(&source, Some(n));

        let src_player = ReplayFilePlayer::new(&source, false).unwrap();
        let res_player = ReplayFilePlayer::new(&bytes, false).unwrap();

        // For each keyframe <= n in the resumed file, the state must equal the source's.
        for (&frame, res_kfs) in res_player.all_keyframes() {
            if frame > n {
                continue;
            }
            let src_kfs = src_player
                .all_keyframes()
                .get(&frame)
                .expect("source missing keyframe present in resumed file");
            // Decode states by seeking the player to the keyframe and reading it.
            let res_state = read_keyframe_state(&bytes, frame);
            let src_state = read_keyframe_state(&source, frame);
            assert_eq!(res_state, src_state, "keyframe state mismatch at frame {frame}");
            // Sanity: metadata input matches too.
            assert_eq!(res_kfs[0].input, src_kfs[0].input);
        }
    }

    fn read_keyframe_state(bytes: &[u8], frame: u64) -> ByteVec {
        let mut player = ReplayFilePlayer::new(bytes, false).unwrap();
        player.go_to_keyframe(frame).unwrap();
        match player.next_packet().unwrap() {
            Some(Packet::Keyframe { state, .. }) => state.clone(),
            other => panic!("expected keyframe at {frame}, got {other:?}"),
        }
    }

    #[test]
    fn bookmarks_and_counters_within_prefix() {
        let source = build_source();

        // Counter increments are emitted just before frames 6 and 10; the bookmark just before
        // frame 8. The stop condition breaks on the NextFrame that would exceed `target` (without
        // emitting it), so packets emitted before that frame's NextFrame ARE retained. Using
        // resume points cleanly away from those frames avoids boundary ambiguity.
        //
        // Resume at 4: stop on NextFrame(5); nothing from frames 6/8/10 is reached.
        let (bytes_4, info_4) = resume_to_bytes(&source, Some(4));
        let player_4 = ReplayFilePlayer::new(&bytes_4, false).unwrap();
        assert!(player_4.all_bookmarks().get("checkpoint").is_none());
        let deaths_4 = info_4
            .counters
            .iter()
            .find(|c| c.name == "deaths")
            .map(|c| c.value)
            .unwrap_or(0);
        assert_eq!(deaths_4, 0);

        // Resume at 12: bookmark present, both counters (1 + 2 = 3).
        let (bytes_12, info_12) = resume_to_bytes(&source, Some(12));
        let player_12 = ReplayFilePlayer::new(&bytes_12, false).unwrap();
        assert!(player_12.all_bookmarks().get("checkpoint").is_some());
        let deaths_12 = info_12
            .counters
            .iter()
            .find(|c| c.name == "deaths")
            .map(|c| c.value)
            .unwrap_or(0);
        assert_eq!(deaths_12, 3);
    }

    #[test]
    fn timing_continuity_after_seam() {
        let source = build_source();
        let n = 14u64;
        let time_at_n = source_time_at(&source, n);

        let mut feed = ReplayFilePlayer::new(&source, false).unwrap();
        let (mut recorder, info) = build_resumed_recorder(
            &mut feed,
            Some(n),
            small_settings(),
            ResumeCropPolicy::PreserveStartDropEnd,
            Vec::<u8>::new(),
            Vec::<u8>::new(),
        )
        .unwrap();

        assert_eq!(info.elapsed_millis.0, time_at_n);

        // Append a few more frames.
        let appended = [20u64, 30, 25];
        let mut running = info.elapsed_millis.0;
        for &d in &appended {
            running += d;
            recorder.next_frame(running.into()).unwrap();
        }
        recorder
            .insert_keyframe(state_for(99), running.into())
            .unwrap();

        let (bytes, _temp) = recorder.close().unwrap();
        let player = ReplayFilePlayer::new(&bytes, false).unwrap();

        assert_eq!(player.get_total_frames(), n + appended.len() as u64);
        assert_eq!(
            player.get_total_milliseconds().0,
            time_at_n + appended.iter().sum::<u64>()
        );

        // Appended deltas are exactly what we fed.
        let deltas = source_deltas(&bytes);
        assert_eq!(deltas.len() as u64, n + appended.len() as u64);
        assert_eq!(deltas[n as usize..].to_vec(), appended.to_vec());
    }

    #[test]
    fn end_mode_reproduces_all_frames() {
        let source = build_source();
        let (bytes, info) = resume_to_bytes(&source, None);

        let player = ReplayFilePlayer::new(&bytes, false).unwrap();
        assert_eq!(player.get_total_frames(), TOTAL_FRAMES);
        assert_eq!(
            player.get_total_milliseconds().0,
            source_time_at(&source, TOTAL_FRAMES)
        );
        assert_eq!(info.elapsed_frames, TOTAL_FRAMES);
        assert_eq!(info.elapsed_millis.0, source_time_at(&source, TOTAL_FRAMES));

        assert_eq!(source_deltas(&bytes), source_deltas(&source));
    }

    #[test]
    fn short_corrupt_source_clamps() {
        let source = build_source();
        let full_total = TOTAL_FRAMES;

        // Truncate mid-stream, always keeping the full header so the file still parses. Cutting 1/3
        // of the post-header data drops the tail blob(s) while leaving earlier ones intact.
        let header_len = core::mem::size_of::<crate::replay_file::ReplayHeaderBytes>();
        let truncated_len = header_len + (source.len() - header_len) * 2 / 3;
        let truncated = source[..truncated_len].to_vec();

        let mut feed = ReplayFilePlayer::new(&truncated, true).unwrap();
        let intact_total = feed.get_total_frames();
        assert!(intact_total < full_total, "truncation should reduce frames");

        // Request a mid N beyond the intact range; should clamp without panicking.
        let (mut recorder, info) = build_resumed_recorder(
            &mut feed,
            Some(full_total),
            small_settings(),
            ResumeCropPolicy::PreserveStartDropEnd,
            Vec::<u8>::new(),
            Vec::<u8>::new(),
        )
        .unwrap();
        let (bytes, _temp) = recorder.close().unwrap();

        // cur_frames clamped to <= intact_total.
        assert!(info.elapsed_frames <= intact_total);

        let player = ReplayFilePlayer::new(&bytes, false).unwrap();
        assert_eq!(player.get_total_frames(), info.elapsed_frames);
    }

    #[test]
    fn crop_policy_all_variants() {
        // crop_start at frame 5 (ms 100, offset 50), crop_end at frame 20 (ms 400).
        let with_crops = |start: Option<u64>, end: Option<u64>| {
            let mut m = make_metadata();
            m.crop_start = start.map(|f| (f, ((f * 20) as u64).into()));
            m.timer_offset = start.map(|_| 50u64.into());
            m.crop_end = end.map(|f| (f, ((f * 20) as u64).into()));
            m
        };

        // PreserveStartDropEnd, resume past start: keep start, drop end.
        let m = with_crops(Some(5), Some(20))
            .with_resume_crop(10, ResumeCropPolicy::PreserveStartDropEnd);
        assert!(m.crop_start.is_some());
        assert!(m.timer_offset.is_some());
        assert!(m.crop_end.is_none());

        // PreserveStartDropEnd, resume before start: drop start too.
        let m = with_crops(Some(15), Some(20))
            .with_resume_crop(10, ResumeCropPolicy::PreserveStartDropEnd);
        assert!(m.crop_start.is_none());
        assert!(m.timer_offset.is_none());
        assert!(m.crop_end.is_none());

        // PreserveStartDropEnd with no start: nothing kept.
        let m = with_crops(None, Some(20))
            .with_resume_crop(10, ResumeCropPolicy::PreserveStartDropEnd);
        assert!(m.crop_start.is_none());
        assert!(m.crop_end.is_none());

        // DropAll: everything gone.
        let m = with_crops(Some(5), Some(20)).with_resume_crop(10, ResumeCropPolicy::DropAll);
        assert!(m.crop_start.is_none());
        assert!(m.crop_end.is_none());
        assert!(m.timer_offset.is_none());

        // PreserveAll: unchanged.
        let original = with_crops(Some(5), Some(20));
        let m = original.clone().with_resume_crop(10, ResumeCropPolicy::PreserveAll);
        assert_eq!(m.crop_start, original.crop_start);
        assert_eq!(m.crop_end, original.crop_end);
        assert_eq!(m.timer_offset, original.timer_offset);
    }
}
