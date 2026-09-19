//! A link cable between this player's console and a partner's, over Play Together.
//!
//! The cable itself never crosses the network (see [`crate::emulator::link`]): on each machine
//! the player's own [`SuperShuckieCore`] is plugged into the follower core of the partner (lent
//! to the same thread for the duration, see `thread.rs`), and the two consoles are stepped in a
//! deterministic interleave. What crosses the network are the *inputs* (and the other per-frame
//! events: RAM writes, resets) of both players, sent `delay_frames` ahead of the frame they
//! apply to, so that both machines run the pair from identical inputs:
//!
//! * the local side ([`LinkState`]) schedules every event it generates for link frame
//!   `now + delay`, sends it to the partner as a [`LinkFrame`] through a [`LinkPublisherFns`],
//!   and applies the events it scheduled `delay` frames ago;
//! * the partner side ([`LinkPartnerState`], on the lent follower) applies the [`LinkFrame`]s that
//!   arrive in its [`LinkInbox`], one per frame, instead of the partner's stream; a frame whose
//!   events have not arrived stalls the pair.
//!
//! Link frames are counted from the moment the cable was plugged in (frame 0), independently of
//! either console's replay frame counter. The first `delay` frames of both consoles use the input
//! each held when the cable went in (pre-filled on both machines, never sent).
//!
//! A *pair hash* (blake3 of both consoles' work RAM at the same point of the interleave, every
//! [`PAIR_HASH_INTERVAL_FRAMES`] frames of the first console) rides along in the link frames so a
//! divergence between the two machines is caught within a second and the cable pulled.

use crate::emulator::link::{second_runs_next, LinkError};
use crate::emulator::{Input, RunTime};
use crate::{ByteVec, SuperShuckieCore};
use std::boxed::Box;
use std::collections::{BTreeMap, VecDeque};
use std::string::String;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::Thread;
use std::vec::Vec;
use supershuckie_replay_recorder::replay_file::ReplayConsoleType;
use supershuckie_replay_recorder::{blake3_hash_slices, InputBuffer, Packet, UnsignedInteger};

pub mod test_rom;

#[cfg(test)]
mod tests;

/// Frames of the first console between two pair hashes (one emulated second).
pub const PAIR_HASH_INTERVAL_FRAMES: u64 = 60;

/// Most link frames an inbox holds ahead of the frame being run; anything further ahead is
/// dropped (a hostile or confused peer).
pub const MAX_INBOX_AHEAD_FRAMES: u64 = 600;

/// Most pair hashes remembered on each side while waiting for the other side's.
const PAIR_HASH_WINDOW: usize = 8;

/// How the cable is configured on this machine (the other machine has the mirror image).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkSettings {
    /// How many frames ahead inputs are scheduled (at least 1).
    pub delay_frames: u64,

    /// Whether the local console is the "first" of the pair (the one with the lower peer id):
    /// the interleave rule is stated in terms of first and second so that both machines run the
    /// same one.
    pub local_is_first: bool,

    /// The local console's replay frame count at which it was held for the link.
    pub local_start_frame: u64,

    /// The partner console's replay frame count at which they held it; the lent follower must
    /// be exactly there when the cable goes in.
    pub partner_start_frame: u64,

    /// The input the partner held at that frame, which their first `delay_frames` link frames
    /// use.
    pub partner_start_input: InputBuffer
}

/// One link frame's worth of a console's events, as sent between the machines.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct LinkFrame {
    /// The events to apply before running that frame (`ChangeInput`, `WriteMemory`,
    /// `ResetConsole`, `NoOp`), in the order to apply them.
    pub events: Vec<Packet>,

    /// The sender's recording clock (its `elapsed_millis`, the base its snapshots and stream
    /// use) when it sent the frame, `delay` frames before the frame runs. The receiver's copy
    /// of the sender's console adopts it as its own clock for the frame: behind the sender's
    /// real clock by `delay` frames, never ahead of it, so the snapshot that follows the link
    /// never turns its time backwards.
    pub elapsed_millis: u64,

    /// The sender's pair hash for the first console's link frame given, if one is due.
    pub pair_hash: Option<(u64, [u8; 32])>
}

