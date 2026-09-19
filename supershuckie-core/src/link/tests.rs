//! The link cable, end to end inside one process: two "machines", each with its own console and
//! a follower of the other's, exchanging link frames through their inboxes exactly as the
//! network would, running the test ROMs (see [`super::test_rom`]) through a 256-byte transfer
//! on the Game Boy, the Game Boy Color and the Game Boy Advance.

use super::test_rom::{self, DONE, ROLE_MASTER, ROLE_SLAVE};
use super::*;
use crate::emulator::link::LinkPort;
use crate::emulator::{EmulatorCore, GameBoyAdvance, GameBoyColor, Model, PartialReplayRecordMetadata};
use crate::live_replay::{live_replay_channel, FollowerStats, LiveReplayFeeder};
use crate::std_timestamp_provider;
use std::collections::VecDeque;
use std::vec;
use std::num::NonZeroU64;
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;
use supershuckie_replay_recorder::replay_file::record::{ReplayFileRecorderSettings, ReplayFileSink, ReplayFileWriteError};
use supershuckie_replay_recorder::replay_file::{ReplayFileMetadata, ReplayHeaderBytes, ReplayPatchFormat};
use supershuckie_replay_recorder::{ByteVec, KeyframeMetadata, Packet, Speed};

const DMG_BOOT: &[u8] = include_bytes!("../../../bootrom/dmg/dmg.bin");
const CGB_BOOT: &[u8] = include_bytes!("../../../bootrom/cgb/cgb_boot/cgb_boot_fast.bin");

/// Most frames a transfer is given before the test gives up.
const MAX_FRAMES: u64 = 900;

/// Which console a test runs on.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Console {
    Dmg,
    Cgb,
    Gba
}

impl Console {
    fn role_address(self) -> u32 {
        match self {
            Console::Gba => test_rom::GBA_ROLE_ADDRESS,
            _ => test_rom::ROLE_ADDRESS
        }
    }
    fn done_address(self) -> u32 {
        match self {
            Console::Gba => test_rom::GBA_DONE_ADDRESS,
            _ => test_rom::DONE_ADDRESS
        }
    }
    fn received_address(self) -> u32 {
        match self {
            Console::Gba => test_rom::GBA_RECEIVED_ADDRESS,
            _ => test_rom::RECEIVED_ADDRESS
        }
    }
    /// The work RAM compared between machines and replays (all of it for the Game Boy; the
    /// first 4 KiB of EWRAM for the Game Boy Advance, where the test ROM keeps everything).
    fn wram_range(self) -> (u32, usize) {
        match self {
            Console::Gba => (0x0200_0000, 0x1000),
            _ => (0xC000, 0x2000)
        }
    }
}

fn console_of(core: &SuperShuckieCore) -> Console {
    match core.get_core().replay_console_type() {
        Some(ReplayConsoleType::GameBoyAdvance) => Console::Gba,
        Some(ReplayConsoleType::GameBoyColor) => Console::Cgb,
        _ => Console::Dmg
    }
}

fn gb(color: bool) -> GameBoyColor {
    let rom = test_rom::build(color);
    if color {
        GameBoyColor::new_from_rom(&rom, CGB_BOOT, None, Model::Cgb0)
    }
    else {
        GameBoyColor::new_from_rom(&rom, DMG_BOOT, None, Model::DmgB)
    }
}

fn core(console: Console) -> SuperShuckieCore {
    let core: Box<dyn EmulatorCore> = match console {
        Console::Dmg => Box::new(gb(false)),
        Console::Cgb => Box::new(gb(true)),
        Console::Gba => Box::new(GameBoyAdvance::new_from_rom(&test_rom::build_gba(), None, &[], std_timestamp_provider()).expect("gba"))
    };
    SuperShuckieCore::new(core, std_timestamp_provider())
}

fn read(core: &SuperShuckieCore, address: u32, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    core.get_core().read_ram(address, &mut out).expect("read");
    out
}

fn done(core: &SuperShuckieCore) -> bool {
    read(core, console_of(core).done_address(), 1)[0] == DONE
}

fn received(core: &SuperShuckieCore) -> Vec<u8> {
    read(core, console_of(core).received_address(), 256)
}

fn run_alone_to(core: &mut SuperShuckieCore, frame: u64) {
    while core.total_frames() < frame {
        core.run_unlocked();
    }
}

/// A sink whose bytes stay reachable after the recorder is boxed away.
#[derive(Clone, Default)]
struct SharedSink(Arc<Mutex<Vec<u8>>>);

impl ReplayFileSink for SharedSink {
    fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), ReplayFileWriteError> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(())
    }
    fn truncate(&mut self, size: u64) -> Result<(), ReplayFileWriteError> {
        self.0.lock().unwrap().truncate(size as usize);
        Ok(())
    }
    fn overwrite_header(&mut self, header_data: &ReplayHeaderBytes) -> Result<(), ReplayFileWriteError> {
        let mut buf = self.0.lock().unwrap();
        if buf.len() > header_data.len() {
            buf[..header_data.len()].copy_from_slice(header_data);
        }
        else {
            buf.clear();
            buf.extend_from_slice(header_data);
        }
        Ok(())
    }
}

fn recording_metadata(final_file: SharedSink) -> PartialReplayRecordMetadata<SharedSink, SharedSink> {
    PartialReplayRecordMetadata {
        rom_name: "linktest".into(),
        rom_filename: "linktest.gb".into(),
        settings: ReplayFileRecorderSettings::default(),
        patch_format: ReplayPatchFormat::Unpatched,
        patch_target_checksum: Default::default(),
        patch_data: ByteVec::new(),
        frames_per_keyframe: NonZeroU64::new(30).unwrap(),
        final_file,
        temp_file: SharedSink::default()
    }
}

