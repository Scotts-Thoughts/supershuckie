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
/// Writes header + patch + the prefix `[0 ..= target]` into the two sinks by re-feeding the
/// source player's flat packet stream. Returns the OPEN recorder (do not close it — the caller
/// continues recording) and a [`ResumeInfo`] describing the resume point.
///
/// `source` should be a FRESH player instance dedicated to this call (its cursor is consumed).
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
    let end_mode = target == total;

    // Step 2: position at frame 0.
    source.go_to_keyframe(0).map_err(ReplayResumeError::Read)?;

    // Step 3: read the first packet (must be a frame-0 keyframe). Clone everything out before any
    // further next_packet() calls (which borrow the player mutably).
    let (state0, input0, speed0, millis0, counters0) = {
        let first = source
            .next_packet()
            .map_err(|error| ReplayResumeError::Read(ReplaySeekError::ReadError { error }))?;

        match first {
            Some(Packet::Keyframe { metadata, state }) if metadata.elapsed_frames == 0 => (
                state.clone(),
                metadata.input.clone(),
                metadata.speed,
                metadata.elapsed_millis,
                metadata.counters.clone(),
            ),
            _ => {
                return Err(ReplayResumeError::BadSource {
                    explanation: Cow::Borrowed("source did not begin with a frame-0 keyframe"),
                })
            }
        }
    };

    // Step 4: apply the crop policy to the source metadata.
    let metadata = source
        .get_replay_metadata()
        .clone()
        .with_resume_crop(target, crop_policy);

    // Step 5: build the recorder (emits header + patch + frame-0 keyframe).
    let mut patch = ByteVec::new();
    if let Some(patch_bytes) = source.get_patch_data() {
        patch.extend_from_slice(patch_bytes);
    }

    let mut recorder = ReplayFileRecorder::new_with_metadata(
        metadata,
        patch,
        settings,
        millis0,
        input0.clone(),
        speed0,
        state0,
        final_sink,
        temp_sink,
    )
    .map_err(ReplayResumeError::Write)?;

    // Step 6: seed counters from the frame-0 keyframe (rare), maintaining a local authoritative map.
    let mut counter_map: BTreeMap<String, i64> = BTreeMap::new();
    for counter in &counters0 {
        counter_map.insert(counter.name.clone(), counter.value);
        recorder
            .change_counter(counter.name.clone(), counter.value)
            .map_err(ReplayResumeError::Write)?;
    }

    // Step 7: re-feed loop.
    let mut running_ms: u64 = millis0.0;
    let mut cur_frames: u64 = 0;
    let mut cur_input: InputBuffer = input0;
    let mut cur_speed: Speed = speed0;

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

    // Step 8: build ResumeInfo.
    let counters: Vec<Counter> = counter_map
        .into_iter()
        .map(|(name, value)| Counter { name, value })
        .collect();

    let resume_info = ResumeInfo {
        elapsed_frames: cur_frames,
        elapsed_millis: running_ms.into(),
        speed: cur_speed,
        input: cur_input,
        counters,
    };

    // Step 9: do NOT close the recorder.
    Ok((recorder, resume_info))
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

        // Truncate mid-stream. Keep header + patch + at least the first blob.
        let truncated_len = source.len() * 2 / 3;
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