/// Where the partner's link frames arrive (pushed by the network reader thread, taken by the
/// core thread running the pair).
pub struct LinkInbox {
    frames: Mutex<BTreeMap<u64, LinkFrame>>,
    /// The link frame the pair has run up to (frames below it are moot).
    applied: AtomicU64,
    /// The newest frame number pushed.
    newest: AtomicU64,
    ended: AtomicBool,
    waker: Mutex<Option<Thread>>
}

impl Default for LinkInbox {
    fn default() -> Self {
        Self::new()
    }
}

impl LinkInbox {
    /// An empty inbox.
    pub fn new() -> Self {
        Self {
            frames: Mutex::new(BTreeMap::new()),
            applied: AtomicU64::new(0),
            newest: AtomicU64::new(0),
            ended: AtomicBool::new(false),
            waker: Mutex::new(None)
        }
    }

    /// The partner's events for link frame `frame`. Frames already run and frames more than
    /// [`MAX_INBOX_AHEAD_FRAMES`] ahead are dropped. Wakes the core thread.
    pub fn push(&self, frame: u64, link_frame: LinkFrame) {
        let applied = self.applied.load(Ordering::Relaxed);
        if frame < applied || frame > applied + MAX_INBOX_AHEAD_FRAMES {
            return
        }
        let mut frames = self.frames.lock().unwrap_or_else(|p| p.into_inner());
        frames.insert(frame, link_frame);
        self.newest.fetch_max(frame, Ordering::Relaxed);
        drop(frames);
        self.wake();
    }

    /// The partner is gone: nothing more arrives.
    pub fn end(&self) {
        self.ended.store(true, Ordering::Relaxed);
        self.wake();
    }

    /// Whether [`end`](Self::end) was called.
    pub fn ended(&self) -> bool {
        self.ended.load(Ordering::Relaxed)
    }

    /// The thread to unpark when something arrives.
    pub fn set_waker(&self, thread: Thread) {
        *self.waker.lock().unwrap_or_else(|p| p.into_inner()) = Some(thread);
    }

    /// The newest link frame number pushed so far.
    pub fn newest(&self) -> u64 {
        self.newest.load(Ordering::Relaxed)
    }

    fn wake(&self) {
        if let Some(thread) = self.waker.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
            thread.unpark();
        }
    }

    fn take(&self, frame: u64) -> Option<LinkFrame> {
        let mut frames = self.frames.lock().unwrap_or_else(|p| p.into_inner());
        let taken = frames.remove(&frame);
        if taken.is_some() {
            self.applied.store(frame + 1, Ordering::Relaxed);
            // Anything older is moot.
            while let Some((&first, _)) = frames.iter().next() {
                if first > frame {
                    break
                }
                frames.remove(&first);
            }
        }
        taken
    }
}

/// Where the local side sends its link frames.
///
/// Called on the core thread once per frame; must never block (hand the data to a writer
/// thread) and reports a broken transport through [`poll_errors`](Self::poll_errors).
pub trait LinkPublisherFns: Send + 'static {
    /// The local console's events for link frame `frame`, the local recording clock as of the
    /// frame being sent from (see [`LinkFrame::elapsed_millis`]), and the pair hash if one is
    /// due.
    fn frame(&mut self, frame: u64, elapsed_millis: u64, events: Vec<Packet>, pair_hash: Option<(u64, [u8; 32])>);

    /// Problems since the last call.
    fn poll_errors(&mut self) -> Vec<String>;
}

/// Why a link ended on its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkFailure {
    /// The two machines computed different pair hashes for that link frame of the first
    /// console: the pair diverged.
    PairHashMismatch {
        /// The first console's link frame.
        frame: u64
    },

    /// The partner's link frames stopped arriving for good.
    PartnerEnded,

    /// The partner's link frames did not arrive in time.
    Timeout,

    /// A console core failed.
    Emulator(String)
}