fn stream_metadata(core: &SuperShuckieCore) -> ReplayFileMetadata {
    ReplayFileMetadata {
        console_type: core.get_core().replay_console_type().unwrap(),
        rom_name: "linktest".into(),
        rom_filename: "linktest.gb".into(),
        rom_checksum: *core.get_core().rom_checksum(),
        bios_checksum: *core.get_core().bios_checksum(),
        emulator_core_name: core.get_core().core_name().into(),
        patch_format: ReplayPatchFormat::Unpatched,
        patch_target_checksum: Default::default(),
        crop_start: None,
        crop_end: None,
        timer_offset: None
    }
}

/// A link publisher that delivers straight into the other machine's inbox, or holds the frames
/// back until told to deliver.
struct TestPublisher {
    to: Arc<LinkInbox>,
    held: Arc<Mutex<VecDeque<(u64, LinkFrame)>>>,
    hold: Arc<Mutex<bool>>,
    sent: Arc<Mutex<Vec<u64>>>
}

impl LinkPublisherFns for TestPublisher {
    fn frame(&mut self, frame: u64, elapsed_millis: u64, events: Vec<Packet>, pair_hash: Option<(u64, [u8; 32])>) {
        self.sent.lock().unwrap().push(frame);
        let link_frame = LinkFrame { events, elapsed_millis, pair_hash };
        if *self.hold.lock().unwrap() {
            self.held.lock().unwrap().push_back((frame, link_frame));
        }
        else {
            self.to.push(frame, link_frame);
        }
    }

    fn poll_errors(&mut self) -> Vec<String> {
        Vec::new()
    }
}

/// One player's machine: their console and the (lent) follower of the other player's.
struct Machine {
    local: SuperShuckieCore,
    follower: SuperShuckieCore,
    /// Where the other machine's link frames arrive (the follower's inbox).
    inbox: Arc<LinkInbox>,
    _feeder: LiveReplayFeeder,
    follower_stats: Arc<FollowerStats>,
    local_file: SharedSink,
    follower_file: SharedSink,
    /// Frames the local publisher held back, and the switch.
    held: Arc<Mutex<VecDeque<(u64, LinkFrame)>>>,
    hold: Arc<Mutex<bool>>,
    sent: Arc<Mutex<Vec<u64>>>
}

/// Build the follower of `of` (at its held frame) on a fresh core, with a file of its own.
fn follower_of(of: &SuperShuckieCore, start_frame: u64, input: &InputBuffer, console: Console) -> (SuperShuckieCore, LiveReplayFeeder, Arc<FollowerStats>, SharedSink) {
    let mut follower = core(console);
    let stats = Arc::new(FollowerStats::default());
    let (tx, _rx) = channel();
    let (feeder, source) = live_replay_channel(stats.clone(), tx);
    let metadata = stream_metadata(of);
    follower.attach_live_replay_source(source, &metadata, false).expect("attach");
    let file = SharedSink::default();
    follower.start_recording_follower_replay(recording_metadata(file.clone()), metadata).expect("follower file");
    feeder.push_packet(Packet::Keyframe {
        metadata: KeyframeMetadata { input: input.clone(), speed: Speed::default(), elapsed_frames: start_frame, elapsed_millis: 0.into(), counters: Vec::new() },
        state: ByteVec::Heap(of.create_save_state())
    });
    follower.run_unlocked_presenting(true);
    assert_eq!(follower.total_frames(), start_frame, "the follower sits at the held frame");
    assert!(follower.is_replay_waiting());
    (follower, feeder, stats, file)
}

/// Two machines linked with `delay` frames, `a` being the first console, both consoles held a
/// few frames in (with `a` recording its own game to a file).
fn link_two(console: Console, delay: u64) -> (Machine, Machine) {
    let mut a = core(console);
    let mut b = core(console);
    run_alone_to(&mut a, 5);
    run_alone_to(&mut b, 7);
    let a_file = SharedSink::default();
    let b_file = SharedSink::default();
    a.start_recording_replay(recording_metadata(a_file.clone())).expect("record a");
    b.start_recording_replay(recording_metadata(b_file.clone())).expect("record b");
    run_alone_to(&mut a, 3);
    run_alone_to(&mut b, 2);

    let (a_frame, a_input) = a.link_hold().expect("hold a");
    let (b_frame, b_input) = b.link_hold().expect("hold b");
    assert_eq!(a_frame, 3);
    assert_eq!(b_frame, 2);

    // Machine 1 plays a and follows b; machine 2 the other way round.
    let (b_on_1, feeder_b, stats_b, b_on_1_file) = follower_of(&b, b_frame, &b_input, console);
    let (a_on_2, feeder_a, stats_a, a_on_2_file) = follower_of(&a, a_frame, &a_input, console);
    let inbox_1 = Arc::new(LinkInbox::new()); // b's frames, for b_on_1
    let inbox_2 = Arc::new(LinkInbox::new()); // a's frames, for a_on_2

    let mut machine_1 = Machine {
        local: a,
        follower: b_on_1,
        inbox: inbox_1.clone(),
        _feeder: feeder_b,
        follower_stats: stats_b,
        local_file: a_file,
        follower_file: b_on_1_file,
        held: Arc::new(Mutex::new(VecDeque::new())),
        hold: Arc::new(Mutex::new(false)),
        sent: Arc::new(Mutex::new(Vec::new()))
    };
    let mut machine_2 = Machine {
        local: b,
        follower: a_on_2,
        inbox: inbox_2.clone(),
        _feeder: feeder_a,
        follower_stats: stats_a,
        local_file: b_file,
        follower_file: a_on_2_file,
        held: Arc::new(Mutex::new(VecDeque::new())),
        hold: Arc::new(Mutex::new(false)),
        sent: Arc::new(Mutex::new(Vec::new()))
    };

    let publisher_1 = TestPublisher { to: inbox_2, held: machine_1.held.clone(), hold: machine_1.hold.clone(), sent: machine_1.sent.clone() };
    let publisher_2 = TestPublisher { to: inbox_1, held: machine_2.held.clone(), hold: machine_2.hold.clone(), sent: machine_2.sent.clone() };

    machine_1.local.begin_link(
        &mut machine_1.follower,
        LinkSettings { delay_frames: delay, local_is_first: true, local_start_frame: a_frame, partner_start_frame: b_frame, partner_start_input: b_input.clone() },
        machine_1.inbox.clone(),
        Box::new(publisher_1)
    ).expect("link on machine 1");
    machine_2.local.begin_link(
        &mut machine_2.follower,
        LinkSettings { delay_frames: delay, local_is_first: false, local_start_frame: b_frame, partner_start_frame: a_frame, partner_start_input: a_input },
        machine_2.inbox.clone(),
        Box::new(publisher_2)
    ).expect("link on machine 2");
    machine_1.local.set_link_paced(false);
    machine_2.local.set_link_paced(false);
    (machine_1, machine_2)
}

