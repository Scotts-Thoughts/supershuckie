//! Put the keyframes back into a replay that was thinned for the archive
//! (`supershuckie-replay-convert --keyframe-frames`), so that it seeks quickly again.
//!
//! ```text
//! replay_rehydrate <rom> <in.replay> <out.replay> [--keyframe-frames N] [--check N]
//!                  [--level L] [--force] [--user-dir DIR]
//! ```
//!
//! A keyframe is the emulator's state, so unlike thinning this needs the game: an emulator plays
//! `in.replay` and, wherever `N` frames have passed since the last keyframe
//! (default: what a recording has, 120 on a Nintendo DS and 240 on a Nintendo 3DS), its state
//! goes into the output as a keyframe. Everything else is copied as it is, which the converter's
//! verification then confirms packet by packet. With `--check N` (default 12, 0 to skip) that
//! many frames are then sought in both files and compared with what playing the source straight
//! through had on them: the emulator's state (what of it a seek must reproduce; see
//! `comparable`), the picture and the clock. The added keyframes are right when a seek that
//! starts from one ends where playback does.
//!
//! A Nintendo DS recording made before 2026-10-11 is refused: this build's states are longer
//! than its keyframes, and a keyframe of another length than the one before is stored whole
//! (`--allow-whole-keyframes` does it anyway).
//!
//! Nintendo DS and Nintendo 3DS. A DS replay rarely needs this (the app's seek index does the
//! same job in memory and on disk); a thinned 3DS one seeks for seconds without it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use supershuckie_core::emulator::{EmulatorCore, Nintendo3DS, Nintendo3DSSettings, NintendoDS};
use supershuckie_core::{std_timestamp_provider, SuperShuckieCore};
use supershuckie_replay_recorder::blake3_hash;
use supershuckie_replay_recorder::keyframe_masks::transient_ranges;
use supershuckie_replay_recorder::replay_file::convert::{convert_replay_file_adding_keyframes, verify_replay_files_with_added_keyframes, AddedKeyframes, ConvertOptions, ConvertPhase};
use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::replay_file::record::ReplayFileRecorderSettings;
use supershuckie_replay_recorder::replay_file::{ReplayConsoleType, RomBytes};

struct Setup {
    rom_path: PathBuf,
    rom: RomBytes,
    console: ReplayConsoleType,
    user_dir: PathBuf,
}

impl Setup {
    /// An emulator with `replay` attached at its first frame, seek index off.
    fn player_core(&self, replay: &RomBytes) -> Result<SuperShuckieCore, String> {
        let emulator: Box<dyn EmulatorCore> = match self.console {
            ReplayConsoleType::NintendoDS => Box::new(NintendoDS::new_from_rom((*self.rom).as_ref(), None, std_timestamp_provider(), false)?),
            ReplayConsoleType::Nintendo3DS => {
                let path = self.rom_path.to_str().ok_or("the ROM path is not UTF-8")?;
                let user = self.user_dir.to_str().ok_or("the user directory is not UTF-8")?;
                Box::new(Nintendo3DS::new_from_path(path, &[], user, std_timestamp_provider(), &Nintendo3DSSettings::default())?)
            },
            other => return Err(format!("{other:?} replays are not supported (their keyframes are cheap enough to keep)"))
        };
        let mut player = ReplayFilePlayer::new_shared(replay.clone(), false).map_err(|e| format!("cannot parse the replay: {e:?}"))?;
        player.enable_threading();
        if player.wants_rom() {
            player.set_rom(self.rom.clone());
        }
        let mut core = SuperShuckieCore::new(emulator, std_timestamp_provider());
        core.set_audio_enabled(false);
        core.set_ignore_speed_changes_in_replays(true);
        core.set_auto_resync_keyframes_in_replays(true);
        core.set_replay_seek_index_budget(0);
        core.attach_replay_player(player, true).map_err(|e| format!("cannot attach the replay: {e:?}"))?;
        Ok(core)
    }
}