impl core::fmt::Display for LinkFailure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::PairHashMismatch { frame } => write!(f, "the two games fell out of step (frame {frame})"),
            Self::PartnerEnded => f.write_str("the other player's game stopped"),
            Self::Timeout => f.write_str("the other player's inputs stopped arriving"),
            Self::Emulator(what) => write!(f, "{what}")
        }
    }
}

/// What one [`SuperShuckieCore::run_linked`] did.
#[derive(Clone, Debug, PartialEq)]
pub enum LinkRunOutcome {
    /// The local console stepped once (and the partner was kept in step). `local.frames == 0`
    /// with a paced console means the frame was not due yet.
    Ran {
        /// What the local step reported.
        local: RunTime,
        /// The frames the partner completed meanwhile.
        partner_frames: u64
    },

    /// Neither console could step: the partner's next link frame has not arrived.
    Stalled,

    /// The link is broken; the caller ends it.
    Failed(LinkFailure)
}

/// A scheduled link frame of the local console: its events and the `Input` its `ChangeInput`
/// encodes (kept so the applied input can be reported without decoding).
struct ScheduledFrame {
    events: Vec<Packet>,
    input: Option<Input>
}

/// The local side of the cable.
pub struct LinkState {
    settings: LinkSettings,
    publisher: Box<dyn LinkPublisherFns>,

    /// Events scheduled for link frames not yet run.
    queue: BTreeMap<u64, ScheduledFrame>,

    /// The local link frame about to run (frames completed since the cable went in).
    frame: u64,

    /// The link frame the next locally generated event is scheduled for (`frame + delay`).
    next_send_frame: u64,

    /// Whether [`SuperShuckieCore::link_prepare_local_frame`] has run for `frame`.
    frame_prepared: bool,

    /// Pair hashes this machine computed, by the first console's link frame, oldest first.
    computed_hashes: VecDeque<(u64, [u8; 32])>,

    /// Pair hashes the partner sent that have not met a computed one yet.
    received_hashes: VecDeque<(u64, [u8; 32])>,

    /// A computed hash to attach to the next link frame sent.
    pending_hash: Option<(u64, [u8; 32])>,

    /// A first-console frame whose pair hash is due once the second console has caught up.
    hash_due_at: Option<u64>,

    /// The first console's link frames completed (what the pair hash is keyed by).
    first_frames: u64,

    /// Whether the last outcome was a stall, for the caller's status.
    stalled: bool,

    /// Whether the local console's linked steps pace themselves (the default; off for tests
    /// and headless tools that run the pair as fast as it goes).
    paced: bool,

    failure: Option<LinkFailure>
}

impl LinkState {
    /// Schedule an event the local console generated now for the link frame its next fresh
    /// input goes to (`delay` frames ahead). Resets go first in a frame's events, before the
    /// input and the writes, on both machines alike.
    pub(crate) fn schedule(&mut self, packet: Packet) {
        let frame = self.next_send_frame;
        let scheduled = self.queue.entry(frame).or_insert_with(|| ScheduledFrame { events: Vec::new(), input: None });
        match packet {
            Packet::ResetConsole => scheduled.events.insert(0, packet),
            other => scheduled.events.push(other)
        }
    }
}

/// The partner side of the cable, on the lent follower core.
pub struct LinkPartnerState {
    inbox: Arc<LinkInbox>,
    delay: u64,
    prefill_input: InputBuffer,

    /// The partner's link frame about to run.
    frame: u64,

    /// Whether the events for `frame` have been applied.
    frame_prepared: bool
}

/// The link cable family a console belongs to: only consoles of the same family link.
fn link_family(console: Option<ReplayConsoleType>) -> Option<u8> {
    match console {
        Some(ReplayConsoleType::GameBoy | ReplayConsoleType::GameBoyColor | ReplayConsoleType::SuperGameBoy2) => Some(1),
        Some(ReplayConsoleType::GameBoyAdvance) => Some(2),
        _ => None
    }
}

/// What [`SuperShuckieCore::link_prepare_partner_frame`] found.
enum PartnerFrame {
    Ready,
    Waiting,
    Ended
}

impl SuperShuckieCore {
    /// Whether this console is one end of a link cable (either side).
    #[inline]
    pub fn is_linked(&self) -> bool {
        self.link.is_some() || self.link_partner.is_some()
    }