fn wram(core: &SuperShuckieCore) -> Vec<u8> {
    let (address, len) = console_of(core).wram_range();
    read(core, address, len)
}

fn assert_ran(outcome: &LinkRunOutcome, what: &str) {
    assert!(matches!(outcome, LinkRunOutcome::Ran { .. }), "{what}: {outcome:?}");
}

/// Run the two machines like two real machines would: each at its own pace, but with no clock
/// here the one whose console is behind in link frames goes next, so neither runs away from the
/// other. Stops when `stop` says so or once both consoles have completed `frames` more link
/// frames. A machine may stall briefly waiting for the other's frames (the other then runs);
/// both stalling at once, or a failure, is a test failure.
fn run_pair(m1: &mut Machine, m2: &mut Machine, frames: u64, mut stop: impl FnMut(&Machine, &Machine) -> bool) {
    let progress = |m: &Machine| m.local.link_progress().map(|(f, _)| f).unwrap_or(0);
    let target_1 = progress(m1) + frames;
    let target_2 = progress(m2) + frames;
    let mut steps = 0u64;
    while (progress(m1) < target_1 || progress(m2) < target_2) && !stop(m1, m2) {
        let first = if progress(m1) <= progress(m2) { 1 } else { 2 };
        let outcome = if first == 1 { m1.local.run_linked(&mut m1.follower) } else { m2.local.run_linked(&mut m2.follower) };
        match outcome {
            LinkRunOutcome::Ran { .. } => {}
            LinkRunOutcome::Stalled => {
                let other = if first == 1 { m2.local.run_linked(&mut m2.follower) } else { m1.local.run_linked(&mut m1.follower) };
                assert_ran(&other, "both machines stalled");
            }
            LinkRunOutcome::Failed(failure) => panic!("machine {first} failed: {failure:?}")
        }
        steps += 1;
        assert!(steps < 400_000_000, "runaway");
    }
}

/// The frame index (count of `NextFrame`s before it) of every packet matching `matches` in a
/// replay, oldest first.
fn packet_frames(bytes: &[u8], matches: impl Fn(&Packet) -> bool) -> Vec<u64> {
    let mut player = ReplayFilePlayer::new(bytes, false).expect("parse");
    player.set_keyframe_states_wanted(false);
    player.go_to_keyframe(0).unwrap();
    let mut frames = 0u64;
    let mut found = Vec::new();
    while let Some(packet) = player.next_packet().unwrap() {
        if matches!(packet, Packet::NextFrame { .. }) {
            frames += 1;
        }
        else if matches(packet) {
            found.push(frames);
        }
    }
    found
}

fn play_back_alone(bytes: &[u8], console: Console) -> SuperShuckieCore {
    let mut playback = core(console);
    let player = ReplayFilePlayer::new(bytes, false).expect("parse");
    let total = player.get_total_frames();
    playback.attach_replay_player(player, false).expect("attach");
    while playback.total_frames() < total && !playback.is_replay_stalled() {
        playback.run_unlocked();
    }
    assert_eq!(playback.total_frames(), total, "played to the end");
    playback
}

