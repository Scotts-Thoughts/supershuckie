//! Play Together end to end, headless: two or three real frontends in one process (each with its
//! own core thread, publisher and followers) on the loopback interface, the way the app runs them
//! (paced, ticked from a UI loop, no window). One hosts, the rest join, everybody follows
//! everybody else for a while, sync pause and the host's start state are tried, then everyone
//! leaves and the followers' replay files are played back to their end.
//!
//! ```text
//! play_together_smoke <rom.gbc|rom.gba> [--players 2] [--speed 4] [--seconds 20] [--peer-rom <rom>] [--pokeabyte <port>] [--link]
//! play_together_smoke --link [--gba] [--players 2] [--speed 4] [--seconds 20]
//! ```
//!
//! `--peer-rom` gives the joiners a ROM of their own (the host then follows a different game
//! than it plays). `--pokeabyte <port>` has the host serve its own game to Poke-A-Byte on that
//! UDP port and every friend's game on the ports above it, the way the app does, and checks
//! that every follower got a port; the session is held for `--seconds`, long enough to point a
//! real Poke-A-Byte at `/instances/<port + 1>/` meanwhile. `--link` plugs a link cable between
//! the host and the first joiner after the measured stretch: the handshake, both games in
//! lockstep at 1x, the refusals, an unplug from the other side, a declined request, and (with
//! no ROM given) the link test ROM exchanging 256 bytes each way, checked on both machines'
//! copies of both games; the friend replay files then carry the serial input and still play
//! back to their end. `--gba` picks the Game Boy Advance test ROM (mGBA's lockstep) over the
//! Game Boy one.
//!
//! Exits non-zero when a frontend's own game ran below 90% of the requested speed (the local
//! game must stay smooth whatever the followers do), when a UI tick took longer than
//! `MAX_TICK` (the app ticks the frontend from its UI thread every millisecond, and that thread
//! is also the one that hands frames to the window: a tick that blocks on another thread is a
//! late frame), when a follower fell more than a second of frames behind or ever desynced, when
//! a follower's screen never arrived, when sync pause does not pause (or unpause) everyone, when
//! the host's start state does not land everyone on the same state (or a race start from it
//! does not restart everyone), or when a saved friend replay does not play back to its end.
//!
//! Link it like `supershuckie-core`'s `nds_bench` (see that file's header).

use std::collections::BTreeMap;
use std::num::NonZeroU8;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use supershuckie_core::emulator::ScreenData;
use supershuckie_core::link::test_rom;
use supershuckie_frontend::play_together::PeerId;
use supershuckie_frontend::{ScreenInfo, SuperShuckieFrontend, SuperShuckieFrontendCallbacks, SuperShuckieReplayState};
use supershuckie_replay_recorder::replay_file::playback::ReplayFilePlayer;

/// Counts the screens each peer delivered.
#[derive(Default)]
struct PeerScreens(Arc<Mutex<BTreeMap<PeerId, u64>>>);

impl SuperShuckieFrontendCallbacks for PeerScreens {
    fn refresh_screens(&mut self, _: &[ScreenData]) {}
    fn change_video_mode(&mut self, _: &[ScreenInfo], _: NonZeroU8) {}
    fn peer_refresh_screens(&mut self, peer: PeerId, _: &[ScreenData]) {
        *self.0.lock().unwrap().entry(peer).or_default() += 1;
    }
}

struct Player {
    name: String,
    frontend: SuperShuckieFrontend,
    screens: Arc<Mutex<BTreeMap<PeerId, u64>>>,
    /// Per followed peer: every frames-behind sample taken.
    behind: BTreeMap<PeerId, Vec<u64>>,
    fps_samples: Vec<f64>,
    /// How long each `tick()` took during the measured stretch.
    tick_times: Vec<Duration>,
}

/// The longest a UI tick may take while in a session (a 120 Hz display refreshes every 8.3 ms;
/// a tick that blocks for longer than this is a visibly late frame).
const MAX_TICK: Duration = Duration::from_millis(4);

fn tick_all(players: &mut [Player]) {
    for p in players.iter_mut() {
        let started = Instant::now();
        if let Err(e) = p.frontend.tick() {
            println!("[{}] tick error: {e}", p.name);
        }
        p.tick_times.push(started.elapsed());
    }
}