    /// Whether the local console is held at a frame boundary for a link handshake (see
    /// [`Self::link_hold`]).
    #[inline]
    pub fn is_link_holding(&self) -> bool {
        self.link_holding
    }

    /// Stop at the next frame boundary and stay there (nothing runs until [`Self::begin_link`]
    /// or [`Self::link_release`]): the first step of a link handshake. Returns the replay frame
    /// count the console is held at and the input it holds, which the partner needs.
    ///
    /// Refused while a replay is attached or the console is already linked.
    pub fn link_hold(&mut self) -> Result<(u64, InputBuffer), String> {
        if self.is_playing_back() || self.has_replay_attached() {
            return Err(String::from("a replay is attached"))
        }
        if self.is_linked() {
            return Err(String::from("already linked"))
        }
        if self.core.link_port().is_none() {
            return Err(String::from("this console has no link port"))
        }
        self.finish_current_frame();
        self.link_holding = true;
        self.input_scratch_buffer.clear();
        self.core.encode_input(self.current_input, &mut self.input_scratch_buffer);
        let mut input = InputBuffer::new();
        input.extend_from_slice(&self.input_scratch_buffer);
        Ok((self.total_frames, input))
    }

    /// Abandon a hold without linking.
    pub fn link_release(&mut self) {
        if self.link_holding {
            self.link_holding = false;
            self.input_latched = false;
        }
    }

    /// Plug the cable in between this (held) console and `partner`, the lent follower of the
    /// other player, which must sit exactly at `settings.partner_start_frame`.
    ///
    /// From here on the pair is stepped with [`Self::run_linked`] only.
    pub fn begin_link(
        &mut self,
        partner: &mut SuperShuckieCore,
        settings: LinkSettings,
        inbox: Arc<LinkInbox>,
        publisher: Box<dyn LinkPublisherFns>
    ) -> Result<(), String> {
        if !self.link_holding {
            return Err(String::from("the local console is not held for a link"))
        }
        if self.is_linked() || partner.is_linked() {
            return Err(String::from("already linked"))
        }
        if self.core.is_mid_frame() || self.total_frames != settings.local_start_frame {
            return Err(alloc::format!("the local console is at frame {} rather than held at {}", self.total_frames, settings.local_start_frame));
        }
        if !partner.is_following() {
            return Err(String::from("the other player's game is not being followed"))
        }
        if partner.core.is_mid_frame() || partner.total_frames != settings.partner_start_frame {
            return Err(alloc::format!("the other player's game is at frame {} rather than {}", partner.total_frames, settings.partner_start_frame));
        }
        let family = link_family(self.core.replay_console_type());
        if family.is_none() || family != link_family(partner.core.replay_console_type()) {
            return Err(String::from("the two consoles cannot be linked to each other"))
        }
        if settings.delay_frames == 0 {
            return Err(String::from("the input delay must be at least one frame"))
        }
        if self.core.link_port().is_none() || partner.core.link_port().is_none() {
            return Err(String::from("one of the consoles has no link port"))
        }

        if let Err(e) = self.core.link_port().expect("checked").connect(settings.local_is_first) {
            return Err(alloc::format!("cannot plug the local console in: {e}"))
        }
        if let Err(e) = partner.core.link_port().expect("checked").connect(!settings.local_is_first) {
            self.core.link_port().expect("checked").disconnect();
            return Err(alloc::format!("cannot plug the other player's console in: {e}"))
        }

        // The first `delay` frames of both consoles use the inputs they held at the plug-in.
        self.input_scratch_buffer.clear();
        self.core.encode_input(self.current_input, &mut self.input_scratch_buffer);
        let mut local_input = InputBuffer::new();
        local_input.extend_from_slice(&self.input_scratch_buffer);
        let mut queue = BTreeMap::new();
        for frame in 0..settings.delay_frames {
            queue.insert(frame, ScheduledFrame { events: alloc::vec![Packet::ChangeInput { data: local_input.clone() }], input: Some(self.current_input) });
        }
        // Writes queued while held belong to the first frame we can schedule.
        let held_writes = core::mem::take(&mut self.writes);
        let delay = settings.delay_frames;
        let partner_input = settings.partner_start_input.clone();
        self.link = Some(LinkState {
            settings,
            publisher,
            queue,
            frame: 0,
            next_send_frame: delay,
            frame_prepared: false,
            computed_hashes: VecDeque::new(),
            received_hashes: VecDeque::new(),
            pending_hash: None,
            hash_due_at: None,
            first_frames: 0,
            stalled: false,
            paced: true,
            failure: None
        });
        self.writes = held_writes;
        self.flush_writes();
        partner.link_partner = Some(LinkPartnerState {
            inbox,
            delay,
            prefill_input: partner_input,
            frame: 0,
            frame_prepared: false
        });
        partner.replay_waiting = false;
        partner.replay_stalled = false;
        partner.replay_frame_pending = false;
        partner.input_latched = false;
        self.link_holding = false;
        self.input_latched = false;
        self.clear_audio();
        partner.clear_audio();
        Ok(())
    }

