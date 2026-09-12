//! Offline re-encoding of replay files into the current format.
//!
//! This is the engine behind the `supershuckie-replay-convert` command line tool and the app's
//! "Convert replay" actions: [`convert_replay_file`] re-feeds every packet of a v2/v3/v4 replay
//! through the recorder (see [`build_reencoded_recorder`]) and [`verify_replay_files`] re-opens the
//! source and the result and compares them packet by packet.

use std::fmt::{Display, Formatter};
use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::time::{Duration, Instant};

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::keyframe_masks::transient_ranges;
use crate::replay_file::playback::ReplayFilePlayer;
use crate::replay_file::record::{
    build_reencoded_recorder, NullReplayFileSink, ReplayFileRecorderSettings, ReplayResumeError,
};
use crate::replay_file::{ReplayConsoleType, ReplayHeaderBytes, ReplayHeaderRaw};
use crate::{Packet, UnsignedInteger};

/// How to re-encode a replay.
#[derive(Clone, Debug)]
pub struct ConvertOptions {
    /// Recorder settings (compression level, chain length, masks) for the output.
    pub settings: ReplayFileRecorderSettings,

    /// Read as much of a damaged source as possible instead of failing.
    pub allow_corruption: bool,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        Self { settings: ReplayFileRecorderSettings::default(), allow_corruption: false }
    }
}

/// What the converter is doing, for progress reporting.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ConvertPhase {
    /// Re-feeding the source through the recorder.
    Converting,
    /// Comparing the source and the output.
    Verifying,
}

/// Progress callback: `(phase, frames_done, frames_total)`; return `false` to cancel.
pub type ProgressFn<'a> = &'a mut dyn FnMut(ConvertPhase, UnsignedInteger, UnsignedInteger) -> bool;

/// Why a conversion or verification did not complete.
#[derive(Clone, Debug, PartialEq)]
pub enum ConvertError {
    /// The progress callback asked to stop. Partial output has been deleted.
    Cancelled,
    /// Anything else, described for the user.
    Failed(String),
}

impl Display for ConvertError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ConvertError::Cancelled => f.write_str("cancelled"),
            ConvertError::Failed(message) => f.write_str(message),
        }
    }
}

impl From<String> for ConvertError {
    fn from(message: String) -> Self {
        ConvertError::Failed(message)
    }
}

/// Summary of a replay file as parsed by the player.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplaySourceInfo {
    /// Format version from the header.
    pub version: u32,
    /// Console the replay is for.
    pub console: ReplayConsoleType,
    /// ROM name from the header.
    pub rom_name: String,
    /// File size in bytes.
    pub size: u64,
    /// Total emulated frames.
    pub frames: UnsignedInteger,
    /// Total elapsed milliseconds.
    pub millis: UnsignedInteger,
    /// Number of keyframes.
    pub keyframes: usize,
    /// Number of compressed blobs.
    pub blobs: usize,
    /// Number of uncompressed top-level packets (non-zero for a crash-safe temp file).
    pub top_level_packets: usize,
}

/// Result of a successful [`convert_replay_file`].
#[derive(Clone, Debug, PartialEq)]
pub struct ConvertReport {
    /// The source as parsed.
    pub source: ReplaySourceInfo,
    /// Size of the output file in bytes.
    pub output_size: u64,
    /// Frames re-fed (equals `source.frames` unless the source was truncated and
    /// `allow_corruption` was set).
    pub frames: UnsignedInteger,
    /// Wall time.
    pub elapsed: Duration,
}

/// Result of a successful [`verify_replay_files`].
#[derive(Clone, Debug, PartialEq)]
pub struct VerifyReport {
    /// Packets compared.
    pub packets: u64,
    /// Keyframes compared.
    pub keyframes: u64,
    /// Frames compared.
    pub frames: u64,
    /// Whether keyframe states were compared only outside the transient (masked) ranges.
    pub masked: bool,
    /// Wall time.
    pub elapsed: Duration,
}

/// Read just the header of a replay file and return its format version.
///
/// Fails if the file is too short, has the wrong signature, or a version this build cannot read.
pub fn replay_file_version(path: &Path) -> Result<u32, String> {
    use std::io::Read;

    let mut file = File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let mut header = [0u8; size_of::<ReplayHeaderBytes>()];
    file.read_exact(&mut header).map_err(|e| format!("cannot read the header of {}: {e}", path.display()))?;
    let raw = ReplayHeaderRaw::from_bytes(&header);
    raw.parse().map_err(|e| format!("{}: {e}", path.display()))?;
    let version = raw.replay_version;
    Ok(version)
}