/// What of the emulator's state a seek must reproduce whatever keyframe it started from.
///
/// Nintendo DS: the state without the 3D geometry banks, which a keyframe stored against the
/// one before leaves out (so they differ from playback until the game has rewritten every
/// slot), and without the render list that points into them (rebuilt at every flush; its unused
/// slots keep whatever an earlier scene left). The same comparison `seek_index_check` makes.
/// Nintendo 3DS: the game's heap, as the 3DS tools compare (the state itself holds GPU memory a
/// keyframe may leave as its reference had it).
fn comparable(core: &SuperShuckieCore, console: ReplayConsoleType) -> [u8; 32] {
    /// From the end of the geometry banks to the render list's count, and the list's length
    /// (melonDS's GPU3D state layout).
    const AFTER_POLYGON_RAM_TO_RENDER_COUNT: usize = 12 + 64 * 8 + 5 * 4;
    const RENDER_LIST_LEN: usize = 4 + 2048 * 4;

    if console == ReplayConsoleType::Nintendo3DS {
        return blake3_hash(core.get_core().memory_region_data(0).unwrap_or(&[]))
    }
    let mut state = core.create_save_state();
    let mut skip = transient_ranges(console, &state);
    if console == ReplayConsoleType::NintendoDS && let Some(banks) = skip.first().cloned() {
        let list = banks.end + AFTER_POLYGON_RAM_TO_RENDER_COUNT;
        skip.push(list..list + RENDER_LIST_LEN);
    }
    for range in skip {
        if range.end <= state.len() {
            state[range].fill(0);
        }
    }
    blake3_hash(&state)
}

/// The file's bytes without reading it all in (a Nintendo 3DS game is gigabytes).
fn map(path: &std::path::Path) -> Result<RomBytes, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    // SAFETY: only read, and nothing else is expected to change a ROM or a finished replay.
    let mapped = unsafe { memmap2::Mmap::map(&file) }.map_err(|e| format!("cannot map {}: {e}", path.display()))?;
    Ok(Arc::new(mapped))
}

/// Where an emulator stands after playing or seeking to a frame.
struct Landing {
    /// The recorded keyframe the last seek loaded.
    keyframe: u64,
    state: [u8; 32],
    picture: [u8; 32],
    clock: u64,
}

impl Landing {
    fn of(core: &SuperShuckieCore, console: ReplayConsoleType) -> Self {
        Self { keyframe: core.replay_keyframe_loaded(), state: comparable(core, console), picture: picture(core), clock: core.get_recording_milliseconds().0 }
    }
}

fn picture(core: &SuperShuckieCore) -> [u8; 32] {
    let mut pixels = Vec::new();
    for screen in core.get_core().get_screens() {
        for pixel in &screen.pixels {
            pixels.extend_from_slice(&pixel.to_le_bytes());
        }
    }
    blake3_hash(&pixels)
}

