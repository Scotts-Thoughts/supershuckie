//! `supershuckie-replay-convert`: re-encode a Super Shuckie replay file into the current format.
//!
//! Every keyframe of the source (v2, v3 or v4) is re-fed through the recorder, so the output is a
//! format-v4 file with region-diffed keyframes, long delta chains and zstd level 9 — typically 3-6x
//! smaller than a v3 file — while the header, patch, crop/timer markers, bookmarks, counters and
//! every emulated frame are carried over unchanged. `--verify` re-opens both files afterwards and
//! checks them packet by packet (keyframe states must reconstruct bit-exactly).
//!
//! ```text
//! supershuckie-replay-convert <in.replay> <out.replay>
//!     [--level 9] [--chain-frames 54000] [--blob-mb 1024] [--no-masks] [--verify]
//!     [--allow-corruption] [--force]
//! ```
//!
//! By default delta keyframes leave out regenerated output buffers (see
//! `supershuckie_replay_recorder::keyframe_masks`); `--no-masks` keeps every keyframe bit-exact.
//! With masks on, `--verify` compares keyframe states outside the masked ranges.
//!
//! Build with `cargo build --release -p supershuckie-replay-recorder --features convert`.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use memmap2::Mmap;
use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::replay_file::record::{
    build_reencoded_recorder, NullReplayFileSink, ReplayFileRecorderSettings, DEFAULT_MAX_FRAMES_PER_BLOB,
    DEFAULT_MINIMUM_UNCOMPRESSED_BYTES_PER_BLOB, DEFAULT_ZSTD_COMPRESSION_LEVEL_V4,
};
use supershuckie_replay_recorder::keyframe_masks::transient_ranges;
use supershuckie_replay_recorder::replay_file::ReplayConsoleType;
use supershuckie_replay_recorder::{Packet, UnsignedInteger};

const USAGE: &str = "\
usage: supershuckie-replay-convert <in.replay> <out.replay> [options]

Re-encodes a replay (format v2/v3/v4) into the current format (v4).

options:
  --level <n>          zstd compression level (default 9)
  --chain-frames <n>   frames per delta chain / compressed blob, 0 = unlimited (default 54000 = 15 min)
  --blob-mb <n>        hard cap on buffered uncompressed bytes per blob in MiB (default 1024)
  --no-masks           keep regenerated output buffers (3D geometry banks, mixed PCM) in delta
                       keyframes, i.e. every keyframe reconstructs bit-exactly
  --verify             re-open both files afterwards and compare them packet by packet
  --allow-corruption   read as much of a damaged source as possible instead of failing
  --force              overwrite <out.replay> if it exists
  -h, --help           show this help
";

struct Args {
    input: PathBuf,
    output: PathBuf,
    level: i32,
    chain_frames: u64,
    blob_bytes: usize,
    masks: bool,
    verify: bool,
    allow_corruption: bool,
    force: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut positional = Vec::new();
    let mut level = DEFAULT_ZSTD_COMPRESSION_LEVEL_V4;
    let mut chain_frames = DEFAULT_MAX_FRAMES_PER_BLOB;
    let mut blob_bytes = DEFAULT_MINIMUM_UNCOMPRESSED_BYTES_PER_BLOB;
    let mut masks = true;
    let mut verify = false;
    let mut allow_corruption = false;
    let mut force = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| -> Result<String, String> {
            args.next().ok_or_else(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "-h" | "--help" => return Err(USAGE.to_owned()),
            "--level" => level = value("--level")?.parse().map_err(|e| format!("--level: {e}"))?,
            "--chain-frames" => chain_frames = value("--chain-frames")?.parse().map_err(|e| format!("--chain-frames: {e}"))?,
            "--blob-mb" => {
                let mb: usize = value("--blob-mb")?.parse().map_err(|e| format!("--blob-mb: {e}"))?;
                blob_bytes = mb.saturating_mul(1024 * 1024);
            }
            "--no-masks" => masks = false,
            "--verify" => verify = true,
            "--allow-corruption" => allow_corruption = true,
            "--force" => force = true,
            other if other.starts_with('-') => return Err(format!("unknown option {other}\n\n{USAGE}")),
            _ => positional.push(PathBuf::from(arg)),
        }
    }

    let [input, output] = <[PathBuf; 2]>::try_from(positional).map_err(|_| USAGE.to_owned())?;

    Ok(Args { input, output, level, chain_frames, blob_bytes, masks, verify, allow_corruption, force })
}

fn map_file(path: &Path) -> Result<Mmap, String> {
    let file = File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    // SAFETY: the file is only read, and nothing else is expected to modify a replay while it is
    // being converted (an in-progress recording should never be converted).
    unsafe { Mmap::map(&file) }.map_err(|e| format!("cannot map {}: {e}", path.display()))
}