/// The main event: both machines run the pair, the transfer completes on all four consoles with
/// the right bytes, both machines hold the same memory for both consoles, the pair hashes agree,
/// and the four replay files (each player's own and each player's friend file) reach the same
/// state on their own.
fn exchanges_bytes_on_both_machines(console: Console, delay: u64) {
    let (mut m1, mut m2) = link_two(console, delay);

    // Roles, scheduled like any input: both consoles learn them `delay` frames in.
    assert!(m1.local.enqueue_write(console.role_address(), ByteVec::from(&test_rom::role_write(ROLE_MASTER)[..])));
    assert!(m2.local.enqueue_write(console.role_address(), ByteVec::from(&test_rom::role_write(ROLE_SLAVE)[..])));

    run_pair(&mut m1, &mut m2, MAX_FRAMES, |m1, m2| done(&m1.local) && done(&m2.local) && done(&m1.follower) && done(&m2.follower));
    assert!(done(&m1.local), "a finished on machine 1");
    assert!(done(&m2.local), "b finished on machine 2");
    assert!(done(&m1.follower), "b finished on machine 1");
    assert!(done(&m2.follower), "a finished on machine 2");
    assert_eq!(received(&m1.local), test_rom::expected_received(ROLE_MASTER), "the master got the slave's bytes");
    assert_eq!(received(&m2.local), test_rom::expected_received(ROLE_SLAVE), "the slave got the master's bytes");

    // Let the pair run past the next pair hash so that every hash sent so far has been checked.
    run_pair(&mut m1, &mut m2, 130, |_, _| false);
    assert!(m1.local.link_failure().is_none(), "{:?}", m1.local.link_failure());
    assert!(m2.local.link_failure().is_none(), "{:?}", m2.local.link_failure());
    assert!(!m1.sent.lock().unwrap().is_empty() && !m2.sent.lock().unwrap().is_empty());

    // Both machines computed the same consoles.
    assert_eq!(wram(&m1.local), wram(&m2.follower), "a on both machines");
    assert_eq!(wram(&m2.local), wram(&m1.follower), "b on both machines");
    assert_eq!(m1.local.serial_replay_misses(), 0);
    assert_eq!(m1.follower.serial_replay_misses(), 0);
    assert_eq!(m2.local.serial_replay_misses(), 0);
    assert_eq!(m2.follower.serial_replay_misses(), 0);
    assert_eq!(m1.follower_stats.hash_mismatches.load(Ordering::Relaxed), 0);
    assert_eq!(m2.follower_stats.hash_mismatches.load(Ordering::Relaxed), 0);

    // Unplug: the local consoles run alone again, the followers wait for a snapshot.
    let a_wram = wram(&m1.local);
    let b_wram = wram(&m2.local);
    m1.local.end_link(&mut m1.follower);
    m2.local.end_link(&mut m2.follower);
    assert!(!m1.local.is_linked() && !m1.follower.is_linked());
    let target = m1.local.total_frames() + 5;
    run_alone_to(&mut m1.local, target);
    // The follower finishes the frame it was in, then waits for a snapshot from the stream.
    let follower_frames = m1.follower.total_frames();
    for _ in 0..100_000 {
        m1.follower.run_unlocked_presenting(true);
        if m1.follower.is_replay_waiting() {
            break
        }
    }
    assert!(m1.follower.is_replay_waiting(), "a follower without a snapshot waits");
    assert!(m1.follower.total_frames() <= follower_frames + 1, "...after finishing at most the frame it was in");
    assert_eq!(m1.local.stop_recording_replay(), Some(true));
    assert_eq!(m2.local.stop_recording_replay(), Some(true));
    m1.follower.detach_live_source();
    m2.follower.detach_live_source();

    // Every file plays back on its own to the state the live console had at its end: the link
    // traffic came with it (`SerialIn`), so no partner is needed.
    let a_file = m1.local_file.0.lock().unwrap().clone();
    let b_file = m2.local_file.0.lock().unwrap().clone();
    let b_on_1_file = m1.follower_file.0.lock().unwrap().clone();
    let a_on_2_file = m2.follower_file.0.lock().unwrap().clone();
    for (what, bytes, expected) in [("a's own file", &a_file, &wram(&m1.local)), ("b's own file", &b_file, &wram(&m2.local)), ("b as followed on machine 1", &b_on_1_file, &b_wram), ("a as followed on machine 2", &a_on_2_file, &a_wram)] {
        let mut playback = play_back_alone(bytes, console);
        assert_eq!(&wram(&playback), expected, "{what}: the replay reaches the live state");
        assert_eq!(playback.serial_replay_misses(), 0, "{what}: every recorded bit was delivered where it was recorded");
        assert!(done(&playback), "{what}: the transfer completed in the replay");
        let mut player = ReplayFilePlayer::new(bytes, false).unwrap();
        player.set_keyframe_states_wanted(false);
        let mut serial_ins = 0u64;
        let mut inputs = 0u64;
        let mut frames = 0u64;
        while let Some(packet) = player.next_packet().unwrap() {
            match packet {
                Packet::SerialIn { .. } => serial_ins += 1,
                Packet::ChangeInput { .. } => inputs += 1,
                Packet::NextFrame { .. } => frames += 1,
                _ => {}
            }
        }
        assert!(serial_ins > 0 && serial_ins <= frames, "{what}: {serial_ins} SerialIn packets over {frames} frames");
        assert!(inputs <= frames + 2, "{what}: {inputs} ChangeInput packets over {frames} frames");
    }
}

#[test]
fn linked_pair_exchanges_bytes_dmg() {
    exchanges_bytes_on_both_machines(Console::Dmg, 3);
}

#[test]
fn linked_pair_exchanges_bytes_gba() {
    exchanges_bytes_on_both_machines(Console::Gba, 3);
}

#[test]
fn linked_pair_exchanges_bytes_cgb_with_one_frame_delay() {
    exchanges_bytes_on_both_machines(Console::Cgb, 1);
}

/// Swapping which console is "first" changes nothing about the outcome: it is the same rule
/// seen from the other side.
#[test]
fn both_perspectives_agree() {
    let (mut m1, mut m2) = link_two(Console::Dmg, 2);
    assert!(m1.local.enqueue_write(test_rom::ROLE_ADDRESS, ByteVec::from(&test_rom::role_write(ROLE_SLAVE)[..])));
    assert!(m2.local.enqueue_write(test_rom::ROLE_ADDRESS, ByteVec::from(&test_rom::role_write(ROLE_MASTER)[..])));
    run_pair(&mut m1, &mut m2, MAX_FRAMES, |m1, m2| done(&m1.local) && done(&m2.local) && done(&m1.follower) && done(&m2.follower));
    assert!(done(&m1.local) && done(&m2.local) && done(&m1.follower) && done(&m2.follower));
    assert_eq!(read(&m1.local, test_rom::RECEIVED_ADDRESS, 256), test_rom::expected_received(ROLE_SLAVE));
    assert_eq!(read(&m2.local, test_rom::RECEIVED_ADDRESS, 256), test_rom::expected_received(ROLE_MASTER));
    for _ in 0..3 {
        run_pair(&mut m1, &mut m2, 61, |_, _| false);
        assert_eq!(wram(&m1.local), wram(&m2.follower));
        assert_eq!(wram(&m2.local), wram(&m1.follower));
    }
    assert!(m1.local.link_failure().is_none() && m2.local.link_failure().is_none());
}

