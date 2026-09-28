//! `supershuckie-replay-convert`: re-encode a Super Shuckie replay file into the current format.
//!
//! Every keyframe of the source (v2 to v6) is re-fed through the recorder, so the output is a
//! current-format file with region-diffed keyframes, long delta chains carrying a keyframe offset
//! table (format v6) and zstd level 9 — typically 3-6x smaller than a v3 file — while the header,
//! patch, crop/timer markers, bookmarks, counters and every emulated frame are carried over
//! unchanged. Re-encoding a v4/v5 file gains the offset table (faster seeks) and, with a longer
//! `--chain-frames` than it was recorded with, a smaller file. `--verify` re-opens both files afterwards and
//! checks them packet by packet (keyframe states must reconstruct bit-exactly).
//!
//! ```text
//! supershuckie-replay-convert <in.replay> <out.replay>
//!     [--level 9] [--chain-frames 108000] [--blob-mb 1024] [--no-masks] [--verify]
//!     [--allow-corruption] [--force]
//! ```
//!
//! By default delta keyframes leave out regenerated output buffers (see
//! `supershuckie_replay_recorder::keyframe_masks`); `--no-masks` keeps every keyframe bit-exact.
//! With masks on, `--verify` compares keyframe states outside the masked ranges.
//!
//! The engine lives in `supershuckie_replay_recorder::replay_file::convert` (shared with the app).
//! Build with `cargo build --release -p supershuckie-replay-recorder --features convert`.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use supershuckie_replay_recorder::replay_file::convert::{
    convert_replay_file, human_duration, human_size, verify_replay_files, ConvertError, ConvertOptions, ConvertPhase,
};
use supershuckie_replay_recorder::replay_file::record::{
    ReplayFileRecorderSettings, DEFAULT_MAX_FRAMES_PER_BLOB, DEFAULT_MINIMUM_UNCOMPRESSED_BYTES_PER_BLOB,
    DEFAULT_ZSTD_COMPRESSION_LEVEL_V4,
};

const USAGE: &str = "\
usage: supershuckie-replay-convert <in.replay> <out.replay> [options]

Re-encodes a replay (format v2 to v6) into the current format (v6).

options:
  --level <n>          zstd compression level (default 9)
  --chain-frames <n>   frames per delta chain / compressed blob, 0 = unlimited (default 108000 = 30 min)
  --blob-mb <n>        hard cap on buffered uncompressed bytes per blob in MiB (default 1024)
  --no-masks           keep regenerated output buffers (3D geometry banks, mixed PCM) in delta
                       keyframes, i.e. every keyframe reconstructs bit-exactly
  --verify             re-open both files afterwards and compare them packet by packet
  --allow-corruption   read as much of a damaged source as possible instead of failing
  --rom <file>         Nintendo 3DS replays: the game's ROM file. Needed to read a v9 source;
                       with it the output's keyframes copy what they can from the ROM (and
                       playing the output needs the ROM, which the app always has)
  --keyframe-levels <a,b>  Nintendo 3DS replays: a level-1 keyframe every <b> keyframes, a full
                       one every <a> level-1 keyframes (default 30,15: with a recording's 8-s
                       keyframes, every 2 minutes and every 60)
  --force              overwrite <out.replay> if it exists
  -h, --help           show this help
";