fn map_file(path: &Path) -> Result<memmap2::Mmap, String> {
    let file = File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    // SAFETY: the file is only read, and nothing else is expected to modify a replay while it is
    // being converted (an in-progress recording must never be converted).
    unsafe { memmap2::Mmap::map(&file) }.map_err(|e| format!("cannot map {}: {e}", path.display()))
}

fn open_player(path: &Path, bytes: &[u8], allow_corruption: bool) -> Result<ReplayFilePlayer, String> {
    ReplayFilePlayer::new(bytes, allow_corruption).map_err(|e| format!("cannot parse {}: {e:?}", path.display()))
}

fn source_info(player: &ReplayFilePlayer, size: u64) -> ReplaySourceInfo {
    let blobs = player.all_uncompressed_packets().iter().filter(|p| matches!(p, Packet::CompressedBlob { .. })).count();
    let metadata = player.get_replay_metadata();
    ReplaySourceInfo {
        version: player.get_replay_version(),
        console: metadata.console_type,
        rom_name: metadata.rom_name.clone(),
        size,
        frames: player.get_total_frames(),
        millis: player.get_total_milliseconds().0,
        keyframes: player.all_keyframes().values().map(|k| k.len()).sum(),
        blobs,
        top_level_packets: player.all_uncompressed_packets().len() - blobs,
    }
}

/// Re-encode `input` into `output` (which must not exist yet unless `overwrite` is set).
///
/// The header, patch, crop/timer markers, bookmarks, counters and every emulated frame are
/// carried over unchanged; keyframes are re-encoded with `options.settings`. On any error, or when
/// the progress callback cancels, a partially written `output` is deleted.
pub fn convert_replay_file(
    input: &Path,
    output: &Path,
    options: &ConvertOptions,
    overwrite: bool,
    progress: ProgressFn<'_>,
) -> Result<ConvertReport, ConvertError> {
    if input == output {
        return Err(ConvertError::Failed("input and output must be different files".into()));
    }
    if output.exists() && !overwrite {
        return Err(ConvertError::Failed(format!("{} already exists", output.display())));
    }

    let started = Instant::now();

    // Open and parse the source before touching the output, so a bad source never costs an
    // existing output file.
    let input_map = map_file(input)?;
    let input_size = input_map.len() as u64;
    let mut player = open_player(input, &input_map[..], options.allow_corruption)?;
    let source = source_info(&player, input_size);

    let output_file = File::create(output).map_err(|e| format!("cannot create {}: {e}", output.display()))?;
    let result = write_reencoded(&mut player, output_file, options, progress);
    if result.is_err() {
        let _ = std::fs::remove_file(output);
    }
    let (output_size, frames) = result?;

    Ok(ConvertReport { source, output_size, frames, elapsed: started.elapsed() })
}

/// Re-feed `player` into `output_file`; returns the output size and the frames re-fed.
fn write_reencoded(
    player: &mut ReplayFilePlayer,
    output_file: File,
    options: &ConvertOptions,
    progress: ProgressFn<'_>,
) -> Result<(u64, UnsignedInteger), ConvertError> {
    let sink = BufWriter::with_capacity(8 * 1024 * 1024, output_file);

    let (mut recorder, info) = build_reencoded_recorder(
        player,
        options.settings.clone(),
        options.allow_corruption,
        sink,
        NullReplayFileSink,
        &mut |frames, total| progress(ConvertPhase::Converting, frames, total),
    )
    .map_err(|e| match e {
        ReplayResumeError::Cancelled => ConvertError::Cancelled,
        other => ConvertError::Failed(format!("conversion failed: {other:?}")),
    })?;

    let (sink, _) = recorder.close().map_err(|(_, _, e)| format!("closing the output failed: {e}"))?;
    let output_file = sink.into_inner().map_err(|e| format!("flushing the output failed: {e}"))?;
    output_file.sync_all().map_err(|e| format!("syncing the output failed: {e}"))?;
    let output_size = output_file.metadata().map_err(|e| format!("cannot stat the output: {e}"))?.len();

    Ok((output_size, info.elapsed_frames))
}