fn open_player(path: &Path, bytes: &[u8], allow_corruption: bool) -> Result<ReplayFilePlayer, String> {
    ReplayFilePlayer::new(bytes, allow_corruption).map_err(|e| format!("cannot parse {}: {e:?}", path.display()))
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

fn human_duration(millis: u64) -> String {
    let seconds = millis / 1000;
    format!("{}:{:02}:{:02}", seconds / 3600, (seconds / 60) % 60, seconds % 60)
}

fn describe_source(path: &Path, size: u64, player: &ReplayFilePlayer) {
    let blobs = player.all_uncompressed_packets().iter().filter(|p| matches!(p, Packet::CompressedBlob { .. })).count();
    let top_level = player.all_uncompressed_packets().len() - blobs;
    let metadata = player.get_replay_metadata();
    eprintln!(
        "{}: format v{}, {}, {} ({}), {} frames ({}), {} keyframes, {} blobs{}",
        path.display(),
        player.get_replay_version(),
        metadata.console_type,
        metadata.rom_name,
        human_size(size),
        player.get_total_frames(),
        human_duration(player.get_total_milliseconds().0),
        player.all_keyframes().values().map(|k| k.len()).sum::<usize>(),
        blobs,
        if top_level > 0 { format!(" + {top_level} uncompressed top-level packets") } else { String::new() }
    );
}

/// Prints `<what>... N%` to stderr, at most twice a second and only when the percentage changed.
struct ProgressReporter {
    what: &'static str,
    started: Instant,
    last_print: Option<Instant>,
    last_percent: u64,
}

impl ProgressReporter {
    fn new(what: &'static str, started: Instant) -> Self {
        Self { what, started, last_print: None, last_percent: u64::MAX }
    }

    fn report(&mut self, done: u64, total: u64) {
        let percent = done * 100 / total.max(1);
        let due = self.last_print.is_none_or(|t| t.elapsed().as_millis() >= 500);
        if percent != self.last_percent && due {
            self.last_percent = percent;
            self.last_print = Some(Instant::now());
            eprint!("\r{}... {percent:3}% ({done} / {total} frames, {:.0} s)", self.what, self.started.elapsed().as_secs_f64());
        }
    }
}

fn convert(args: &Args) -> Result<(), String> {
    if args.input == args.output {
        return Err("input and output must be different files".to_owned());
    }
    if args.output.exists() && !args.force {
        return Err(format!("{} already exists (use --force to overwrite)", args.output.display()));
    }

    let started = Instant::now();
    let input_map = map_file(&args.input)?;
    let input_size = input_map.len() as u64;
    let mut player = open_player(&args.input, &input_map[..], args.allow_corruption)?;
    describe_source(&args.input, input_size, &player);

    let output_file = File::create(&args.output).map_err(|e| format!("cannot create {}: {e}", args.output.display()))?;
    let output = BufWriter::with_capacity(8 * 1024 * 1024, output_file);

    let settings = ReplayFileRecorderSettings {
        minimum_uncompressed_bytes_per_blob: args.blob_bytes,
        max_frames_per_blob: args.chain_frames,
        compression_level: args.level,
        mask_transient_buffers: args.masks,
    };

    let total = player.get_total_frames().max(1);
    let mut reporter = ProgressReporter::new("converting", started);
    let mut progress = |frames: UnsignedInteger, _target: UnsignedInteger| reporter.report(frames, total);

    let (mut recorder, info) = build_reencoded_recorder(&mut player, settings, args.allow_corruption, output, NullReplayFileSink, &mut progress)
        .map_err(|e| format!("\nconversion failed: {e:?}"))?;
    eprintln!();

    let (output, _) = recorder.close().map_err(|(_, _, e)| format!("closing the output failed: {e}"))?;
    let output_file = output.into_inner().map_err(|e| format!("flushing the output failed: {e}"))?;
    output_file.sync_all().map_err(|e| format!("syncing the output failed: {e}"))?;
    let output_size = output_file.metadata().map_err(|e| format!("cannot stat the output: {e}"))?.len();
    drop(output_file);
    drop(player);
    drop(input_map);

    eprintln!(
        "wrote {}: {} ({:.2}x smaller, {} frames) in {:.1} s",
        args.output.display(),
        human_size(output_size),
        input_size as f64 / output_size.max(1) as f64,
        info.elapsed_frames,
        started.elapsed().as_secs_f64()
    );

    Ok(())
}

fn describe_packet(packet: &Packet) -> String {
    match packet {
        Packet::Keyframe { metadata, state } => format!("Keyframe(frame {}, {} bytes)", metadata.elapsed_frames, state.len()),
        Packet::LoadSaveState { state } => format!("LoadSaveState({} bytes)", state.len()),
        Packet::DeltaKeyframe { metadata, .. } => format!("DeltaKeyframe(frame {})", metadata.elapsed_frames),
        Packet::RegionDeltaKeyframe { metadata, .. } => format!("RegionDeltaKeyframe(frame {})", metadata.elapsed_frames),
        Packet::CompressedBlob { .. } => "CompressedBlob".to_owned(),
        other => format!("{other:?}"),
    }
}

fn check_eq<T: PartialEq + std::fmt::Debug>(what: &str, source: T, output: T) -> Result<(), String> {
    if source == output {
        Ok(())
    }
    else {
        Err(format!("{what} differs:\n  source: {source:?}\n  output: {output:?}"))
    }
}

/// Compare two keyframe states, reporting the first differing byte. With `masks`, bytes inside
/// the transient ranges of the source state are ignored (the output legitimately holds the chain
/// restart's copy there).
fn check_states(console: ReplayConsoleType, masks: bool, frame: UnsignedInteger, source: &[u8], output: &[u8]) -> Result<(), String> {
    if source.len() != output.len() {
        return Err(format!("keyframe state length differs at frame {frame}: {} vs {}", source.len(), output.len()));
    }

    let masked = if masks { transient_ranges(console, source) } else { Vec::new() };
    let mut offset = 0usize;
    for range in masked.iter().chain(core::iter::once(&(source.len()..source.len()))) {
        let live = offset..range.start.clamp(offset, source.len());
        if let Some(at) = source[live.clone()].iter().zip(&output[live.clone()]).position(|(a, b)| a != b) {
            return Err(format!("keyframe state differs at frame {frame}, byte 0x{:X}", live.start + at));
        }
        offset = range.end.clamp(offset, source.len());
    }
    Ok(())
}

fn verify(args: &Args) -> Result<(), String> {
    let started = Instant::now();
    let input_map = map_file(&args.input)?;
    let output_map = map_file(&args.output)?;
    let mut source = open_player(&args.input, &input_map[..], args.allow_corruption)?;
    let mut output = open_player(&args.output, &output_map[..], false)?;

    check_eq("total frames", source.get_total_frames(), output.get_total_frames())?;
    check_eq("total milliseconds", source.get_total_milliseconds(), output.get_total_milliseconds())?;
    check_eq("header metadata", source.get_replay_metadata(), output.get_replay_metadata())?;
    check_eq("patch data", source.get_patch_data(), output.get_patch_data())?;
    check_eq("keyframe frames", source.all_keyframes().keys().collect::<Vec<_>>(), output.all_keyframes().keys().collect::<Vec<_>>())?;
    let bookmark_index = |player: &ReplayFilePlayer| {
        player
            .all_bookmarks()
            .iter()
            .map(|(name, list)| (name.clone(), list.iter().map(|b| (b.elapsed_frames, b.elapsed_millis)).collect::<Vec<_>>()))
            .collect::<Vec<_>>()
    };
    check_eq("bookmarks", bookmark_index(&source), bookmark_index(&output))?;
    let console = source.get_replay_metadata().console_type;

    source.go_to_keyframe(0).map_err(|e| format!("source: cannot seek to frame 0: {e:?}"))?;
    output.go_to_keyframe(0).map_err(|e| format!("output: cannot seek to frame 0: {e:?}"))?;

    let total = source.get_total_frames().max(1);
    let mut packets = 0u64;
    let mut keyframes = 0u64;
    let mut frames = 0u64;
    let mut reporter = ProgressReporter::new("verifying", started);

    loop {
        let a = source.next_packet().map_err(|e| format!("source: read error after packet {packets} (frame {frames}): {e:?}"))?;
        let b = output.next_packet().map_err(|e| format!("output: read error after packet {packets} (frame {frames}): {e:?}"))?;

        let (a, b) = match (a, b) {
            (None, None) => break,
            (Some(a), None) => return Err(format!("output ends after packet {packets} (frame {frames}); source continues with {}", describe_packet(a))),
            (None, Some(b)) => return Err(format!("output continues after the source ended at packet {packets} (frame {frames}) with {}", describe_packet(b))),
            (Some(a), Some(b)) => (a, b),
        };

        match (a, b) {
            (Packet::Keyframe { metadata: ma, state: sa }, Packet::Keyframe { metadata: mb, state: sb }) => {
                check_eq(&format!("keyframe metadata at frame {}", ma.elapsed_frames), ma, mb)?;
                check_states(console, args.masks, ma.elapsed_frames, sa.as_slice(), sb.as_slice())?;
                keyframes += 1;
                reporter.report(frames, total);
            }
            _ => {
                if a != b {
                    return Err(format!("packet {packets} (frame {frames}) differs: source {}, output {}", describe_packet(a), describe_packet(b)));
                }
            }
        }

        if matches!(a, Packet::NextFrame { .. }) {
            frames += 1;
        }
        packets += 1;
    }
    eprintln!();

    eprintln!(
        "verified: {packets} packets, {keyframes} keyframes and {frames} frames are identical{} ({:.1} s)",
        if args.masks { " outside the masked buffers" } else { "" },
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(2);
        }
    };

    if let Err(message) = convert(&args) {
        eprintln!("{message}");
        return ExitCode::FAILURE;
    }

    if args.verify {
        if let Err(message) = verify(&args) {
            eprintln!("VERIFY FAILED: {message}");
            let _ = std::io::stderr().flush();
            return ExitCode::FAILURE;
        }
    }

    ExitCode::SUCCESS
}