fn tick_all_for(players: &mut [Player], duration: Duration) {
    let until = Instant::now() + duration;
    while Instant::now() < until {
        tick_all(players);
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(windows)]
fn request_fine_timer_resolution() {
    #[link(name = "winmm")]
    unsafe extern "system" {
        fn timeBeginPeriod(period: u32) -> u32;
    }
    // SAFETY: plain Win32 call with a constant argument.
    unsafe {
        let _ = timeBeginPeriod(1);
    }
}

#[cfg(not(windows))]
fn request_fine_timer_resolution() {}

/// Tick until `cond` holds for every player, or `timeout` passes.
fn tick_until_all(players: &mut [Player], timeout: Duration, cond: impl Fn(&Player) -> bool) -> bool {
    let until = Instant::now() + timeout;
    while Instant::now() < until {
        tick_all(players);
        if players.iter().all(&cond) {
            return true
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    false
}

/// Tick everyone until `cond` holds for `players[index]`, or `timeout` passes.
fn tick_until_one(players: &mut [Player], index: usize, timeout: Duration, cond: impl Fn(&Player) -> bool) -> bool {
    let until = Instant::now() + timeout;
    while Instant::now() < until {
        tick_all(players);
        if cond(&players[index]) {
            return true
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    false
}

/// The sync-pause round: the setting travels from the host, a pause from one player pauses
/// everyone, an unpause from another unpauses everyone, and with the setting off a pause stays
/// local. Leaves everyone unpaused with the setting off. Returns whether it all held.
fn check_sync_pause(players: &mut [Player]) -> bool {
    let mut ok = true;
    let wait = Duration::from_secs(3);
    let paused_state = |p: &Player| (p.frontend.is_paused(), p.frontend.play_together_state().paused_by);

    if let Err(e) = players[1].frontend.set_play_together_sync_pause(true) {
        println!("  a client cannot set sync pause: {e}");
    }
    else {
        println!("FAIL: a client could set sync pause");
        ok = false;
    }
    players[0].frontend.set_play_together_sync_pause(true).expect("host sets sync pause");
    if !tick_until_all(players, wait, |p| p.frontend.get_play_together_sync_pause()) {
        println!("FAIL: the host's sync pause setting did not reach everyone: {:?}", players.iter().map(|p| p.frontend.get_play_together_sync_pause()).collect::<Vec<_>>());
        return false
    }
    println!("  sync pause on everywhere");

    // A client pauses: everyone pauses, and knows who did it.
    let started = Instant::now();
    players[1].frontend.set_paused(true);
    if tick_until_all(players, wait, |p| p.frontend.is_paused()) {
        println!("  {} paused everyone in {} ms", players[1].name, started.elapsed().as_millis());
        tick_all_for(players, Duration::from_millis(50));
        let by: Vec<String> = players.iter().map(|p| p.frontend.play_together_state().paused_by).collect();
        let expected_by: Vec<String> = players.iter().enumerate().map(|(i, _)| if i == 1 { String::from("you") } else { players[1].name.clone() }).collect();
        if by != expected_by {
            println!("FAIL: paused_by is {by:?}, expected {expected_by:?}");
            ok = false;
        }
    }
    else {
        println!("FAIL: {}'s pause did not pause everyone: {:?}", players[1].name, players.iter().map(paused_state).collect::<Vec<_>>());
        ok = false;
    }

    // Another player unpauses: everyone unpauses.
    let started = Instant::now();
    let unpauser = players.len() - 1;
    players[unpauser].frontend.set_paused(false);
    if tick_until_all(players, wait, |p| !p.frontend.is_paused()) {
        println!("  {} unpaused everyone in {} ms", players[unpauser].name, started.elapsed().as_millis());
    }
    else {
        println!("FAIL: {}'s unpause did not unpause everyone: {:?}", players[unpauser].name, players.iter().map(paused_state).collect::<Vec<_>>());
        ok = false;
    }
    // Nothing keeps toggling afterwards (no echo between participants).
    tick_all_for(players, Duration::from_millis(300));
    if players.iter().any(|p| p.frontend.is_paused()) {
        println!("FAIL: someone paused again on their own: {:?}", players.iter().map(paused_state).collect::<Vec<_>>());
        ok = false;
    }

    // The host pauses, then turns the setting off while paused: everyone stays paused (the
    // setting going off changes nobody's state), and a pause is now local again.
    players[0].frontend.set_paused(true);
    if !tick_until_all(players, wait, |p| p.frontend.is_paused()) {
        println!("FAIL: the host's pause did not pause everyone: {:?}", players.iter().map(paused_state).collect::<Vec<_>>());
        ok = false;
    }
    players[0].frontend.set_play_together_sync_pause(false).expect("host clears sync pause");
    if !tick_until_all(players, wait, |p| !p.frontend.get_play_together_sync_pause()) {
        println!("FAIL: turning sync pause off did not reach everyone");
        ok = false;
    }
    for p in players.iter_mut() {
        p.frontend.set_paused(false);
    }
    tick_all_for(players, Duration::from_millis(100));
    players[1].frontend.set_paused(true);
    tick_all_for(players, Duration::from_millis(300));
    let paused: Vec<bool> = players.iter().map(|p| p.frontend.is_paused()).collect();
    if paused.iter().enumerate().any(|(i, &paused)| paused != (i == 1)) {
        println!("FAIL: with sync pause off, {}'s pause was not local: {paused:?}", players[1].name);
        ok = false;
    }
    else {
        println!("  sync pause off: a pause stays local");
    }
    players[1].frontend.set_paused(false);
    tick_all_for(players, Duration::from_millis(100));
    ok
}

/// The start-state round: the host sets one (everyone pauses on the very same state), a race
/// start restarts everyone from it, then it is cleared. Leaves everyone unpaused with no start
/// state. Returns whether it all held.
fn check_start_state(players: &mut [Player], mixed_roms: bool) -> bool {
    let mut ok = true;
    let wait = Duration::from_secs(3);

    if players[1].frontend.set_play_together_start_state(true).is_ok() {
        println!("FAIL: a client could set the start state");
        ok = false;
    }
    if mixed_roms {
        // The joiners play another ROM: they could not load the host's state, so it is refused.
        return match players[0].frontend.set_play_together_start_state(true) {
            Err(e) => {
                println!("  the start state is refused with mixed ROMs: {}", e.as_str().lines().last().unwrap_or_default());
                ok
            }
            Ok(()) => {
                println!("FAIL: the host could set a start state although the others play another ROM");
                false
            }
        }
    }
    let started = Instant::now();
    if let Err(e) = players[0].frontend.set_play_together_start_state(true) {
        println!("FAIL: the host could not set the start state: {e}");
        return false
    }
    if !players[0].frontend.is_paused() {
        println!("FAIL: setting the start state did not pause the host");
        ok = false;
    }
    if !tick_until_all(players, wait, |p| p.frontend.is_paused() && p.frontend.play_together_state().start_state) {
        println!(
            "FAIL: the start state did not reach everyone: {:?}",
            players.iter().map(|p| (p.frontend.is_paused(), p.frontend.play_together_state().start_state)).collect::<Vec<_>>()
        );
        return false
    }
    println!("  the start state reached everyone (paused) in {} ms", started.elapsed().as_millis());
    // Give every core a moment to apply the load, then compare what each game is at.
    tick_all_for(players, Duration::from_millis(300));
    let hashes: Vec<String> = players.iter().map(|p| {
        let state = p.frontend.create_save_state_bytes().unwrap_or_default();
        supershuckie_replay_recorder::replay_file::blake3_hash_to_ascii(supershuckie_replay_recorder::blake3_hash(&state))[..12].to_owned()
    }).collect();
    if hashes.iter().any(|h| *h != hashes[0]) {
        println!("FAIL: the games are not on the same state after the start state: {hashes:?}");
        ok = false;
    }
    else {
        println!("  every game is on the same state ({}…)", hashes[0]);
    }

    // A race start restarts everyone from it (and unpauses everyone).
    players[0].frontend.play_together_reset_all(1).expect("reset all");
    let started = Instant::now();
    if tick_until_all(players, Duration::from_millis(2500), |p| !p.frontend.is_paused()) {
        println!("  the race start from the start state unpaused everyone after {} ms", started.elapsed().as_millis());
    }
    else {
        println!("FAIL: the race start from the start state did not unpause everyone: {:?}", players.iter().map(|p| p.frontend.is_paused()).collect::<Vec<_>>());
        ok = false;
    }
    tick_all_for(players, Duration::from_millis(300));

    // Clearing it reaches everyone and changes nobody's game.
    players[0].frontend.set_play_together_start_state(false).expect("host clears the start state");
    if !tick_until_all(players, wait, |p| !p.frontend.play_together_state().start_state) {
        println!("FAIL: clearing the start state did not reach everyone");
        ok = false;
    }
    if players.iter().any(|p| p.frontend.is_paused()) {
        println!("FAIL: clearing the start state paused someone");
        ok = false;
    }
    ok
}

/// The link cable round between the host and the first joiner. Returns whether it all held.
/// With `test_rom`, the two games are the link test ROM: the host's is told to be the master
/// and the joiner's the slave, and the 256 bytes each side receives are checked on both
/// machines' copies of both games.
fn check_link(all: &mut [Player], speed: f64, test_rom: Option<TestRom>) -> bool {
    let mut ok = true;
    let wait = Duration::from_secs(15);
    // Only the host and the first joiner link; anyone else keeps following both of them.
    let (players, watchers) = all.split_at_mut(2);
    let phase = |p: &Player| p.frontend.play_together_link_state().phase.to_owned();
    let host_id = players[0].frontend.play_together_state().local_peer_id;
    let ash_id = players[1].frontend.play_together_state().local_peer_id;
    let ash_name = players[1].name.clone();

    // The joiner's game must be linkable from the host's point of view (followed, in sync, same
    // console family), and vice versa.
    for (i, other) in [(0usize, ash_id), (1usize, host_id)] {
        let state = players[i].frontend.play_together_state();
        let Some(q) = state.participants.iter().find(|q| q.peer_id == other) else {
            println!("FAIL: [{}] does not see peer {other}", players[i].name);
            return false
        };
        if !q.can_link {
            println!("FAIL: [{}] cannot link with {} ({}: {})", players[i].name, q.name, q.status, q.status_text);
            return false
        }
    }

    // A request the other player declines.
    let started = Instant::now();
    if let Err(e) = players[1].frontend.play_together_link_request(host_id) {
        println!("FAIL: {ash_name} could not request a link: {e}");
        return false
    }
    if !tick_until_one(players, 0, wait, |p| phase(p) == "incoming") {
        println!("FAIL: the host never saw {ash_name}'s link request: {}", phase(&players[0]));
        return false
    }
    let incoming = players[0].frontend.play_together_link_state();
    if incoming.peer_id != ash_id || incoming.peer_name != ash_name {
        println!("FAIL: the host's incoming request is from {} ({}), expected {ash_name} ({ash_id})", incoming.peer_name, incoming.peer_id);
        ok = false;
    }
    players[0].frontend.play_together_link_respond(incoming.nonce, false).expect("decline");
    if !tick_until_all(players, wait, |p| phase(p) == "none") {
        println!("FAIL: the declined request did not clear: {:?}", players.iter().map(phase).collect::<Vec<_>>());
        return false
    }
    tick_all(watchers);
    let reason = players[1].frontend.play_together_link_state().last_reason;
    if !reason.contains("declined") {
        println!("FAIL: {ash_name} was not told the request was declined: {reason:?}");
        ok = false;
    }
    else {
        println!("  a declined request comes back as such in {} ms: {reason:?}", started.elapsed().as_millis());
    }

    // The host asks, the joiner accepts: both hold, exchange their start frames and plug in.
    let started = Instant::now();
    if let Err(e) = players[0].frontend.play_together_link_request(ash_id) {
        println!("FAIL: the host could not request a link: {e}");
        return false
    }
    if phase(&players[0]) != "requesting" {
        println!("FAIL: the host is not requesting after asking: {}", phase(&players[0]));
        ok = false;
    }
    if !tick_until_one(players, 1, wait, |p| phase(p) == "incoming") {
        println!("FAIL: {ash_name} never saw the host's link request: {}", phase(&players[1]));
        return false
    }
    // A request while one is pending is refused on both sides.
    if players[1].frontend.play_together_link_request(host_id).is_ok() {
        println!("FAIL: {ash_name} could request a link while answering one");
        ok = false;
    }
    let nonce = players[1].frontend.play_together_link_state().nonce;
    if let Err(e) = players[1].frontend.play_together_link_respond(nonce, true) {
        println!("FAIL: {ash_name} could not accept: {e}");
        return false
    }
    if !tick_until_all(players, wait, |p| phase(p) == "linked") {
        println!("FAIL: not linked after {} ms: {:?}", wait.as_millis(), players.iter().map(|p| p.frontend.play_together_link_state()).collect::<Vec<_>>());
        tick_all(watchers);
        for p in players.iter() {
            println!("  [{}] errors: {:?}", p.name, p.frontend.play_together_state().errors);
        }
        return false
    }
    let views: Vec<_> = players.iter().map(|p| p.frontend.play_together_link_state()).collect();
    println!("  linked in {} ms: {} frames of input delay ({} sees {}, {} sees {})", started.elapsed().as_millis(), views[0].input_delay, players[0].name, views[0].peer_name, players[1].name, views[1].peer_name);
    if views[0].input_delay != views[1].input_delay || views[0].input_delay == 0 {
        println!("FAIL: the two sides disagree on the input delay: {} vs {}", views[0].input_delay, views[1].input_delay);
        ok = false;
    }
    if views[0].peer_id != ash_id || views[1].peer_id != host_id {
        println!("FAIL: the link peers are {} and {}, expected {ash_id} and {host_id}", views[0].peer_id, views[1].peer_id);
        ok = false;
    }
    // The roster says so too, to everyone (the watchers included).
    tick_all_for(players, Duration::from_millis(200));
    tick_all_for(watchers, Duration::from_millis(50));
    for p in players.iter().chain(watchers.iter()) {
        let state = p.frontend.play_together_state();
        let linked: Vec<(PeerId, Option<PeerId>)> = state.participants.iter().map(|q| (q.peer_id, q.linked_with)).collect();
        for (peer, with) in &linked {
            let expected = if *peer == host_id { Some(ash_id) } else if *peer == ash_id { Some(host_id) } else { None };
            if *with != expected {
                println!("FAIL: [{}] sees peer {peer} linked with {with:?}, expected {expected:?}", p.name);
                ok = false;
            }
        }
    }

    // While linked: refusals, and both games at 1x whatever the speed setting.
    match players[0].frontend.load_save_state_if_exists("anything") {
        Err(e) if e.as_str().contains("link cable") => println!("  loading a save state is refused while linked: {e}"),
        other => {
            println!("FAIL: loading a save state while linked: {other:?}");
            ok = false;
        }
    }
    if players[0].frontend.reload_core().is_ok() {
        println!("FAIL: the core could be reloaded while linked");
        ok = false;
    }
    if players[1].frontend.play_together_link_request(host_id).is_ok() {
        println!("FAIL: a second cable could be plugged in");
        ok = false;
    }

    // The test ROM: hand out the roles (a RAM write, scheduled like an input) and wait for the
    // 256 bytes to go through both ways.
    if let Some(test_rom) = test_rom {
        let (master, slave) = (0usize, 1usize);
        let (role_address, done_address, received_address) = test_rom.addresses();
        players[master].frontend.memory_tools_mut().1.enqueue_write(role_address, test_rom::role_write(test_rom::ROLE_MASTER).to_vec());
        players[slave].frontend.memory_tools_mut().1.enqueue_write(role_address, test_rom::role_write(test_rom::ROLE_SLAVE).to_vec());
        let started = Instant::now();
        let mut finished = false;
        let until = Instant::now() + Duration::from_secs(30);
        while Instant::now() < until {
            tick_all(players);
            tick_all(watchers);
            let own: Vec<bool> = players.iter_mut().map(|p| read_own(p, done_address, 1) == vec![test_rom::DONE]).collect();
            if own.iter().all(|d| *d) {
                finished = true;
                break
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        if !finished {
            let done: Vec<Vec<u8>> = players.iter_mut().map(|p| read_own(p, done_address, 1)).collect();
            println!("FAIL: the test ROMs did not finish exchanging 256 bytes in 30 s: {done:?}; link {:?}", players.iter().map(|p| p.frontend.play_together_link_state()).collect::<Vec<_>>());
            ok = false;
        }
        else {
            println!("  256 bytes exchanged each way in {} ms", started.elapsed().as_millis());
            // Every copy of every game agrees: each machine's own game and its copy of the other's.
            let expect_master = test_rom::expected_received(test_rom::ROLE_MASTER);
            let expect_slave = test_rom::expected_received(test_rom::ROLE_SLAVE);
            let own_master = read_own(&mut players[master], received_address, 256);
            let own_slave = read_own(&mut players[slave], received_address, 256);
            let checks: [(&str, Vec<u8>, &Vec<u8>); 4] = [
                ("the host's own game", own_master, &expect_master),
                ("the joiner's own game", own_slave, &expect_slave),
                ("the host's copy of the joiner's game", players[master].frontend.play_together_peer_read_ram(ash_id, received_address, 256).unwrap_or_default(), &expect_slave),
                ("the joiner's copy of the host's game", players[slave].frontend.play_together_peer_read_ram(host_id, received_address, 256).unwrap_or_default(), &expect_master)
            ];
            for (what, got, expected) in checks {
                if got != *expected {
                    println!("FAIL: {what} received {:02X?}… (expected {:02X?}…)", &got[..got.len().min(8)], &expected[..8]);
                    ok = false;
                }
            }
            if ok {
                println!("  both machines' copies of both games hold the right bytes");
            }
        }
    }

    // A stretch of lockstep: the link frames advance on both sides, nobody desyncs, and both
    // games run at 1x (the speed setting is set aside while linked).
    let before: Vec<u64> = players.iter().map(|p| p.frontend.play_together_link_state().link_frame).collect();
    let started = Instant::now();
    let mut fps: Vec<Vec<f64>> = vec![Vec::new(); players.len()];
    let mut since_fps = Instant::now();
    while started.elapsed() < Duration::from_secs(4) {
        tick_all(players);
        tick_all(watchers);
        if since_fps.elapsed() >= Duration::from_secs(1) {
            since_fps = Instant::now();
            for (i, p) in players.iter_mut().enumerate() {
                let f = p.frontend.get_emulation_fps();
                if f > 0.0 {
                    fps[i].push(f);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    for (i, p) in players.iter().enumerate() {
        let view = p.frontend.play_together_link_state();
        let avg = if fps[i].is_empty() { 0.0 } else { fps[i].iter().sum::<f64>() / fps[i].len() as f64 };
        println!("  [{}] linked: {} link frames in 4 s, {avg:.1} fps, stalled: {}", p.name, view.link_frame.saturating_sub(before[i]), view.stalled);
        if view.phase != "linked" {
            println!("FAIL: [{}] is no longer linked: {} ({})", p.name, view.phase, view.last_reason);
            ok = false;
        }
        if view.link_frame.saturating_sub(before[i]) < 150 {
            println!("FAIL: [{}] ran only {} link frames in 4 s", p.name, view.link_frame.saturating_sub(before[i]));
            ok = false;
        }
        if speed >= 2.0 && avg > 90.0 {
            println!("FAIL: [{}] ran at {avg:.1} fps while linked; linked games run at 1x", p.name);
            ok = false;
        }
        if avg < 50.0 {
            println!("FAIL: [{}] ran at {avg:.1} fps while linked", p.name);
            ok = false;
        }
        for q in p.frontend.play_together_state().participants {
            if q.hash_mismatches > 0 {
                println!("FAIL: [{}] desynced from {} while linked", p.name, q.name);
                ok = false;
            }
        }
    }

    // Pausing one side stalls the other.
    players[0].frontend.set_paused(true);
    if tick_until_one(players, 1, Duration::from_secs(3), |p| p.frontend.play_together_link_state().stalled) {
        println!("  the host pausing stalls {ash_name}");
    }
    else {
        println!("FAIL: the host pausing did not stall {ash_name}");
        ok = false;
    }
    players[0].frontend.set_paused(false);
    if !tick_until_all(players, Duration::from_secs(3), |p| !p.frontend.play_together_link_state().stalled) {
        println!("FAIL: the pair did not resume after the host unpaused");
        ok = false;
    }

    // The joiner unplugs: both sides go back to following, at the set speed.
    let started = Instant::now();
    players[1].frontend.play_together_unlink();
    tick_all(watchers);
    if !tick_until_all(players, wait, |p| phase(p) == "none") {
        println!("FAIL: the cable did not come out on both sides: {:?}", players.iter().map(phase).collect::<Vec<_>>());
        return false
    }
    let reason = players[0].frontend.play_together_link_state().last_reason;
    println!("  {ash_name} unplugged after {} ms; the host was told: {reason:?}", started.elapsed().as_millis());
    if !reason.contains(&ash_name) {
        println!("FAIL: the host's reason does not name {ash_name}: {reason:?}");
        ok = false;
    }
    if !tick_until_all(all, Duration::from_secs(15), |p| {
        p.frontend.play_together_state().participants.iter().all(|q| q.status == "following" || q.status == "waiting")
    }) {
        let players = &mut *all;
        println!("FAIL: not everyone is following again after the unplug: {:?}", players.iter().map(|p| p.frontend.play_together_state().participants.iter().map(|q| format!("{}:{}", q.name, q.status)).collect::<Vec<_>>()).collect::<Vec<_>>());
        ok = false;
    }
    else {
        println!("  everyone is following again after the unplug");
    }
    let players = &mut *all;
    // The fps window is a second long and only turns over when polled: poll along the way and
    // keep the last full second's reading.
    let mut fps = vec![0.0; players.len()];
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(3500) {
        tick_all(players);
        for (i, p) in players.iter_mut().enumerate() {
            fps[i] = p.frontend.get_emulation_fps();
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    if speed >= 2.0 && fps.iter().any(|f| *f < speed * 60.0 * 0.8) {
        println!("FAIL: the speed setting did not come back after the unplug: {fps:?} fps");
        ok = false;
    }
    else {
        println!("  speed is back: {:?} fps", fps.iter().map(|f| f.round()).collect::<Vec<_>>());
    }
    ok
}

/// Which link test ROM the players run.
#[derive(Copy, Clone, PartialEq, Eq)]
enum TestRom {
    GameBoy,
    GameBoyAdvance
}

impl TestRom {
    /// The role, done and received addresses of the ROM's RAM.
    fn addresses(self) -> (u32, u32, u32) {
        match self {
            TestRom::GameBoy => (test_rom::ROLE_ADDRESS, test_rom::DONE_ADDRESS, test_rom::RECEIVED_ADDRESS),
            TestRom::GameBoyAdvance => (test_rom::GBA_ROLE_ADDRESS, test_rom::GBA_DONE_ADDRESS, test_rom::GBA_RECEIVED_ADDRESS)
        }
    }
}

/// Plug the cable back in between the host and the first joiner (the joiner accepts). Returns
/// whether it went in.
fn relink(all: &mut [Player]) -> bool {
    let phase = |p: &Player| p.frontend.play_together_link_state().phase.to_owned();
    let host_id = all[0].frontend.play_together_state().local_peer_id;
    let ash_id = all[1].frontend.play_together_state().local_peer_id;
    if let Err(e) = all[0].frontend.play_together_link_request(ash_id) {
        println!("FAIL: the host could not request a second link: {e}");
        return false
    }
    if !tick_until_one(all, 1, Duration::from_secs(15), |p| phase(p) == "incoming") {
        println!("FAIL: the second link request never arrived");
        return false
    }
    let nonce = all[1].frontend.play_together_link_state().nonce;
    if let Err(e) = all[1].frontend.play_together_link_respond(nonce, true) {
        println!("FAIL: could not accept the second link: {e}");
        return false
    }
    let until = Instant::now() + Duration::from_secs(15);
    while Instant::now() < until {
        tick_all(all);
        if all[..2].iter().all(|p| phase(p) == "linked") {
            let view = all[0].frontend.play_together_link_state();
            println!("  linked again ({} sees {}, {} frames delay): the race start and the leave happen with the cable in", all[0].name, view.peer_name, view.input_delay);
            return true
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    println!("FAIL: not linked again after 15 s: {:?}", all[..2].iter().map(|p| p.frontend.play_together_link_state()).collect::<Vec<_>>());
    let _ = host_id;
    false
}

/// `len` bytes at `address` of a player's own game.
fn read_own(p: &mut Player, address: u32, len: usize) -> Vec<u8> {
    p.frontend.memory_tools_mut().1.read_ram(address, len).unwrap_or_default()
}

fn make_player(dir: &Path, name: &str, rom: &Path, speed: f64, pokeabyte_port: Option<u16>) -> Player {
    let user = dir.join(name);
    let _ = std::fs::remove_dir_all(&user);
    std::fs::create_dir_all(&user).unwrap();
    // Several frontends in one process: the REST server and Poke-A-Byte bind fixed ports, so
    // only the settings can keep them apart (both default on). With --pokeabyte, the host alone
    // serves its game (and, above that port, every friend's).
    let pokeabyte = match pokeabyte_port {
        Some(port) => format!(r#"{{"enabled": true, "port": {port}, "serve_friends": true}}"#),
        None => String::from(r#"{"enabled": false}"#)
    };
    std::fs::write(
        user.join("settings.json"),
        r#"{"pokeabyte": POKEABYTE, "external_commands": {"enabled": false}, "play_together": {"display_name": "NAME", "bind_address": "127.0.0.1"}}"#.replace("NAME", name).replace("POKEABYTE", &pokeabyte)
    ).unwrap();
    let screens = Arc::new(Mutex::new(BTreeMap::new()));
    let mut frontend = SuperShuckieFrontend::new(user.clone(), user.clone(), Box::new(PeerScreens(screens.clone())));
    frontend.set_speed_settings(speed, 2.0);
    frontend.set_auto_pause_on_record_setting(false);
    frontend.load_rom(rom).expect("load rom");
    frontend.set_paused(false);
    Player { name: name.to_owned(), frontend, screens, behind: BTreeMap::new(), fps_samples: Vec::new(), tick_times: Vec::new() }
}

fn main() {
    let mut args = std::env::args().skip(1).peekable();
    let mut rom: Option<PathBuf> = match args.peek() {
        Some(first) if !first.starts_with("--") => Some(std::path::absolute(args.next().unwrap()).unwrap()),
        _ => None
    };
    let mut players_wanted = 2usize;
    let mut speed = 4.0f64;
    let mut seconds = 20.0f64;
    let mut peer_rom: Option<PathBuf> = None;
    let mut pokeabyte_port: Option<u16> = None;
    let mut link = false;
    let mut gba = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--players" => players_wanted = args.next().unwrap().parse().unwrap(),
            "--speed" => speed = args.next().unwrap().parse().unwrap(),
            "--seconds" => seconds = args.next().unwrap().parse().unwrap(),
            "--peer-rom" => peer_rom = Some(std::path::absolute(args.next().unwrap()).unwrap()),
            "--pokeabyte" => pokeabyte_port = Some(args.next().unwrap().parse().unwrap()),
            "--link" => link = true,
            "--gba" => gba = true,
            other => panic!("unexpected {other}"),
        }
    }
    assert!((2..=8).contains(&players_wanted), "2 to 8 players");
    request_fine_timer_resolution();

    let dir = std::env::temp_dir().join("supershuckie-play-together-smoke");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // With --link and no ROM, the link test ROM (256 bytes each way over the cable) is the game.
    let link_test_rom = if rom.is_none() && link { Some(if gba { TestRom::GameBoyAdvance } else { TestRom::GameBoy }) } else { None };
    if rom.is_none() {
        assert!(link, "usage: play_together_smoke <rom> [--players n] [--speed n] [--seconds n] [--peer-rom rom] [--pokeabyte port] [--link], or --link [--gba] alone");
        let path = if gba { dir.join("linktest.gba") } else { dir.join("linktest.gbc") };
        std::fs::write(&path, if gba { test_rom::build_gba() } else { test_rom::build(true) }).unwrap();
        rom = Some(path);
    }
    let rom = rom.unwrap();
    if link && peer_rom.is_some() {
        panic!("--link needs both players on the same ROM");
    }

    let names = ["Host", "Ash", "Misty", "Brock", "Gary", "Red", "Blue", "May"];
    let mut players: Vec<Player> = names.iter().take(players_wanted).enumerate().map(|(i, n)| {
        let rom = if i == 0 { &rom } else { peer_rom.as_ref().unwrap_or(&rom) };
        make_player(&dir, n, rom, speed, if i == 0 { pokeabyte_port } else { None })
    }).collect();
    println!("{} at {speed}x, {players_wanted} players, {seconds} s", rom.display());
    if let Some(peer_rom) = &peer_rom {
        println!("joiners play {}", peer_rom.display());
        // Everyone must be able to find the others' ROM by hash: it is not among their recent ROMs.
        players[0].frontend.play_together_add_rom_candidates(vec![peer_rom.clone()]);
        for p in players.iter_mut().skip(1) {
            p.frontend.play_together_add_rom_candidates(vec![rom.clone()]);
        }
    }
    if let Some(port) = pokeabyte_port {
        println!("the host serves its game to Poke-A-Byte on UDP {port} and friends' games above it");
    }

    // Past the boot sequence.
    tick_all_for(&mut players, Duration::from_secs(3));

    let mut failed = false;

    // Host on a free port (found by binding one and letting it go), then everyone else joins.
    let free_port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let code = match players[0].frontend.play_together_host(free_port, "Host", 0) {
        Ok(code) => code.to_string(),
        Err(e) => {
            println!("FAIL: host: {e}");
            std::process::exit(1);
        }
    };
    let port = code.rsplit(':').next().unwrap();
    let join_code = format!("127.0.0.1:{port}");
    println!("hosting at {code}; joining with {join_code}");
    tick_all_for(&mut players, Duration::from_millis(200));
    for p in players.iter_mut().skip(1) {
        let name = p.name.clone();
        if let Err(e) = p.frontend.play_together_join(&join_code, &name, 0) {
            println!("FAIL: {name} could not join: {e}");
            std::process::exit(1);
        }
    }

    // Everyone should end up following everyone else.
    let started = Instant::now();
    loop {
        tick_all(&mut players);
        let all_following = players.iter().all(|p| {
            let state = p.frontend.play_together_state();
            state.participants.len() == players_wanted - 1 && state.participants.iter().all(|q| q.status == "following" || q.status == "waiting")
        });
        if all_following {
            break;
        }
        if started.elapsed() > Duration::from_secs(30) {
            println!("FAIL: not everyone is following after 30 s:");
            for p in &players {
                let state = p.frontend.play_together_state();
                println!("  [{}] role={} participants={:?} errors={:?}", p.name, state.role, state.participants.iter().map(|q| format!("{}:{}:{}", q.name, q.status, q.status_text)).collect::<Vec<_>>(), state.errors);
            }
            std::process::exit(1);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    println!("everyone is following after {:.1} s", started.elapsed().as_secs_f64());
    for p in &players {
        let state = p.frontend.play_together_state();
        println!("  [{}] {} as {} (peer {}, {}), following {}", p.name, state.role, state.local_name, state.local_peer_id, state.local_color_name, state.participants.iter().map(|q| format!("{} ({}, {})", q.name, q.color_name, q.replay_file.clone().unwrap_or_else(|| "no file".into()))).collect::<Vec<_>>().join(", "));
    }
    // Every player has a colour of their own, and everyone agrees on who has which.
    {
        let mut by_peer: std::collections::BTreeMap<u16, u8> = std::collections::BTreeMap::new();
        for p in &players {
            let state = p.frontend.play_together_state();
            for (peer, color) in std::iter::once((state.local_peer_id, state.local_color)).chain(state.participants.iter().map(|q| (q.peer_id, q.color))) {
                if color == 0 {
                    println!("FAIL: [{}] sees peer {peer} without a colour", p.name);
                    failed = true;
                }
                if let Some(seen) = by_peer.insert(peer, color) {
                    if seen != color {
                        println!("FAIL: [{}] sees peer {peer} as colour {color}, someone else saw {seen}", p.name);
                        failed = true;
                    }
                }
            }
        }
        let mut colors: Vec<u8> = by_peer.values().copied().collect();
        colors.sort_unstable();
        colors.dedup();
        if colors.len() != by_peer.len() {
            println!("FAIL: colours are not unique: {by_peer:?}");
            failed = true;
        }
    }

    // With --pokeabyte, every friend's game the host follows is served on a port of its own,
    // the lowest free ones above the host's, and the REST state says which.
    if let Some(base) = pokeabyte_port {
        let state = players[0].frontend.play_together_state();
        let mut ports: Vec<u16> = state.participants.iter().filter_map(|q| q.pokeabyte_port).collect();
        for q in &state.participants {
            match q.pokeabyte_port {
                Some(port) => println!("  [Host] serves {}'s game to Poke-A-Byte on UDP {port} (Poke-A-Byte: /instances/{port}/)", q.name),
                None => {
                    println!("FAIL: [Host] does not serve {}'s game to Poke-A-Byte ({:?})", q.name, state.errors);
                    failed = true;
                }
            }
        }
        ports.sort();
        ports.dedup();
        if ports.len() != state.participants.len() {
            println!("FAIL: [Host] two friends share a Poke-A-Byte port: {ports:?}");
            failed = true;
        }
        if ports.iter().any(|&p| p <= base || p > base + players_wanted as u16) {
            println!("FAIL: [Host] friends' ports {ports:?} are not the lowest ones above {base}");
            failed = true;
        }
        if players[0].frontend.get_pokeabyte_port() != base || players[0].frontend.is_pokeabyte_enabled() != Ok(true) {
            println!("FAIL: [Host] its own game is not served on {base}: {:?}", players[0].frontend.is_pokeabyte_enabled());
            failed = true;
        }
        // Each server answers a Poke-A-Protocol PING on its own port.
        let client = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        for port in std::iter::once(base).chain(ports.iter().copied()) {
            let mut ping = [0u8; 32];
            ping[0] = 1; // protocol version
            ping[4] = 1; // PING
            client.send_to(&ping, ("127.0.0.1", port)).unwrap();
            let mut reply = [0u8; 64];
            match client.recv_from(&mut reply) {
                Ok((len, from)) if len >= 6 && from.port() == port && reply[4] == 1 && reply[5] == 1 => println!("  UDP {port} answers PING"),
                other => {
                    println!("FAIL: no PING reply from UDP {port}: {other:?}");
                    failed = true;
                }
            }
        }
    }

    // Measure.
    for p in players.iter_mut() {
        p.tick_times.clear();
    }
    let started = Instant::now();
    let mut since_sample = Instant::now();
    let mut since_fps = Instant::now();
    while started.elapsed().as_secs_f64() < seconds {
        tick_all(&mut players);
        if since_sample.elapsed() >= Duration::from_millis(100) {
            since_sample = Instant::now();
            for p in players.iter_mut() {
                for q in p.frontend.play_together_state().participants {
                    p.behind.entry(q.peer_id).or_default().push(q.frames_behind);
                }
            }
        }
        if since_fps.elapsed() >= Duration::from_secs(1) {
            since_fps = Instant::now();
            for p in players.iter_mut() {
                let fps = p.frontend.get_emulation_fps();
                if fps > 0.0 {
                    p.fps_samples.push(fps);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    // Report.
    let target = speed * 60.0;
    for p in &players {
        let avg = if p.fps_samples.is_empty() { 0.0 } else { p.fps_samples.iter().sum::<f64>() / p.fps_samples.len() as f64 };
        let min = p.fps_samples.iter().cloned().fold(f64::INFINITY, f64::min);
        println!("[{}] own game: {avg:.1} fps average, {min:.1} fps worst second (target {target:.0})", p.name);
        if avg < target * 0.9 {
            println!("FAIL: [{}] own game ran below 90% of the target", p.name);
            failed = true;
        }
        let mut ticks = p.tick_times.clone();
        ticks.sort();
        if !ticks.is_empty() {
            let total: Duration = ticks.iter().sum();
            let p99 = ticks[(ticks.len() - 1) * 99 / 100];
            let max = ticks[ticks.len() - 1];
            let slow = ticks.iter().filter(|t| **t > MAX_TICK).count();
            println!(
                "[{}] UI ticks: {} ticks, {:.3} ms average, {:.3} ms p99, {:.3} ms max, {slow} over {} ms",
                p.name, ticks.len(), total.as_secs_f64() * 1000.0 / ticks.len() as f64, p99.as_secs_f64() * 1000.0, max.as_secs_f64() * 1000.0, MAX_TICK.as_millis()
            );
            if p99 > MAX_TICK {
                println!("FAIL: [{}] UI ticks block for too long (the frontend waits on another thread)", p.name);
                failed = true;
            }
        }
        let state = p.frontend.play_together_state();
        for q in &state.participants {
            let samples = p.behind.get(&q.peer_id).cloned().unwrap_or_default();
            let mut sorted = samples.clone();
            sorted.sort();
            let median = sorted.get(sorted.len() / 2).copied().unwrap_or(0);
            let max = sorted.last().copied().unwrap_or(0);
            let screens = p.screens.lock().unwrap().get(&q.peer_id).copied().unwrap_or(0);
            println!(
                "[{}] following {}: status {} · behind median {median} max {max} frames · {} snapshots · {} desyncs · {:.0} fps · {screens} screens",
                p.name, q.name, q.status, q.snapshots_applied, q.hash_mismatches, q.fps
            );
            if max > (speed * 60.0) as u64 {
                println!("FAIL: [{}] fell more than a second behind {}", p.name, q.name);
                failed = true;
            }
            if median > 10 {
                println!("FAIL: [{}] is usually more than 10 frames behind {}", p.name, q.name);
                failed = true;
            }
            if q.hash_mismatches > 0 {
                println!("FAIL: [{}] desynced from {}", p.name, q.name);
                failed = true;
            }
            if screens == 0 {
                println!("FAIL: [{}] never received {}'s screen", p.name, q.name);
                failed = true;
            }
        }
        if !state.errors.is_empty() {
            println!("[{}] errors: {:?}", p.name, state.errors);
        }
    }

    // Sync pause: the host's setting reaches everyone; then one player's pause pauses everyone,
    // another's unpause unpauses everyone, and with the setting off a pause stays local.
    failed |= !check_sync_pause(&mut players);

    // Start state: the host's save state lands everyone on the same state, paused; a race start
    // restarts everyone from it; clearing it reaches everyone.
    failed |= !check_start_state(&mut players, peer_rom.is_some());

    // The link cable between the host and the first joiner.
    if link {
        println!("link cable:");
        failed |= !check_link(&mut players, speed, link_test_rom);
        // Plugged in again for the rest: the race start below resets both linked games through
        // the cable, and everyone leaves with it still in.
        failed |= !relink(&mut players);
    }

    // A race start from the host: everyone resets (which shows up as a ResetConsole packet in
    // every friend's replay file, checked below).
    players[0].frontend.play_together_reset_all(1).expect("reset all");
    let started = Instant::now();
    let mut counted_down = false;
    while started.elapsed() < Duration::from_millis(1500) {
        tick_all(&mut players);
        counted_down |= players.iter().all(|p| p.frontend.play_together_reset_countdown_ms() > 0);
        std::thread::sleep(Duration::from_millis(1));
    }
    if !counted_down {
        println!("FAIL: the reset countdown never showed on every player");
        failed = true;
    }
    tick_all_for(&mut players, Duration::from_secs(2));

    // Leave, then play every saved friend replay back to its end.
    let replay_files: Vec<(String, PathBuf, u64)> = players.iter().enumerate().flat_map(|(i, p)| {
        let state = p.frontend.play_together_state();
        // A friend's replay lives with the local copy of *their* ROM.
        let followed_rom = if i == 0 { peer_rom.as_ref().unwrap_or(&rom) } else { &rom };
        let dir = p.frontend.get_replays_dir_for_rom(followed_rom.file_name().unwrap().to_str().unwrap());
        state.participants.into_iter().filter_map(move |q| q.replay_file.map(|f| (format!("{} following {}", p.name, q.name), dir.join(f), q.elapsed_frames)))
    }).collect();
    for p in players.iter_mut() {
        p.frontend.play_together_leave();
    }
    tick_all_for(&mut players, Duration::from_millis(500));

    for (what, path, elapsed_frames) in &replay_files {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                println!("FAIL: {what}: cannot read {}: {e}", path.display());
                failed = true;
                continue;
            }
        };
        let player = match ReplayFilePlayer::new(&bytes, false) {
            Ok(p) => p,
            Err(e) => {
                println!("FAIL: {what}: cannot parse {}: {e:?}", path.display());
                failed = true;
                continue;
            }
        };
        let total = player.get_total_frames();
        let mut player = player;
        player.set_keyframe_states_wanted(false);
        let mut resets = 0;
        let mut inputs = 0u64;
        let mut serial = 0u64;
        while let Some(packet) = player.next_packet().expect("read packet") {
            match packet {
                supershuckie_replay_recorder::Packet::ResetConsole => resets += 1,
                supershuckie_replay_recorder::Packet::ChangeInput { .. } => inputs += 1,
                supershuckie_replay_recorder::Packet::SerialIn { .. } => serial += 1,
                _ => {}
            }
        }
        println!(
            "{what}: {} is v{}, {total} frames, {} keyframes, {resets} resets, {:.3} ChangeInput/frame, {serial} SerialIn, {} bytes (publisher was at frame {elapsed_frames})",
            path.file_name().unwrap().to_string_lossy(), player.get_replay_version(), player.all_keyframes().len(), inputs as f64 / total.max(1) as f64, bytes.len()
        );
        if link_test_rom.is_some() && serial == 0 && (what.ends_with(&format!("following {}", players[0].name)) || what.ends_with(&format!("following {}", players[1].name))) {
            println!("FAIL: {what}: no serial input was recorded although the games were linked");
            failed = true;
        }
        if total < 100 {
            println!("FAIL: {what}: the file is too short");
            failed = true;
        }
        if resets != 1 {
            println!("FAIL: {what}: expected the race-start reset once in the file, found {resets}");
            failed = true;
        }
        if inputs as f64 > 2.0 * total as f64 {
            println!("FAIL: {what}: the input was streamed more than once per frame");
            failed = true;
        }
    }

    // Play the first friend replay back in a fresh frontend.
    if let Some((what, path, _)) = replay_files.first() {
        let mut viewer = make_player(&dir, "Viewer", peer_rom.as_ref().unwrap_or(&rom), speed, None);
        let replays = viewer.frontend.get_replays_dir_for_current_rom().expect("replays dir");
        let name = "friend-smoke";
        std::fs::copy(path, replays.join(format!("{name}.replay"))).unwrap();
        tick_all_for(std::slice::from_mut(&mut viewer), Duration::from_secs(1));
        match viewer.frontend.load_replay_if_exists(name, false) {
            Ok(true) => {}
            other => {
                println!("FAIL: {what}: load_replay_if_exists: {other:?}");
                std::process::exit(1);
            }
        }
        let stats = viewer.frontend.get_replay_playback_stats().expect("attached");
        assert_eq!(viewer.frontend.get_replay_state(), SuperShuckieReplayState::Playback);
        viewer.frontend.set_paused(false);
        let started = Instant::now();
        // The file's length at the viewer's speed, with slack.
        let budget = Duration::from_secs_f64(stats.total_frames as f64 / (60.0 * speed) * 1.5 + 10.0);
        while started.elapsed() < budget {
            tick_all(std::slice::from_mut(&mut viewer));
            if viewer.frontend.is_paused() && viewer.frontend.get_replay_frame() >= stats.total_frames.saturating_sub(1) {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let end = viewer.frontend.get_replay_frame();
        println!("{what}: played back to frame {end} of {}", stats.total_frames);
        if end + 1 < stats.total_frames {
            println!("FAIL: {what}: playback stopped {} frames early", stats.total_frames - end);
            failed = true;
        }
    }

    if failed {
        println!("FAILED");
        std::process::exit(1);
    }
    println!("OK");
}