    /// Pull the cable: both consoles see no cable from here on, the local console runs alone
    /// under the user's input again (the inputs still scheduled are dropped), and the partner's
    /// follower waits for a fresh snapshot from their stream (the caller re-subscribes).
    pub fn end_link(&mut self, partner: &mut SuperShuckieCore) {
        if let Some(port) = self.core.link_port() {
            port.disconnect();
        }
        if let Some(port) = partner.core.link_port() {
            port.disconnect();
        }
        self.link = None;
        self.link_holding = false;
        self.input_latched = false;
        self.writes.clear();
        partner.link_partner = None;
        partner.input_latched = false;
        partner.replay_frame_pending = false;
        // The partner's frame in progress (its inputs were applied) may finish on its own; at
        // the next boundary its stream is read again, which waits for the fresh snapshot.
        partner.replay_waiting = false;
        if let Some(stats) = partner.follower_stats() {
            stats.waiting.store(true, Ordering::Relaxed);
        }
        self.clear_audio();
        partner.clear_audio();
    }

    /// The failure that ended (or is about to end) the link, if any.
    pub fn link_failure(&self) -> Option<&LinkFailure> {
        self.link.as_ref().and_then(|l| l.failure.as_ref())
    }

    /// Problems the link publisher reported since the last call.
    pub fn poll_link_errors(&mut self) -> Vec<String> {
        self.link.as_mut().map(|l| l.publisher.poll_errors()).unwrap_or_default()
    }

    /// The local console's link frames completed, and the frames ahead its inputs are sent.
    pub fn link_progress(&self) -> Option<(u64, u64)> {
        self.link.as_ref().map(|l| (l.frame, l.settings.delay_frames))
    }

    /// Whether the last [`Self::run_linked`] stalled.
    pub fn is_link_stalled(&self) -> bool {
        self.link.as_ref().is_some_and(|l| l.stalled)
    }

    /// Whether the local console's linked steps pace themselves at the game's speed (the
    /// default) or run as fast as they can (tests, headless tools). Does nothing unless linked.
    pub fn set_link_paced(&mut self, paced: bool) {
        if let Some(link) = self.link.as_mut() {
            link.paced = paced;
        }
    }

