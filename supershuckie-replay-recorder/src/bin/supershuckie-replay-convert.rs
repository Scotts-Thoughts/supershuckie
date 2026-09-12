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

    let options = ConvertOptions {
        settings: ReplayFileRecorderSettings {
            minimum_uncompressed_bytes_per_blob: blob_bytes,
            max_frames_per_blob: chain_frames,
            compression_level: level,
            mask_transient_buffers: masks,
        },
        allow_corruption,
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
        let verified = verify_replay_files(&args.input, &args.output, args.options.settings.mask_transient_buffers, args.options.allow_corruption, &mut |phase, done, total| reporter.report(phase, done, total))
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