/// A write scheduled at link frame `n` lands on both machines in the same frame, `n + delay`
/// (as both machines' replay files show), a reset likewise and first among a frame's events;
/// unlinking drops what was still scheduled.
#[test]
fn inputs_writes_and_resets_are_delayed_and_land_together() {
    const DELAY: u64 = 4;
    let (mut m1, mut m2) = link_two(Console::Dmg, DELAY);
    let probe = 0xC200u32;
    assert_eq!(m1.local.link_progress().unwrap().0, 0);

    // Not on either machine for `DELAY` frames, then on both.
    assert!(m1.local.enqueue_write(probe, ByteVec::from(&[0x5A][..])));
    run_pair(&mut m1, &mut m2, DELAY, |_, _| false);
    assert_ne!(read(&m1.local, probe, 1)[0], 0x5A, "not before the delay");
    run_pair(&mut m1, &mut m2, 2, |_, _| false);
    assert_eq!(read(&m1.local, probe, 1)[0], 0x5A, "on machine 1");
    assert_eq!(read(&m2.follower, probe, 1)[0], 0x5A, "on machine 2");

    // A reset: the ROM restarts and clears its flags, on both machines.
    assert!(m2.local.enqueue_write(0xC001, ByteVec::from(&[0x77][..])));
    run_pair(&mut m1, &mut m2, DELAY + 2, |_, _| false);
    assert_eq!(read(&m2.local, 0xC001, 1)[0], 0x77);
    assert_eq!(read(&m1.follower, 0xC001, 1)[0], 0x77);
    let epoch_before = m2.local.state_epoch();
    m2.local.hard_reset();
    assert_eq!(m2.local.state_epoch(), epoch_before, "a reset is scheduled, not applied now");
    // The reset lands `DELAY` frames on; the boot ROM then takes about three frames to clear
    // VRAM before the ROM clears its flags.
    run_pair(&mut m1, &mut m2, DELAY + 6, |_, _| false);
    assert_eq!(read(&m2.local, 0xC001, 1)[0], 0, "reset on machine 2");
    assert_eq!(read(&m1.follower, 0xC001, 1)[0], 0, "reset on machine 1");
    assert!(m2.local.state_epoch() > epoch_before);
    assert_eq!(wram(&m2.local), wram(&m1.follower), "a reset leaves the same memory on both machines");

    // Unlinking drops the frames still scheduled: a write made just before is never applied.
    assert!(m1.local.enqueue_write(probe, ByteVec::from(&[0xEE][..])));
    m1.local.end_link(&mut m1.follower);
    m2.local.end_link(&mut m2.follower);
    let target = m1.local.total_frames() + DELAY + 2;
    run_alone_to(&mut m1.local, target);
    assert_ne!(read(&m1.local, probe, 1)[0], 0xEE, "scheduled inputs are dropped at unlink");
    // ...and the next write is immediate again.
    assert!(m1.local.enqueue_write(probe, ByteVec::from(&[0xEF][..])));
    assert_eq!(read(&m1.local, probe, 1)[0], 0xEF);

    // The files agree on the frame everything landed in: a's own file and a as followed on
    // machine 2 have the write before the same link frame (`delay` frames after the link
    // started); b's files have the reset before the same link frame.
    assert_eq!(m1.local.stop_recording_replay(), Some(true));
    assert_eq!(m2.local.stop_recording_replay(), Some(true));
    m1.follower.detach_live_source();
    m2.follower.detach_live_source();
    let a_file = m1.local_file.0.lock().unwrap().clone();
    let a_on_2_file = m2.follower_file.0.lock().unwrap().clone();
    let b_file = m2.local_file.0.lock().unwrap().clone();
    let b_on_1_file = m1.follower_file.0.lock().unwrap().clone();
    let is_probe_write = |p: &Packet| matches!(p, Packet::WriteMemory { address, data } if *address == probe as u64 && data.as_slice() == [0x5A]);
    let a_writes = packet_frames(&a_file, is_probe_write);
    let a_on_2_writes = packet_frames(&a_on_2_file, is_probe_write);
    // a's own file started 3 frames before the hold (it records from its start), the follower's
    // at the held frame: the same link frame is 3 file frames later in a's own file.
    assert_eq!(a_writes.len(), 1, "{a_writes:?}");
    assert_eq!(a_on_2_writes.len(), 1, "{a_on_2_writes:?}");
    assert_eq!(a_writes[0], 3 + DELAY, "the write sits before link frame `delay` in a's own file");
    assert_eq!(a_on_2_writes[0], DELAY, "and before the same link frame in a as followed on machine 2");
    let b_resets = packet_frames(&b_file, |p| matches!(p, Packet::ResetConsole));
    let b_on_1_resets = packet_frames(&b_on_1_file, |p| matches!(p, Packet::ResetConsole));
    assert_eq!(b_resets.len(), 1, "{b_resets:?}");
    assert_eq!(b_on_1_resets.len(), 1, "{b_on_1_resets:?}");
    assert_eq!(b_resets[0], b_on_1_resets[0] + 2, "the reset sits before the same link frame in both of b's files");
}