struct Args {
    input: PathBuf,
    output: PathBuf,
    options: ConvertOptions,
    verify: bool,
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
    let mut rom_path = None;
    let mut levels = (30u32, 15u32);

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
            "--rom" => rom_path = Some(PathBuf::from(value("--rom")?)),
            "--keyframe-levels" => {
                let text = value("--keyframe-levels")?;
                let (a, b) = text.split_once(',').ok_or("--keyframe-levels: expected <a>,<b>")?;
                levels = (a.trim().parse().map_err(|e| format!("--keyframe-levels: {e}"))?, b.trim().parse().map_err(|e| format!("--keyframe-levels: {e}"))?);
            }
            other if other.starts_with('-') => return Err(format!("unknown option {other}\n\n{USAGE}")),
            _ => positional.push(PathBuf::from(arg)),
        }
    }

    let [input, output] = <[PathBuf; 2]>::try_from(positional).map_err(|_| USAGE.to_owned())?;

    let options = ConvertOptions {
        settings: ReplayFileRecorderSettings {
            minimum_uncompressed_bytes_per_blob: blob_bytes,
            max_frames_per_blob: chain_frames,
            compression_level: level,
            mask_transient_buffers: masks,
            stored_keyframe_levels: levels,
            stored_keyframe_compression_level: 3,
            stored_keyframe_mask_transients: true,
            stored_thumbnail_jpeg_quality: 75,
        },
        allow_corruption,
        rom_path,
    };

    Ok(Args { input, output, options, verify, force })
}

/// Prints `<what>... N%` to stderr, at most twice a second and only when the percentage changed.
struct ProgressReporter {
    started: Instant,
    last_print: Option<Instant>,
    last_percent: u64,
}

impl ProgressReporter {
    fn new() -> Self {
        Self { started: Instant::now(), last_print: None, last_percent: u64::MAX }
    }

    fn report(&mut self, phase: ConvertPhase, done: u64, total: u64) -> bool {
        let percent = done * 100 / total.max(1);
        let due = self.last_print.is_none_or(|t| t.elapsed().as_millis() >= 500);
        if percent != self.last_percent && due {
            self.last_percent = percent;
            self.last_print = Some(Instant::now());
            let what = match phase {
                ConvertPhase::Converting => "converting",
                ConvertPhase::Verifying => "verifying",
            };
            eprint!("\r{what}... {percent:3}% ({done} / {total} frames, {:.0} s)", self.started.elapsed().as_secs_f64());
        }
        true
    }
}

fn run(args: &Args) -> Result<(), String> {
    let mut reporter = ProgressReporter::new();
    let report = convert_replay_file(&args.input, &args.output, &args.options, args.force, &mut |phase, done, total| reporter.report(phase, done, total))
        .map_err(|e| match e {
            ConvertError::Cancelled => "cancelled".to_owned(),
            ConvertError::Failed(message) => message,
        })?;
    eprintln!();

    let source = &report.source;
    eprintln!(
        "{}: format v{}, {}, {} ({}), {} frames ({}), {} keyframes, {} blobs{}",
        args.input.display(),
        source.version,
        source.console,
        source.rom_name,
        human_size(source.size),
        source.frames,
        human_duration(source.millis),
        source.keyframes,
        source.blobs,
        if source.top_level_packets > 0 { format!(" + {} uncompressed top-level packets", source.top_level_packets) } else { String::new() }
    );
    eprintln!(
        "wrote {}: {} ({:.2}x smaller, {} frames) in {:.1} s",
        args.output.display(),
        human_size(report.output_size),
        source.size as f64 / report.output_size.max(1) as f64,
        report.frames,
        report.elapsed.as_secs_f64()
    );

    if args.verify {
        let mut reporter = ProgressReporter::new();
        let verified = verify_replay_files(&args.input, &args.output, args.options.settings.mask_transient_buffers, args.options.allow_corruption, args.options.rom_path.as_deref(), &mut |phase, done, total| reporter.report(phase, done, total))
            .map_err(|e| format!("VERIFY FAILED: {e}"))?;
        eprintln!();
        eprintln!(
            "verified: {} packets, {} keyframes and {} frames are identical{} ({:.1} s)",
            verified.packets,
            verified.keyframes,
            verified.frames,
            if verified.masked { " outside the masked buffers" } else { "" },
            verified.elapsed.as_secs_f64()
        );
    }

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

    if let Err(message) = run(&args) {
        eprintln!();
        eprintln!("{message}");
        let _ = std::io::stderr().flush();
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}