    /// Step the pair: the local console once (paced), with the partner kept in step before and
    /// after it. See the module docs for the rule.
    pub fn run_linked(&mut self, partner: &mut SuperShuckieCore) -> LinkRunOutcome {
        let Some(link) = self.link.as_ref() else {
            return LinkRunOutcome::Failed(LinkFailure::Emulator(String::from("not linked")))
        };
        if partner.link_partner.is_none() {
            return LinkRunOutcome::Failed(LinkFailure::Emulator(String::from("the partner is not linked")))
        }
        if let Some(failure) = link.failure.clone() {
            return LinkRunOutcome::Failed(failure)
        }
        let local_is_first = link.settings.local_is_first;

        /// Loop iterations before the pair is declared stuck (a console whose link time never
        /// advances); a slice is a few steps.
        const MAX_STEPS: u32 = 1_000_000;

        // The partner's frames are drawn whenever they complete: its window is live.
        partner.core.set_skip_drawing(false);
        self.core.set_skip_drawing(false);

        let mut partner_frames = 0u64;
        for _ in 0..MAX_STEPS {
            // Whose turn: the console that is behind in its own emulated time.
            let second_runs = {
                let (Some(local_port), Some(partner_port)) = (self.core.link_port(), partner.core.link_port()) else {
                    return self.link_fail(LinkFailure::Emulator(String::from("a console lost its link port")))
                };
                let decision = if local_is_first {
                    second_runs_next(&*local_port, &*partner_port)
                }
                else {
                    second_runs_next(&*partner_port, &*local_port)
                };
                match decision {
                    Ok(second_runs) => second_runs,
                    Err(e) => return self.link_fail(LinkFailure::Emulator(alloc::format!("{e}")))
                }
            };
            let local_runs = second_runs != local_is_first;

            // A pair hash due for the first console's last completed frame is taken once the
            // second console has caught up, i.e. right before the first steps again.
            if !second_runs && let Some(frame) = self.link.as_mut().and_then(|l| l.hash_due_at.take()) {
                let (first_hash, second_hash) = if local_is_first { (self.sync_hash(), partner.sync_hash()) } else { (partner.sync_hash(), self.sync_hash()) };
                let hash = blake3_hash_slices([first_hash.unwrap_or_default().as_slice(), second_hash.unwrap_or_default().as_slice()].into_iter());
                if let Some(failure) = self.link_note_computed_hash(frame, hash) {
                    return self.link_fail(failure);
                }
            }

            if local_runs {
                if !self.core.is_mid_frame() && !self.link.as_ref().is_some_and(|l| l.frame_prepared) {
                    self.link_prepare_local_frame();
                }
                let paced = self.link.as_ref().is_some_and(|l| l.paced);
                let time = match self.link_step(partner, paced) {
                    Ok(time) => time,
                    Err(e) => return self.link_fail(LinkFailure::Emulator(alloc::format!("the local console failed: {e}")))
                };
                if time.frames > 0 && local_is_first {
                    self.link_first_frame_completed();
                }
                if let Some(l) = self.link.as_mut() {
                    l.stalled = false;
                }
                return LinkRunOutcome::Ran { local: time, partner_frames }
            }

            if !partner.core.is_mid_frame() && !partner.link_partner.as_ref().is_some_and(|p| p.frame_prepared) {
                match self.link_prepare_partner_frame(partner) {
                    Ok(PartnerFrame::Ready) => {}
                    Ok(PartnerFrame::Waiting) => {
                        if let Some(l) = self.link.as_mut() {
                            l.stalled = true;
                        }
                        return LinkRunOutcome::Stalled
                    }
                    Ok(PartnerFrame::Ended) => return self.link_fail(LinkFailure::PartnerEnded),
                    Err(failure) => return self.link_fail(failure)
                }
            }
            let time = match partner.link_step(self, false) {
                Ok(time) => time,
                Err(e) => return self.link_fail(LinkFailure::Emulator(alloc::format!("the other player's console failed: {e}")))
            };
            partner_frames += time.frames;
            if time.frames > 0 && !local_is_first {
                self.link_first_frame_completed();
            }
        }
        self.link_fail(LinkFailure::Emulator(String::from("the linked consoles stopped making progress")))
    }

    fn link_fail(&mut self, failure: LinkFailure) -> LinkRunOutcome {
        if let Some(l) = self.link.as_mut() {
            l.failure = Some(failure.clone());
        }
        LinkRunOutcome::Failed(failure)
    }

    /// The first console completed a link frame: count it and mark a pair hash due when it is
    /// time for one.
    fn link_first_frame_completed(&mut self) {
        let Some(link) = self.link.as_mut() else { return };
        link.first_frames += 1;
        if link.first_frames % PAIR_HASH_INTERVAL_FRAMES == 0 {
            link.hash_due_at = Some(link.first_frames);
        }
    }