/// Link frames that do not arrive stall the pair (both consoles); they resume when the frames
/// come, and the outcome is unchanged.
#[test]
fn missing_link_frames_stall_the_pair_until_they_arrive() {
    const DELAY: u64 = 3;
    let (mut m1, mut m2) = link_two(Console::Dmg, DELAY);
    assert!(m1.local.enqueue_write(test_rom::ROLE_ADDRESS, ByteVec::from(&test_rom::role_write(ROLE_MASTER)[..])));
    assert!(m2.local.enqueue_write(test_rom::ROLE_ADDRESS, ByteVec::from(&test_rom::role_write(ROLE_SLAVE)[..])));

    // Machine 2 stops delivering: machine 1 runs its pre-filled frames, then stalls (and so,
    // `DELAY` frames further, does machine 2, waiting for machine 1's).
    *m2.hold.lock().unwrap() = true;
    let mut stalled = false;
    for _ in 0..(DELAY + 2) * 400_000 {
        match m1.local.run_linked(&mut m1.follower) {
            LinkRunOutcome::Ran { .. } => {}
            LinkRunOutcome::Stalled => {
                stalled = true;
                break
            }
            other => panic!("machine 1: {other:?}")
        }
    }
    assert!(stalled, "machine 1 should stall without machine 2's frames");
    assert!(m1.local.is_link_stalled());
    let progress = m1.local.link_progress().unwrap().0;
    assert!(progress <= DELAY, "stalled once the pre-filled frames ran out: {progress}");
    let frames_before = m1.local.total_frames();
    for _ in 0..100 {
        assert_eq!(m1.local.run_linked(&mut m1.follower), LinkRunOutcome::Stalled);
    }
    assert_eq!(m1.local.total_frames(), frames_before, "nothing runs while stalled");
    let mut stalled_2 = false;
    for _ in 0..(DELAY + 2) * 400_000 {
        match m2.local.run_linked(&mut m2.follower) {
            LinkRunOutcome::Ran { .. } => {}
            LinkRunOutcome::Stalled => {
                stalled_2 = true;
                break
            }
            other => panic!("machine 2: {other:?}")
        }
    }
    assert!(stalled_2, "machine 2 stalls too once it has used up machine 1's frames");
    assert!(m2.local.link_progress().unwrap().0 <= progress + DELAY + 2, "{}", m2.local.link_progress().unwrap().0);

    // Deliver everything held back: the pair resumes and finishes like an uninterrupted one.
    *m2.hold.lock().unwrap() = false;
    for (frame, link_frame) in m2.held.lock().unwrap().drain(..) {
        m1.inbox.push(frame, link_frame);
    }
    assert_ran(&m1.local.run_linked(&mut m1.follower), "machine 1 after delivery");
    assert!(!m1.local.is_link_stalled());
    run_pair(&mut m1, &mut m2, MAX_FRAMES, |m1, m2| done(&m1.local) && done(&m2.local) && done(&m1.follower) && done(&m2.follower));
    assert!(done(&m1.local) && done(&m2.local) && done(&m1.follower) && done(&m2.follower));
    assert_eq!(wram(&m1.local), wram(&m2.follower));
    assert_eq!(wram(&m2.local), wram(&m1.follower));
}

/// A divergence between the machines (here a stray write on one machine's follower) is caught
/// by the pair hash within a second of the first console's frames, and the partner going away
/// ends the link.
#[test]
fn desync_and_partner_loss_fail_the_link() {
    let (mut m1, mut m2) = link_two(Console::Dmg, 2);
    run_pair(&mut m1, &mut m2, 5, |_, _| false);
    // Corrupt b as machine 1 sees it (a write straight into the follower, bypassing the link).
    m1.follower.core.write_ram(0xC300, &[1, 2, 3, 4]).unwrap();
    let mut failed = None;
    for _ in 0..(PAIR_HASH_INTERVAL_FRAMES * 3) as usize * 40_000 {
        let o1 = m1.local.run_linked(&mut m1.follower);
        let o2 = m2.local.run_linked(&mut m2.follower);
        for (machine, outcome) in [(1, o1), (2, o2)] {
            match outcome {
                LinkRunOutcome::Failed(failure) => {
                    failed = Some((machine, failure));
                    break
                }
                LinkRunOutcome::Stalled => {}
                LinkRunOutcome::Ran { .. } => {}
            }
        }
        if failed.is_some() {
            break
        }
    }
    let (machine, failure) = failed.expect("the pair hash catches the divergence");
    assert!(matches!(failure, LinkFailure::PairHashMismatch { .. }), "machine {machine}: {failure:?}");
    assert!(m1.local.link_failure().is_some() || m2.local.link_failure().is_some());
    m1.local.end_link(&mut m1.follower);
    m2.local.end_link(&mut m2.follower);

    // A partner whose frames end for good.
    let (mut m1, mut m2) = link_two(Console::Dmg, 2);
    run_pair(&mut m1, &mut m2, 3, |_, _| false);
    m1.inbox.end();
    let mut ended = false;
    for _ in 0..200_000 {
        match m1.local.run_linked(&mut m1.follower) {
            LinkRunOutcome::Failed(LinkFailure::PartnerEnded) => {
                ended = true;
                break
            }
            LinkRunOutcome::Failed(other) => panic!("{other:?}"),
            _ => {}
        }
    }
    assert!(ended);
}

/// What a link refuses, and what it does not.
#[test]
fn refusals_while_held_and_linked() {
    let (mut m1, mut m2) = link_two(Console::Dmg, 2);
    let state = m1.local.create_save_state();
    run_pair(&mut m1, &mut m2, 2, |_, _| false);

    let epoch = m1.local.state_epoch();
    m1.local.load_save_state(&state);
    assert_eq!(m1.local.state_epoch(), epoch, "a state load is refused while linked");
    m1.local.set_speed(Speed::from_multiplier_float(4.0));
    assert_eq!(m1.local.game_speed, Speed::from_multiplier_float(1.0), "speed changes are refused while linked");
    assert!(m1.local.link_hold().is_err(), "cannot hold while linked");
    let player = ReplayFilePlayer::new(&m1.local_file.0.lock().unwrap().clone(), false);
    if let Ok(player) = player {
        assert!(m1.local.attach_replay_player(player, true).is_err(), "no replay while linked");
    }

    // A console held for a link runs nothing until released or linked.
    let mut lone = core(Console::Dmg);
    run_alone_to(&mut lone, 4);
    let (frame, _) = lone.link_hold().unwrap();
    assert_eq!(frame, 4);
    for _ in 0..50 {
        lone.run_unlocked();
    }
    assert_eq!(lone.total_frames(), 4, "held: nothing runs");
    lone.link_release();
    run_alone_to(&mut lone, 6);
    assert_eq!(lone.total_frames(), 6);

    m1.local.end_link(&mut m1.follower);
    m2.local.end_link(&mut m2.follower);
}