fn describe_packet(packet: &Packet) -> String {
    match packet {
        Packet::Keyframe { metadata, state } => format!("Keyframe(frame {}, {} bytes)", metadata.elapsed_frames, state.len()),
        Packet::LoadSaveState { state } => format!("LoadSaveState({} bytes)", state.len()),
        Packet::DeltaKeyframe { metadata, .. } => format!("DeltaKeyframe(frame {})", metadata.elapsed_frames),
        Packet::RegionDeltaKeyframe { metadata, .. } => format!("RegionDeltaKeyframe(frame {})", metadata.elapsed_frames),
        Packet::CompressedBlob { .. } => "CompressedBlob".into(),
        other => format!("{other:?}"),
    }
}

fn check_eq<T: PartialEq + core::fmt::Debug>(what: &str, source: T, output: T) -> Result<(), String> {
    if source == output {
        Ok(())
    }
    else {
        Err(format!("{what} differs:\n  source: {source:?}\n  output: {output:?}"))
    }
}

/// Compare two keyframe states, reporting the first differing byte. With `masked`, bytes inside
/// the transient ranges of the source state are ignored (the output legitimately holds the chain
/// restart's copy there).
fn check_states(console: ReplayConsoleType, masked: bool, frame: UnsignedInteger, source: &[u8], output: &[u8]) -> Result<(), String> {
    if source.len() != output.len() {
        return Err(format!("keyframe state length differs at frame {frame}: {} vs {}", source.len(), output.len()));
    }

    let ranges = if masked { transient_ranges(console, source) } else { Vec::new() };
    let mut offset = 0usize;
    for range in ranges.iter().chain(core::iter::once(&(source.len()..source.len()))) {
        let live = offset..range.start.clamp(offset, source.len());
        if let Some(at) = source[live.clone()].iter().zip(&output[live.clone()]).position(|(a, b)| a != b) {
            return Err(format!("keyframe state differs at frame {frame}, byte 0x{:X}", live.start + at));
        }
        offset = range.end.clamp(offset, source.len());
    }
    Ok(())
}

/// Re-open `input` and `output` and check that they describe the same replay: same totals,
/// header metadata, patch, keyframe and bookmark indexes, and the same packet stream, with every
/// keyframe state reconstructing bit-exactly — outside the transient ranges if `masked` (the
/// output was written with `mask_transient_buffers`).
pub fn verify_replay_files(
    input: &Path,
    output: &Path,
    masked: bool,
    allow_corruption: bool,
    progress: ProgressFn<'_>,
) -> Result<VerifyReport, ConvertError> {
    let started = Instant::now();
    let input_map = map_file(input)?;
    let output_map = map_file(output)?;
    let mut source = open_player(input, &input_map[..], allow_corruption)?;
    let mut result = open_player(output, &output_map[..], false)?;

    check_eq("total frames", source.get_total_frames(), result.get_total_frames())?;
    check_eq("total milliseconds", source.get_total_milliseconds(), result.get_total_milliseconds())?;
    check_eq("header metadata", source.get_replay_metadata(), result.get_replay_metadata())?;
    check_eq("patch data", source.get_patch_data(), result.get_patch_data())?;
    check_eq("keyframe frames", source.all_keyframes().keys().collect::<Vec<_>>(), result.all_keyframes().keys().collect::<Vec<_>>())?;
    let bookmark_index = |player: &ReplayFilePlayer| {
        player
            .all_bookmarks()
            .iter()
            .map(|(name, list)| (name.clone(), list.iter().map(|b| (b.elapsed_frames, b.elapsed_millis)).collect::<Vec<_>>()))
            .collect::<Vec<_>>()
    };
    check_eq("bookmarks", bookmark_index(&source), bookmark_index(&result))?;
    let console = source.get_replay_metadata().console_type;

    source.go_to_keyframe(0).map_err(|e| format!("source: cannot seek to frame 0: {e:?}"))?;
    result.go_to_keyframe(0).map_err(|e| format!("output: cannot seek to frame 0: {e:?}"))?;

    let total = source.get_total_frames();
    let mut packets = 0u64;
    let mut keyframes = 0u64;
    let mut frames = 0u64;

    loop {
        let a = source.next_packet().map_err(|e| format!("source: read error after packet {packets} (frame {frames}): {e:?}"))?;
        let b = result.next_packet().map_err(|e| format!("output: read error after packet {packets} (frame {frames}): {e:?}"))?;

        let (a, b) = match (a, b) {
            (None, None) => break,
            (Some(a), None) => return Err(format!("output ends after packet {packets} (frame {frames}); source continues with {}", describe_packet(a)).into()),
            (None, Some(b)) => return Err(format!("output continues after the source ended at packet {packets} (frame {frames}) with {}", describe_packet(b)).into()),
            (Some(a), Some(b)) => (a, b),
        };

        match (a, b) {
            (Packet::Keyframe { metadata: ma, state: sa }, Packet::Keyframe { metadata: mb, state: sb }) => {
                check_eq(&format!("keyframe metadata at frame {}", ma.elapsed_frames), ma, mb)?;
                check_states(console, masked, ma.elapsed_frames, sa.as_slice(), sb.as_slice())?;
                keyframes += 1;
                if !progress(ConvertPhase::Verifying, frames, total) {
                    return Err(ConvertError::Cancelled);
                }
            }
            _ => {
                if a != b {
                    return Err(format!("packet {packets} (frame {frames}) differs: source {}, output {}", describe_packet(a), describe_packet(b)).into());
                }
            }
        }

        if matches!(a, Packet::NextFrame { .. }) {
            frames += 1;
        }
        packets += 1;
    }

    Ok(VerifyReport { packets, keyframes, frames, masked, elapsed: started.elapsed() })
}