fn main() -> Result<(), String> {
    const USAGE: &str = "usage: replay_rehydrate <rom> <in.replay> <out.replay> [--keyframe-frames N] [--check N] [--level L] [--force] [--user-dir DIR] [--allow-whole-keyframes]";
    let mut args = std::env::args().skip(1);
    let rom_path: PathBuf = args.next().ok_or(USAGE)?.into();
    let input: PathBuf = args.next().ok_or(USAGE)?.into();
    let output: PathBuf = args.next().ok_or(USAGE)?.into();
    let (mut spacing, mut checks, mut level, mut force, mut allow_whole) = (0u64, 12usize, 9i32, false, false);
    let mut user_dir = std::env::temp_dir().join("supershuckie-replay-rehydrate").join("user");
    while let Some(a) = args.next() {
        let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
        match a.as_str() {
            "--keyframe-frames" => spacing = value("--keyframe-frames")?.parse().map_err(|e| format!("--keyframe-frames: {e}"))?,
            "--check" => checks = value("--check")?.parse().map_err(|e| format!("--check: {e}"))?,
            "--level" => level = value("--level")?.parse().map_err(|e| format!("--level: {e}"))?,
            "--user-dir" => user_dir = value("--user-dir")?.into(),
            "--force" => force = true,
            "--allow-whole-keyframes" => allow_whole = true,
            other => return Err(format!("unknown argument {other}\n{USAGE}"))
        }
    }
    std::fs::create_dir_all(&user_dir).map_err(|e| format!("cannot create {}: {e}", user_dir.display()))?;

    let rom = map(&rom_path)?;
    let source = map(&input)?;
    let source_len = (*source).as_ref().len();
    let (console, total, source_keyframes, source_state_len) = {
        let mut player = ReplayFilePlayer::new_shared(source.clone(), false).map_err(|e| format!("cannot parse {}: {e:?}", input.display()))?;
        if player.wants_rom() {
            player.set_rom(rom.clone());
        }
        player.go_to_keyframe(0).map_err(|e| format!("cannot read the first keyframe: {e:?}"))?;
        player.next_packet().map_err(|e| format!("cannot read the first keyframe: {e:?}"))?;
        (player.get_replay_metadata().console_type, player.get_total_frames(), player.all_keyframes().len(), player.current_keyframe_state().len())
    };
    if spacing == 0 {
        spacing = if console == ReplayConsoleType::Nintendo3DS { 240 } else { 120 };
    }
    let setup = Setup { rom_path: rom_path.clone(), rom, console, user_dir };
    println!("{}: {console:?}, {total} frames, {source_keyframes} keyframes, {:.1} MiB; adding one wherever {spacing} frames have none", input.display(), source_len as f64 / 1048576.0);

    // The frames sought afterwards to check the result: half of them right before a keyframe
    // interval of the output ends (the longest walk from an added keyframe), half at random.
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut random = |below: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % below.max(1)
    };
    let frames: Vec<u64> = (0..checks).map(|i| {
        let frame = 10 + random(total.saturating_sub(20));
        if i % 2 == 0 { (frame / spacing * spacing + spacing - 1).min(total.saturating_sub(2)) } else { frame }
    }).collect();
    let wanted: BTreeSet<u64> = frames.iter().copied().collect();
    // What playing the source straight through has at each of them: what every seek must match.
    let mut truth: BTreeMap<u64, Landing> = BTreeMap::new();

    // The emulator that makes the keyframes: it plays the source, and is asked for its state
    // after ever later frames.
    let mut maker = setup.player_core(&source)?;
    // Keyframes of a fixed-size console are stored against the one before only when both are
    // the same length. A recording from a build whose states were shorter (Nintendo DS ones
    // before 2026-10-11) would get every added keyframe, and every one of its own after an
    // added one, stored whole: several times the file it was before it was thinned.
    let made_len = maker.create_save_state().len();
    if console != ReplayConsoleType::Nintendo3DS && made_len != source_state_len && !allow_whole {
        return Err(format!(
            "this recording's keyframes are {source_state_len} bytes and this build's states are {made_len}: it was made by an older build, \
             and keyframes added now would each be stored whole (megabytes apiece). Not done; pass --allow-whole-keyframes to do it anyway. \
             The app's seek index makes such a recording seek quickly without changing the file."
        ))
    }
    // A Nintendo 3DS state holds what the GPU drew (its framebuffers are emulated memory), so
    // the walk that makes keyframes draws every frame, as the recording did. A Nintendo DS
    // state does not, and only the frames whose pictures are compared are drawn.
    let draw_all = console == ReplayConsoleType::Nintendo3DS;
    let started = Instant::now();
    let walk_to = |maker: &mut SuperShuckieCore, truth: &mut BTreeMap<u64, Landing>, frame: u64| -> Result<(), String> {
        while maker.total_frames() < frame {
            if maker.is_replay_stalled() {
                return Err(format!("playback stopped at frame {}", maker.total_frames()))
            }
            let next = maker.total_frames() + 1;
            if draw_all || wanted.range(next..next + 4).next().is_some() {
                maker.run_unlocked();
            }
            else {
                maker.run_unlocked_hidden();
            }
            let now = maker.total_frames();
            if wanted.contains(&now) && !maker.get_core().is_mid_frame() {
                truth.insert(now, Landing::of(maker, console));
            }
        }
        Ok(())
    };
    let report;
    let added_count;
    {
        let mut supplier = |frame: u64| -> Result<Option<Vec<u8>>, String> {
            if maker.total_frames() > frame {
                return Err(format!("the emulator is at frame {} already", maker.total_frames()))
            }
            walk_to(&mut maker, &mut truth, frame)?;
            if maker.total_frames() != frame || maker.get_core().is_mid_frame() || !maker.get_core().save_state_possible() {
                return Ok(None)
            }
            Ok(Some(maker.create_save_state()))
        };
        let mut added = AddedKeyframes { spacing, supplier: &mut supplier, added: 0 };
        let options = ConvertOptions {
            settings: ReplayFileRecorderSettings { compression_level: level, ..ReplayFileRecorderSettings::default() },
            rom_path: Some(rom_path.clone()),
            ..ConvertOptions::default()
        };
        let mut last = Instant::now();
        let mut progress = |phase: ConvertPhase, done: u64, all: u64| {
            if last.elapsed().as_secs() >= 5 {
                last = Instant::now();
                println!("  {}: frame {done} of {all}", if phase == ConvertPhase::Converting { "adding keyframes" } else { "verifying" });
            }
            true
        };
        report = convert_replay_file_adding_keyframes(&input, &output, &options, force, Some(&mut added), &mut progress).map_err(|e| e.to_string())?;
        added_count = added.added;
        println!(
            "{}: {added_count} keyframes added in {:.1} s, {:.1} MiB ({:+.0}%)",
            output.display(), started.elapsed().as_secs_f64(), report.output_size as f64 / 1048576.0,
            (report.output_size as f64 / source_len as f64 - 1.0) * 100.0
        );

        let verified = verify_replay_files_with_added_keyframes(&input, &output, options.settings.mask_transient_buffers, false, Some(&rom_path), &mut progress).map_err(|e| format!("verification FAILED: {e}"))?;
        println!("verified: {} packets, {} source keyframes and {} frames are the same in both files", verified.packets, verified.keyframes, verified.frames);
    }

    if checks == 0 {
        return Ok(())
    }
    // The rest of the replay, for the frames to check after the last keyframe added.
    if let Some(&last) = wanted.iter().next_back() {
        walk_to(&mut maker, &mut truth, last)?;
    }
    drop(maker);

    // Seeks to those frames in both files. One file after the other: only one Nintendo 3DS
    // emulator can exist at a time.
    let result = map(&output)?;
    let seek_all = |replay: &RomBytes| -> Result<(Vec<Landing>, f64), String> {
        let mut core = setup.player_core(replay)?;
        let mut landings = Vec::with_capacity(frames.len());
        let mut ms = 0.0f64;
        for &frame in &frames {
            let t = Instant::now();
            core.go_to_replay_frame(frame)?;
            ms += t.elapsed().as_secs_f64() * 1000.0;
            if core.total_frames() != frame {
                return Err(format!("a seek to frame {frame} landed on {}", core.total_frames()))
            }
            landings.push(Landing::of(&core, console));
        }
        Ok((landings, ms))
    };
    let (in_source, source_ms) = seek_all(&source)?;
    let (in_result, result_ms) = seek_all(&result)?;
    let same = |a: bool| if a { "same" } else { "DIFFERS" };
    let (mut wrong, mut source_wrong) = (0usize, 0usize);
    for ((frame, a), b) in frames.iter().zip(&in_source).zip(&in_result) {
        let Some(played) = truth.get(frame) else {
            println!("  frame {frame}: playback never stood on it; not compared");
            continue
        };
        let source_ok = a.state == played.state && a.picture == played.picture && a.clock == played.clock;
        let result_ok = b.state == played.state && b.picture == played.picture && b.clock == played.clock;
        source_wrong += !source_ok as usize;
        wrong += !result_ok as usize;
        println!(
            "  frame {frame}: a seek in the output (from keyframe {}) against playback: state {}, picture {}, clock {}; a seek in the source (from keyframe {}): state {}, picture {}, clock {}",
            b.keyframe, same(b.state == played.state), same(b.picture == played.picture), same(b.clock == played.clock),
            a.keyframe, same(a.state == played.state), same(a.picture == played.picture), same(a.clock == played.clock)
        );
    }
    println!(
        "{checks} seeks: {:.0} ms each in the source, {:.0} ms each in the output; {wrong} in the output and {source_wrong} in the source do not end where playback does",
        source_ms / checks as f64, result_ms / checks as f64
    );
    if wrong > source_wrong {
        return Err(String::from("seeks in the output go wrong where seeks in the source do not"))
    }
    if wrong > 0 {
        println!("(the source's own seeks do not reproduce playback either: a recording from an older build, whose keyframes this build's emulator does not continue exactly)");
    }
    Ok(())
}