/// Two consoles linked directly at the port level, with audio on: the shadow instances receive
/// the same bits as the emulated ones and never need a resync over the whole transfer.
#[test]
fn shadow_audio_follows_serial() {
    let mut master = gb(false);
    let mut slave = gb(false);
    for gb in [&mut master, &mut slave] {
        gb.set_audio_enabled(true);
        // A couple of frames in; the role may be handed over any time, even during the boot ROM.
        let mut frames = 0;
        while frames < 2 {
            frames += gb.run_unlocked().frames;
        }
    }
    master.write_ram(test_rom::ROLE_ADDRESS, &test_rom::role_write(ROLE_MASTER)).unwrap();
    slave.write_ram(test_rom::ROLE_ADDRESS, &test_rom::role_write(ROLE_SLAVE)).unwrap();
    master.connect(true).unwrap();
    slave.connect(false).unwrap();
    let mut steps = 0u64;
    loop {
        let second_runs = crate::emulator::link::second_runs_next(&master, &slave).unwrap();
        if second_runs {
            slave.step_linked(&mut master, false).unwrap();
        }
        else {
            master.step_linked(&mut slave, false).unwrap();
        }
        steps += 1;
        let mut done = [0u8; 2];
        master.read_ram(test_rom::DONE_ADDRESS, &mut done[..1]).unwrap();
        slave.read_ram(test_rom::DONE_ADDRESS, &mut done[1..]).unwrap();
        if done == [DONE, DONE] {
            break
        }
        assert!(steps < 100_000_000, "the transfer never finished");
    }
    let mut got = vec![0u8; 256];
    master.read_ram(test_rom::RECEIVED_ADDRESS, &mut got).unwrap();
    assert_eq!(got, test_rom::expected_received(ROLE_MASTER));
    slave.read_ram(test_rom::RECEIVED_ADDRESS, &mut got).unwrap();
    assert_eq!(got, test_rom::expected_received(ROLE_SLAVE));
    assert_eq!(master.audio_resyncs(), 0, "the master's shadow followed the transfer");
    assert_eq!(slave.audio_resyncs(), 0, "the slave's shadow followed the transfer");
    assert!(master.serial_replay_misses() == 0 && slave.serial_replay_misses() == 0);

    // Unplugging mid-way is what the game sees as no cable: the master reads 1s.
    master.disconnect();
    slave.disconnect();
    assert!(!master.is_live() && !slave.is_live());
}

/// Malformed link traffic in a replay is refused without touching the port, and a recording
/// played back into a port that is live is refused too.
#[test]
fn malformed_serial_in_is_refused() {
    let mut gb = gb(false);
    assert!(gb.queue_serial_in(&[0x09]).is_err());
    assert!(!gb.is_live());
    gb.connect(true).unwrap();
    assert!(gb.queue_serial_in(&[]).is_err(), "a live port takes no recording");
    gb.disconnect();
    assert!(gb.queue_serial_in(&[0x04, 0x01]).is_ok(), "a clean packet puts the port in replay mode");
    assert!(!gb.is_live());
}