    /// Record a pair hash computed here and compare it with any the partner sent for the same
    /// frame; `Some` on a mismatch.
    fn link_note_computed_hash(&mut self, frame: u64, hash: [u8; 32]) -> Option<LinkFailure> {
        let link = self.link.as_mut()?;
        link.pending_hash = Some((frame, hash));
        link.computed_hashes.push_back((frame, hash));
        while link.computed_hashes.len() > PAIR_HASH_WINDOW {
            link.computed_hashes.pop_front();
        }
        Self::link_match_hashes(link)
    }

    /// Match every received hash against the computed ones, dropping what matched or went
    /// stale; `Some` on a mismatch.
    fn link_match_hashes(link: &mut LinkState) -> Option<LinkFailure> {
        let oldest_computed = link.computed_hashes.front().map(|(f, _)| *f).unwrap_or(0);
        let mut mismatch = None;
        link.received_hashes.retain(|(frame, hash)| {
            if let Some((_, mine)) = link.computed_hashes.iter().find(|(f, _)| f == frame) {
                if mine != hash && mismatch.is_none() {
                    mismatch = Some(LinkFailure::PairHashMismatch { frame: *frame });
                }
                return false
            }
            // Older than anything we still remember: cannot be checked any more (harmless).
            *frame >= oldest_computed
        });
        while link.received_hashes.len() > PAIR_HASH_WINDOW {
            link.received_hashes.pop_front();
        }
        mismatch
    }

    /// One step of a linked console: the port's linked step, then the usual post-run work
    /// (frame timekeeping with the link traffic captured, keyframes, the stream).
    fn link_step(&mut self, partner: &mut SuperShuckieCore, paced: bool) -> Result<RunTime, LinkError> {
        self.last_run = RunTime::NONE;
        let time = {
            let Some(port) = self.core.link_port() else {
                return Err(LinkError::NotConnected)
            };
            port.step_linked(partner.core.as_mut(), paced)?
        };
        self.after_run(&time);
        self.drain_audio(true);
        Ok(time)
    }

    /// At a local frame boundary: schedule this frame's fresh input `delay` frames ahead, send
    /// that link frame to the partner, and apply the events scheduled for the frame about to run.
    fn link_prepare_local_frame(&mut self) {
        let elapsed_millis = self.recording_millis().0;
        let input = self.compute_pending_input();
        self.input_scratch_buffer.clear();
        self.core.encode_input(input, &mut self.input_scratch_buffer);
        let mut encoded = InputBuffer::new();
        encoded.extend_from_slice(&self.input_scratch_buffer);

        let (to_send, to_apply) = {
            let Some(link) = self.link.as_mut() else { return };
            let send_frame = link.next_send_frame;
            let scheduled = link.queue.entry(send_frame).or_insert_with(|| ScheduledFrame { events: Vec::new(), input: None });
            scheduled.events.push(Packet::ChangeInput { data: encoded });
            scheduled.input = Some(input);
            let events = scheduled.events.clone();
            let pair_hash = link.pending_hash.take();
            link.publisher.frame(send_frame, elapsed_millis, events, pair_hash);
            link.next_send_frame += 1;
            let frame = link.frame;
            let to_apply = link.queue.remove(&frame);
            link.frame_prepared = true;
            (send_frame, to_apply)
        };
        let _ = to_send;

        let Some(scheduled) = to_apply else { return };
        for packet in &scheduled.events {
            match packet {
                Packet::ResetConsole => {
                    self.core.hard_reset();
                    self.bump_state_epoch();
                    self.clear_audio();
                    self.with_recorder(|r| r.reset_console());
                    self.with_publisher(|p| p.reset_console());
                }
                Packet::ChangeInput { data } => {
                    self.core.set_input_encoded(data.as_slice());
                    if let Some(input) = scheduled.input {
                        self.current_input = input;
                    }
                    if self.stream_publisher.is_some() {
                        let data = data.clone();
                        self.with_publisher(|p| p.set_input(data));
                    }
                    if self.replay_file_recorder.is_some() {
                        let data = data.clone();
                        self.with_recorder(|r| r.set_input(data));
                    }
                }
                Packet::WriteMemory { address, data } => {
                    // Recorded only if it was applied, as any external write is.
                    if u32::try_from(*address).is_ok_and(|a| self.core.write_ram(a, data.as_slice()).is_ok()) {
                        let (address, data) = (*address, data.clone());
                        if self.stream_publisher.is_some() {
                            let data = data.clone();
                            self.with_publisher(|p| p.write_memory(address, data));
                        }
                        self.with_recorder(|r| r.write_memory(address, data));
                    }
                }
                _ => {}
            }
        }
    }