/// `bytes` as a short human-readable size.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

/// `millis` as `h:mm:ss`.
pub fn human_duration(millis: u64) -> String {
    let seconds = millis / 1000;
    format!("{}:{:02}:{:02}", seconds / 3600, (seconds / 60) % 60, seconds % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("supershuckie-convert-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn convert_and_verify_the_v3_fixture() {
        let dir = temp_dir("fixture");
        let input = dir.join("v3.replay");
        let output = dir.join("v4.replay");
        std::fs::write(&input, V3_SMALL_CLOSED).unwrap();

        assert_eq!(replay_file_version(&input).unwrap(), 3);

        let options = ConvertOptions { settings: ReplayFileRecorderSettings { max_frames_per_blob: 70, ..Default::default() }, allow_corruption: false };
        let mut phases = Vec::new();
        let report = convert_replay_file(&input, &output, &options, false, &mut |phase, done, total| {
            phases.push((phase, done, total));
            true
        })
        .unwrap();

        assert_eq!(report.source.version, 3);
        assert_eq!(report.source.frames, TOTAL_FRAMES);
        assert_eq!(report.source.keyframes, keyframe_frames().len());
        assert_eq!(report.frames, TOTAL_FRAMES);
        assert_eq!(report.output_size, std::fs::metadata(&output).unwrap().len());
        assert!(report.output_size < report.source.size);
        assert!(phases.iter().all(|p| p.0 == ConvertPhase::Converting && p.1 <= p.2));
        assert_eq!(replay_file_version(&output).unwrap(), crate::replay_file::REPLAY_VERSION);

        // Refuses to overwrite unless asked.
        assert!(matches!(convert_replay_file(&input, &output, &options, false, &mut |_, _, _| true), Err(ConvertError::Failed(_))));
        assert!(output.exists());

        let verify = verify_replay_files(&input, &output, true, false, &mut |phase, _, _| phase == ConvertPhase::Verifying).unwrap();
        assert_eq!(verify.frames, TOTAL_FRAMES);
        assert_eq!(verify.keyframes as usize, keyframe_frames().len());
        assert!(verify.packets > 200);

        // A tampered output fails verification.
        let mut bytes = std::fs::read(&output).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        let tampered = dir.join("tampered.replay");
        std::fs::write(&tampered, &bytes).unwrap();
        assert!(matches!(verify_replay_files(&input, &tampered, true, false, &mut |_, _, _| true), Err(ConvertError::Failed(_))));

        // Cancelling removes the partial output.
        let cancelled = dir.join("cancelled.replay");
        let result = convert_replay_file(&input, &cancelled, &options, false, &mut |_, done, _| done < 50);
        assert_eq!(result, Err(ConvertError::Cancelled));
        assert!(!cancelled.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn version_of_a_non_replay_is_an_error() {
        let dir = temp_dir("version");
        let junk = dir.join("junk.replay");
        std::fs::write(&junk, b"not a replay").unwrap();
        assert!(replay_file_version(&junk).is_err());
        assert!(replay_file_version(&dir.join("missing.replay")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