/// The whole thing over real threads: two players' cores on their own (primary) threads, each
/// with a follower of the other lent to it, linked through inboxes as the session would do it,
/// exchanging the 256 bytes at 8x and unlinking cleanly.
#[test]
fn link_over_threads() {
    use crate::{CoreThreadRole, LinkStatus, ThreadedSuperShuckieCore};
    use std::time::{Duration, Instant};

    struct InboxPublisher(Arc<LinkInbox>);
    impl LinkPublisherFns for InboxPublisher {
        fn frame(&mut self, frame: u64, elapsed_millis: u64, events: Vec<Packet>, pair_hash: Option<(u64, [u8; 32])>) {
            self.0.push(frame, LinkFrame { events, elapsed_millis, pair_hash });
        }
        fn poll_errors(&mut self) -> Vec<String> {
            Vec::new()
        }
    }

    let a = ThreadedSuperShuckieCore::new(Box::new(gb(false)));
    let b = ThreadedSuperShuckieCore::new(Box::new(gb(false)));
    let speed = Speed::from_multiplier_float(8.0);
    a.set_speed(speed);
    b.set_speed(speed);
    a.start();
    b.start();
    std::thread::sleep(Duration::from_millis(300));

    let (a_frame, a_input) = a.link_hold().expect("hold a");
    let (b_frame, b_input) = b.link_hold().expect("hold b");
    assert!(a_frame > 0 && b_frame > 0);
    let a_state = a.create_save_state().expect("state a");
    let b_state = b.create_save_state().expect("state b");

    // Each machine's follower of the other, fed a snapshot at the held frame (what the stream
    // would have delivered).
    let make_follower = |of_state: Vec<u8>, frame: u64, input: &InputBuffer| {
        let mut follower = ThreadedSuperShuckieCore::new_with_role(Box::new(gb(false)), CoreThreadRole::Follower);
        let stats = Arc::new(FollowerStats::default());
        let (tx, _rx) = channel();
        let (feeder, source) = live_replay_channel(stats, tx);
        let metadata = ReplayFileMetadata {
            console_type: follower.console_type().unwrap(),
            rom_name: "linktest".into(),
            rom_filename: "linktest.gb".into(),
            rom_checksum: *follower.rom_checksum(),
            bios_checksum: [0; 32],
            emulator_core_name: follower.core_name().into(),
            patch_format: ReplayPatchFormat::Unpatched,
            patch_target_checksum: Default::default(),
            crop_start: None,
            crop_end: None,
            timer_offset: None
        };
        follower.attach_live_replay_source(source, metadata, true).expect("attach");
        feeder.push_packet(Packet::Keyframe {
            metadata: KeyframeMetadata { input: input.clone(), speed: Speed::default(), elapsed_frames: frame, elapsed_millis: 0.into(), counters: Vec::new() },
            state: ByteVec::Heap(of_state)
        });
        follower.start();
        (follower, feeder)
    };
    let (b_on_1, _feeder_b) = make_follower(b_state, b_frame, &b_input);
    let (a_on_2, _feeder_a) = make_follower(a_state, a_frame, &a_input);
    // Let the follower threads apply their snapshots.
    let started = Instant::now();
    while (b_on_1.follower_stats().map(|s| s.snapshots_applied).unwrap_or(0) == 0 || a_on_2.follower_stats().map(|s| s.snapshots_applied).unwrap_or(0) == 0) && started.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(b_on_1.follower_stats().unwrap().snapshots_applied, 1);
    assert_eq!(a_on_2.follower_stats().unwrap().snapshots_applied, 1);

    let inbox_1 = Arc::new(LinkInbox::new());
    let inbox_2 = Arc::new(LinkInbox::new());
    let lent_b = b_on_1.lend().expect("lend b");
    let lent_a = a_on_2.lend().expect("lend a");
    a.link(
        lent_b,
        LinkSettings { delay_frames: 2, local_is_first: true, local_start_frame: a_frame, partner_start_frame: b_frame, partner_start_input: b_input },
        inbox_1.clone(),
        Box::new(InboxPublisher(inbox_2.clone()))
    );
    b.link(
        lent_a,
        LinkSettings { delay_frames: 2, local_is_first: false, local_start_frame: b_frame, partner_start_frame: a_frame, partner_start_input: a_input },
        inbox_2.clone(),
        Box::new(InboxPublisher(inbox_1.clone()))
    );
    let started = Instant::now();
    loop {
        let (sa, sb) = (a.link_status(), b.link_status());
        if matches!(sa, LinkStatus::Linked { .. }) && matches!(sb, LinkStatus::Linked { .. }) {
            break
        }
        assert!(!matches!(sa, LinkStatus::Failed(_)) && !matches!(sb, LinkStatus::Failed(_)), "{sa:?} / {sb:?}");
        assert!(started.elapsed() < Duration::from_secs(10), "never linked: {sa:?} / {sb:?}");
        std::thread::sleep(Duration::from_millis(5));
    }

    // The lent followers' wrappers keep answering (through the primary threads).
    assert!(a.read_ram(test_rom::DONE_ADDRESS, 1).is_some());
    assert!(b_on_1.read_ram(test_rom::DONE_ADDRESS, 1).is_some());
    assert!(a_on_2.create_save_state().is_some());

    a.enqueue_write(test_rom::ROLE_ADDRESS, test_rom::role_write(ROLE_MASTER).to_vec());
    b.enqueue_write(test_rom::ROLE_ADDRESS, test_rom::role_write(ROLE_SLAVE).to_vec());
    let started = Instant::now();
    loop {
        let done = |c: &ThreadedSuperShuckieCore| c.read_ram(test_rom::DONE_ADDRESS, 1).map(|d| d[0] == DONE).unwrap_or(false);
        if done(&a) && done(&b) && done(&b_on_1) && done(&a_on_2) {
            break
        }
        let (sa, sb) = (a.link_status(), b.link_status());
        assert!(matches!(sa, LinkStatus::Linked { .. }) && matches!(sb, LinkStatus::Linked { .. }), "{sa:?} / {sb:?}");
        assert!(started.elapsed() < Duration::from_secs(30), "the transfer never finished over threads");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(a.read_ram(test_rom::RECEIVED_ADDRESS, 256).unwrap(), test_rom::expected_received(ROLE_MASTER));
    assert_eq!(b.read_ram(test_rom::RECEIVED_ADDRESS, 256).unwrap(), test_rom::expected_received(ROLE_SLAVE));
    assert_eq!(a_on_2.read_ram(test_rom::RECEIVED_ADDRESS, 256).unwrap(), test_rom::expected_received(ROLE_MASTER));
    assert_eq!(b_on_1.read_ram(test_rom::RECEIVED_ADDRESS, 256).unwrap(), test_rom::expected_received(ROLE_SLAVE));

    // A little longer, past a few pair hashes: still in step and moving (a momentary stall is
    // normal: the two machines run free here and take turns waiting for each other's frames).
    let frame_at = |status: &LinkStatus| match status { LinkStatus::Linked { link_frame, .. } => *link_frame, other => panic!("{other:?}") };
    let (before_a, before_b) = (frame_at(&a.link_status()), frame_at(&b.link_status()));
    std::thread::sleep(Duration::from_millis(1500));
    let (sa, sb) = (a.link_status(), b.link_status());
    assert!(frame_at(&sa) > before_a + 60 && frame_at(&sb) > before_b + 60, "{sa:?} / {sb:?}");
    assert!(a.get_link_errors().is_empty() && b.get_link_errors().is_empty());

    // Unplug: the followers go home to their threads and wait for a snapshot; everyone is alive.
    a.unlink();
    b.unlink();
    assert_eq!(a.link_status(), LinkStatus::Idle);
    assert_eq!(b.link_status(), LinkStatus::Idle);
    std::thread::sleep(Duration::from_millis(50));
    assert!(a.is_alive() && b.is_alive() && a_on_2.is_alive() && b_on_1.is_alive());
    assert!(b_on_1.follower_stats().unwrap().waiting);
    assert!(a_on_2.create_save_state().is_some(), "the follower answers from its own thread again");
    drop(a_on_2);
    drop(b_on_1);
    drop(a);
    drop(b);
}