    /// At a partner frame boundary: apply the partner's events for the frame about to run (the
    /// pre-filled input for the first `delay` frames, then what arrived in the inbox) and note
    /// any pair hash they sent.
    fn link_prepare_partner_frame(&mut self, partner: &mut SuperShuckieCore) -> Result<PartnerFrame, LinkFailure> {
        let now = partner.total_milliseconds.0;
        let (events, pair_hash, elapsed_millis) = {
            let Some(state) = partner.link_partner.as_mut() else {
                return Err(LinkFailure::Emulator(String::from("the partner is not linked")))
            };
            let frame = state.frame;
            let link_frame = if frame < state.delay {
                // The first `delay` frames: the input the partner held, and the clock where its
                // stream left it.
                LinkFrame { events: alloc::vec![Packet::ChangeInput { data: state.prefill_input.clone() }], elapsed_millis: now, pair_hash: None }
            }
            else {
                match state.inbox.take(frame) {
                    Some(link_frame) => link_frame,
                    None if state.inbox.ended() => return Ok(PartnerFrame::Ended),
                    None => return Ok(PartnerFrame::Waiting)
                }
            };
            state.frame_prepared = true;
            (link_frame.events, link_frame.pair_hash, link_frame.elapsed_millis)
        };

        for packet in &events {
            match packet {
                Packet::NoOp | Packet::ChangeInput { .. } | Packet::WriteMemory { .. } | Packet::ResetConsole => partner.apply_playback_packet(packet, None),
                // Not something a link frame may carry (the network layer refuses them); never
                // applied here either.
                _ => {}
            }
        }

        // The partner's clock: the sender's own, as of `delay` frames ago (never backwards).
        partner.total_milliseconds = elapsed_millis.max(now).into();

        if let Some((hash_frame, hash)) = pair_hash && let Some(link) = self.link.as_mut() {
            link.received_hashes.push_back((hash_frame, hash));
            if let Some(failure) = Self::link_match_hashes(link) {
                return Err(failure)
            }
        }
        Ok(PartnerFrame::Ready)
    }

    /// Mark the link frame the console just completed (either side): the next boundary prepares
    /// the next one. Called from the frame timekeeping.
    pub(crate) fn link_frame_completed(&mut self, frames: u64) {
        if let Some(link) = self.link.as_mut() {
            link.frame += frames;
            link.frame_prepared = false;
        }
        if let Some(state) = self.link_partner.as_mut() {
            state.frame += frames;
            state.frame_prepared = false;
        }
    }

    /// The link cable traffic the console received during the frame it just completed, for the
    /// recorder and the stream; nothing when there was none (or no port).
    pub(crate) fn take_serial_in_bytes(&mut self) -> Option<ByteVec> {
        let port = self.core.link_port()?;
        let mut bytes: Vec<u8> = Vec::new();
        port.take_serial_in(&mut bytes);
        if bytes.is_empty() {
            return None
        }
        let mut data = ByteVec::with_capacity(bytes.len());
        data.extend_from_slice(&bytes);
        Some(data)
    }

    /// Replay bookkeeping of the console's link port: events that could not be delivered where
    /// they were recorded (a desync indicator on a follower or a replay); 0 without a port.
    pub fn serial_replay_misses(&mut self) -> u64 {
        self.core.link_port().map(|p| p.serial_replay_misses()).unwrap_or(0)
    }

    /// The unsigned frame index of the partner's link frame about to run (for the thread's
    /// stall diagnostics).
    pub fn link_partner_frame(&self) -> Option<UnsignedInteger> {
        self.link_partner.as_ref().map(|s| s.frame)
    }
}
