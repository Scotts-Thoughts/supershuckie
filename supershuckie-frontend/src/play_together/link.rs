//! The link cable over Play Together: plugging a cable between this player's game and a
//! friend's, so the two can trade and battle.
//!
//! Both machines already have both games: the player's own and the follower that mirrors the
//! friend's. Plugging in lends the follower's core loop to the player's own core thread, which
//! then runs the two consoles in delay-based lockstep (see `supershuckie_core::link`): every
//! input, RAM write and reset is scheduled `delay` frames ahead and sent to the friend in a
//! `LinkFrame`, so both machines feed both consoles the same events on the same frames, and the
//! serial cable itself never crosses the network.
//!
//! The handshake, over the session:
//!
//! ```text
//! request ──▶ LinkRequest ──▶ friend's UI asks ──▶ LinkAccept ──▶ both hold their game
//!         ◀── LinkDecline (or the host declines: busy, other console)
//! both send LinkStart { frame, input, rtt, delay setting, host speed } ──▶ both compute the same delay,
//! lend the follower, install the inbox sink, plug in ──▶ Linked (LinkFrames both ways)
//! Unlink { reason } from either side, a departure, a desync or a stall ends it; the follower
//! goes back to its own thread and asks for a fresh snapshot.
//! ```
//!
//! Speed: every linked pair runs at the session host's game speed (`LinkSpeed` from the host,
//! kept in `PlayTogetherSession::link_speed`), on both machines, so neither side stalls on the
//! other and the input delay can be sized for the trip at that speed. A client's own speed
//! controls do nothing while its cable is in; the host's keep working and drive everyone.
//!
//! Nothing here waits on a core thread from the tick: `link_hold`, `lend` and `unlink` are
//! one-off calls made when the user acts or a message arrives; the status is polled.

use super::*;
use supershuckie_core::link::{LinkFailure, LinkFrame, LinkInbox, LinkPublisherFns, LinkSettings};
use supershuckie_core::{LinkStatus, Speed};
use supershuckie_play_together::{can_link, LinkDeclineReason, LinkEvent, LinkMessage, LinkSink, UnlinkReason, MAX_LINK_DELAY};

/// How long a request waits for the other player's answer, and how long an incoming request
/// waits for this player's.
const LINK_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the two sides have, once both accepted, to exchange their start frames and plug in.
const LINK_START_TIMEOUT: Duration = Duration::from_secs(8);

/// A stall shorter than this is not shown as waiting.
const LINK_STALL_SHOWN_AFTER: Duration = Duration::from_millis(100);

/// A friend's game further behind than this cannot be linked with: catching up would take too
/// long, and their snapshot may still be in flight.
const MAX_LINK_FRAMES_BEHIND: u64 = 30;

/// The length of a Game Boy / Game Boy Advance frame, in milliseconds (both run at 59.7275 Hz).
const LINK_FRAME_MS: f64 = 1000.0 / 59.7275;

/// What the other player told us about where their game stopped.
#[derive(Clone, Debug)]
pub(super) struct PartnerStart {
    frame: u64,
    input: InputBuffer,
    rtt_millis: u32,
    delay_setting: u8,
    speed: Speed
}

/// The cable is in (or going in): the inbox the friend's frames land in and the delay agreed.
pub(super) struct Plugged {
    inbox: Arc<LinkInbox>,
    delay: u64
}

/// Where this player stands with a link cable.
pub(super) enum LinkPhase {
    /// We asked `peer` and wait for their answer.
    Requesting {
        peer: PeerId,
        nonce: u32,
        since: Instant
    },
    /// `peer` asked us; the UI is asking the player.
    Incoming {
        peer: PeerId,
        nonce: u32,
        since: Instant
    },
    /// Both agreed; our game is held at `my_frame`. Waiting for `peer`'s start frame, then for
    /// the cores to plug in. `inbox` already receives their frames: its sink went in before our
    /// `LinkStart` went out, and they send nothing before they have that.
    Starting {
        peer: PeerId,
        nonce: u32,
        my_frame: u64,
        my_rtt: u32,
        inbox: Arc<LinkInbox>,
        their: Option<PartnerStart>,
        plugged: Option<Plugged>,
        since: Instant
    },
    /// Running in lockstep.
    Linked {
        peer: PeerId,
        plugged: Plugged,
        link_frame: u64,
        stalled_since: Option<Instant>,
        since: Instant
    }
}

impl LinkPhase {
    /// The other end.
    pub(super) fn peer(&self) -> PeerId {
        match self {
            Self::Requesting { peer, .. } | Self::Incoming { peer, .. } | Self::Starting { peer, .. } | Self::Linked { peer, .. } => *peer
        }
    }

    fn since(&self) -> Instant {
        match self {
            Self::Requesting { since, .. } | Self::Incoming { since, .. } | Self::Starting { since, .. } | Self::Linked { since, .. } => *since
        }
    }

    /// Whether the cores are plugged together (running, or about to, in lockstep).
    pub(super) fn is_plugged(&self) -> bool {
        matches!(self, Self::Starting { plugged: Some(_), .. } | Self::Linked { .. })
    }

    fn as_str(&self) -> &'static str {
        match self {
            Self::Requesting { .. } => "requesting",
            Self::Incoming { .. } => "incoming",
            Self::Starting { .. } => "starting",
            Self::Linked { .. } => "linked"
        }
    }
}

/// The link cable, for the UI.
#[derive(Serialize, Clone, Debug)]
pub struct LinkView {
    /// `none`, `requesting` (we asked), `incoming` (answer with `play_together_link_respond`),
    /// `starting` (plugging in) or `linked`.
    pub phase: &'static str,
    /// The other end (0 when `none`).
    pub peer_id: PeerId,
    pub peer_name: String,
    /// The request's nonce (what `play_together_link_respond` takes), while `incoming`.
    pub nonce: u32,
    /// The input delay in frames, once agreed (0 before).
    pub input_delay: u8,
    /// Whether the pair is waiting for the other side's frames (linked only).
    pub stalled: bool,
    /// How long the phase has lasted.
    pub since_ms: u64,
    /// Link frames run so far (linked only).
    pub link_frame: u64,
    /// The speed multiplier the pair runs at: the session host's game speed.
    pub speed: f64,
    /// Why the last cable came out, or the last request came to nothing (empty until then).
    pub last_reason: String
}

impl LinkView {
    pub(super) fn none(last_reason: String) -> LinkView {
        LinkView { phase: "none", peer_id: 0, peer_name: String::new(), nonce: 0, input_delay: 0, stalled: false, since_ms: 0, link_frame: 0, speed: 1.0, last_reason }
    }
}

/// Where the friend's link frames go: straight into the core's inbox, on the reader thread.
struct InboxSink {
    inbox: Arc<LinkInbox>
}

impl LinkSink for InboxSink {
    fn frame(&mut self, frame: u64, elapsed_millis: u64, events: Vec<Packet>, pair_hash: Option<(u64, [u8; 32])>) {
        self.inbox.push(frame, LinkFrame { events, elapsed_millis, pair_hash });
    }

    fn ended(&mut self, _reason: LeaveReason) {
        self.inbox.end();
    }
}

/// Where our link frames go: the session's urgent lane, from the core thread.
struct SessionLinkPublisher {
    session: Arc<dyn Session>,
    target: PeerId,
    errors: Vec<String>
}

impl LinkPublisherFns for SessionLinkPublisher {
    fn frame(&mut self, frame: u64, elapsed_millis: u64, events: Vec<Packet>, pair_hash: Option<(u64, [u8; 32])>) {
        if let Err(e) = self.session.send_link(LinkMessage::Frame { target: self.target, frame, elapsed_millis, events, pair_hash }) && self.errors.len() < 8 {
            self.errors.push(format!("{e}"));
        }
    }

    fn poll_errors(&mut self) -> Vec<String> {
        core::mem::take(&mut self.errors)
    }
}

/// The input delay both sides agree on: enough frames, at `speed` times normal, to cover the
/// one-way trip through the host plus one, or more if either player asked for more; at least
/// 1, at most [`MAX_LINK_DELAY`].
pub fn compute_link_delay(rtt_a_ms: u32, rtt_b_ms: u32, setting_a: u8, setting_b: u8, speed: f64) -> u64 {
    let one_way_ms = (rtt_a_ms as f64 + rtt_b_ms as f64) / 2.0;
    let frame_ms = LINK_FRAME_MS / speed.max(f64::MIN_POSITIVE);
    let auto = (one_way_ms / frame_ms).ceil() as u64 + 1;
    auto.max(setting_a as u64).max(setting_b as u64).clamp(1, MAX_LINK_DELAY as u64)
}

fn describe_decline(reason: LinkDeclineReason) -> &'static str {
    match reason {
        LinkDeclineReason::Declined => "declined",
        LinkDeclineReason::Busy => "is already linked with someone",
        LinkDeclineReason::ConsoleMismatch => "is playing on a different console",
        LinkDeclineReason::NotFollowing => "is not following your game closely enough",
        LinkDeclineReason::Timeout => "did not answer",
        LinkDeclineReason::Unavailable => "cannot link right now",
        LinkDeclineReason::Other => "declined (unknown reason)"
    }
}

fn describe_unlink(reason: UnlinkReason) -> &'static str {
    match reason {
        UnlinkReason::Unplugged => "unplugged the link cable",
        UnlinkReason::Desync => "reported that the games went out of sync",
        UnlinkReason::Timeout => "stopped hearing from you",
        UnlinkReason::PeerLeft => "left the session",
        UnlinkReason::Failed => "could not keep the link going",
        UnlinkReason::Busy => "is already linked with someone",
        UnlinkReason::Other => "unplugged the link cable (unknown reason)"
    }
}

impl SuperShuckieFrontend {
    /// Whether a link cable is in, going in, or being asked for: what refuses save-state loads,
    /// speed changes and the like.
    #[inline]
    pub fn is_link_cable_plugged(&self) -> bool {
        self.play_together.as_ref().is_some_and(|s| s.link.is_some())
    }

    /// Whether the two games are running in lockstep right now.
    #[inline]
    pub fn is_link_cable_linked(&self) -> bool {
        self.play_together.as_ref().is_some_and(|s| matches!(s.link, Some(LinkPhase::Linked { .. })))
    }

    /// The link cable, for the UI.
    pub fn play_together_link_state(&self) -> LinkView {
        let Some(s) = self.play_together.as_ref() else {
            return LinkView::none(String::new())
        };
        let Some(phase) = s.link.as_ref() else {
            return LinkView::none(s.last_link_reason.clone())
        };
        let (nonce, input_delay, stalled, link_frame) = match phase {
            LinkPhase::Requesting { nonce, .. } | LinkPhase::Incoming { nonce, .. } => (*nonce, 0, false, 0),
            LinkPhase::Starting { nonce, plugged, .. } => (*nonce, plugged.as_ref().map(|p| p.delay as u8).unwrap_or(0), false, 0),
            LinkPhase::Linked { plugged, link_frame, stalled_since, .. } => {
                (0, plugged.delay as u8, stalled_since.is_some_and(|t| t.elapsed() >= LINK_STALL_SHOWN_AFTER), *link_frame)
            }
        };
        LinkView {
            phase: phase.as_str(),
            peer_id: phase.peer(),
            peer_name: s.name_of(phase.peer()),
            nonce,
            input_delay,
            stalled,
            since_ms: phase.since().elapsed().as_millis().min(u64::MAX as u128) as u64,
            link_frame,
            speed: s.link_speed.into_multiplier_float(),
            last_reason: s.last_link_reason.clone()
        }
    }

    /// The input delay asked for: 0 for automatic (from the round-trip times), else frames.
    #[inline]
    pub fn get_play_together_link_input_delay(&self) -> u8 {
        self.settings.play_together.link_input_delay
    }

    /// Set the input delay asked for (0 = automatic, else 1 to [`MAX_LINK_DELAY`] frames).
    /// Applies to the next cable plugged in; the larger of the two players' settings wins.
    pub fn set_play_together_link_input_delay(&mut self, frames: u8) {
        self.settings.play_together.link_input_delay = frames.min(MAX_LINK_DELAY);
        self.mark_settings_dirty();
    }

    /// Whether `peer`'s game can have a cable plugged into it right now, or why not.
    pub(super) fn link_precondition(&self, session: &PlayTogetherSession, peer_id: PeerId) -> Result<(), String> {
        if session.role == PlayTogetherRole::Connecting {
            return Err(String::from("Not connected yet."))
        }
        if let Some(phase) = session.link.as_ref() {
            return Err(match phase {
                LinkPhase::Requesting { .. } => String::from("A link request is already pending."),
                LinkPhase::Incoming { .. } => String::from("Answer the pending link request first."),
                _ => format!("The link cable is already plugged into {}'s game.", session.name_of(phase.peer()))
            })
        }
        let Some(peer) = session.peers.iter().find(|p| p.peer_id == peer_id) else {
            return Err(String::from("No such player."))
        };
        if peer.linked_with.is_some() {
            return Err(format!("{} is already linked with someone.", peer.name))
        }
        if !can_link(session.local_metadata.console_type, peer.info.publisher.metadata.console_type) {
            return Err(format!("{} is playing on a {}; a link cable only joins two Game Boys or two Game Boy Advances.", peer.name, console_name(peer.info.publisher.metadata.console_type)))
        }
        if peer.core.is_none() {
            return Err(format!("{}'s game is not running here (locate their ROM first).", peer.name))
        }
        if !matches!(peer.status, PeerStatus::Following | PeerStatus::Waiting) || peer.snapshot_requested_at.is_some() {
            return Err(format!("{}'s game is not in sync here yet.", peer.name))
        }
        if peer.last_follow.frames_behind > MAX_LINK_FRAMES_BEHIND {
            return Err(format!("{}'s game is {} frames behind here; wait for it to catch up.", peer.name, peer.last_follow.frames_behind))
        }
        if self.current_replay.is_some() {
            return Err(String::from("Close the replay first."))
        }
        if self.current_export.is_some() {
            return Err(String::from("Wait for the video export to finish."))
        }
        Ok(())
    }

    /// Ask to plug a link cable into `peer`'s game. The answer shows up in the link state
    /// (`starting`/`linked`, or `none` with a reason).
    pub fn play_together_link_request(&mut self, peer_id: PeerId) -> Result<(), UTF8CString> {
        let Some(mut session) = self.play_together.take() else {
            return Err("Not in a Play Together session.".into())
        };
        let result = (|| {
            self.link_precondition(&session, peer_id).map_err(UTF8CString::from)?;
            let nonce = session.next_link_nonce;
            session.next_link_nonce = session.next_link_nonce.wrapping_add(2).max(1);
            let console = u32::from(session.local_metadata.console_type);
            session.session.send_link(LinkMessage::Request { target: peer_id, nonce, console }).map_err(|e| UTF8CString::from(format!("{e}")))?;
            session.link = Some(LinkPhase::Requesting { peer: peer_id, nonce, since: Instant::now() });
            session.last_link_reason.clear();
            session.bump();
            Ok(())
        })();
        self.play_together = Some(session);
        result
    }

    /// Answer the incoming request `nonce`: plug in (`accept`) or decline.
    pub fn play_together_link_respond(&mut self, nonce: u32, accept: bool) -> Result<(), UTF8CString> {
        let Some(mut session) = self.play_together.take() else {
            return Err("Not in a Play Together session.".into())
        };
        let result = self.link_respond_in(&mut session, nonce, accept);
        self.play_together = Some(session);
        result
    }

    fn link_respond_in(&mut self, session: &mut PlayTogetherSession, nonce: u32, accept: bool) -> Result<(), UTF8CString> {
        let peer = match session.link.as_ref() {
            Some(LinkPhase::Incoming { peer, nonce: pending, .. }) if *pending == nonce => *peer,
            _ => return Err(UTF8CString::from("That link request is no longer pending."))
        };
        if !accept {
            session.link = None;
            let _ = session.session.send_link(LinkMessage::Decline { target: peer, nonce, reason: LinkDeclineReason::Declined });
            session.last_link_reason = format!("You declined {}'s link cable.", session.name_of(peer));
            session.bump();
            return Ok(())
        }
        // The pending request itself is not in the way of the check.
        let pending = session.link.take();
        let outcome = self.link_precondition(session, peer);
        session.link = pending;
        if let Err(why) = outcome {
            session.link = None;
            let _ = session.session.send_link(LinkMessage::Decline { target: peer, nonce, reason: LinkDeclineReason::Unavailable });
            session.last_link_reason = why.clone();
            session.bump();
            return Err(UTF8CString::from(why))
        }
        session.session.send_link(LinkMessage::Accept { target: peer, nonce }).map_err(|e| UTF8CString::from(format!("{e}")))?;
        self.hold_for_link(session, peer, nonce);
        Ok(())
    }

    /// Pull the cable (or withdraw a request, or decline a pending one).
    pub fn play_together_unlink(&mut self) {
        let Some(mut session) = self.play_together.take() else {
            return
        };
        let note = match session.link.as_ref() {
            Some(LinkPhase::Requesting { peer, .. }) => format!("You withdrew the link request to {}.", session.name_of(*peer)),
            Some(LinkPhase::Incoming { peer, .. }) => format!("You declined {}'s link cable.", session.name_of(*peer)),
            Some(phase) => format!("You unplugged the link cable from {}'s game.", session.name_of(phase.peer())),
            None => String::new()
        };
        self.unlink_cable(&mut session, UnlinkReason::Unplugged, true, note);
        self.play_together = Some(session);
    }

    /// Hold our game at its next frame boundary and tell `peer` where.
    fn hold_for_link(&mut self, session: &mut PlayTogetherSession, peer: PeerId, nonce: u32) {
        match self.core.link_hold() {
            Ok((my_frame, input)) => {
                let my_rtt = session.local_rtt_ms;
                let delay_setting = self.settings.play_together.link_input_delay.min(MAX_LINK_DELAY);
                // Their frames may arrive the moment they have our start: the inbox is ready first.
                let inbox = Arc::new(LinkInbox::new());
                session.session.set_link_sink(peer, Some(Box::new(InboxSink { inbox: Arc::clone(&inbox) })));
                let speed = session.link_speed;
                if let Err(e) = session.session.send_link(LinkMessage::Start { target: peer, nonce, frame: my_frame, input, rtt_millis: my_rtt, delay_setting, speed }) {
                    self.core.link_release();
                    session.session.set_link_sink(peer, None);
                    session.link = None;
                    session.last_link_reason = format!("Could not start the link: {e}");
                    session.bump();
                    return
                }
                session.link = Some(LinkPhase::Starting { peer, nonce, my_frame, my_rtt, inbox, their: None, plugged: None, since: Instant::now() });
                session.last_link_reason.clear();
                session.bump();
            }
            Err(e) => {
                session.link = None;
                let _ = session.session.send_link(LinkMessage::Unlink { target: peer, reason: UnlinkReason::Failed });
                session.last_link_reason = format!("Could not hold the game for the link: {e}");
                session.bump();
            }
        }
    }

    /// Both start frames are known: agree on the delay, lend the follower and plug in.
    fn plug_in(&mut self, session: &mut PlayTogetherSession) {
        let (peer, my_frame, my_rtt, their, inbox) = match session.link.as_ref() {
            Some(LinkPhase::Starting { peer, my_frame, my_rtt, their: Some(their), plugged: None, inbox, .. }) => (*peer, *my_frame, *my_rtt, their.clone(), Arc::clone(inbox)),
            _ => return
        };
        // The faster of the two reports of the host's speed, so both ends size the delay alike
        // even if the host changed it mid-handshake (a later `LinkSpeed` reaches both anyway).
        let delay_speed = session.link_speed.into_multiplier_float().max(their.speed.into_multiplier_float());
        let delay = compute_link_delay(my_rtt, their.rtt_millis, self.settings.play_together.link_input_delay, their.delay_setting, delay_speed);
        let local_is_first = session.local_peer_id < peer;

        let lent = match session.peers.iter().find(|p| p.peer_id == peer).and_then(|p| p.core.as_ref()) {
            Some(core) => core.lend(),
            None => Err(String::from("their game is not running here"))
        };
        let lent = match lent {
            Ok(lent) => lent,
            Err(e) => {
                let name = session.name_of(peer);
                self.unlink_cable(session, UnlinkReason::Failed, true, format!("Could not plug into {name}'s game: {e}"));
                return
            }
        };
        // The follower is driven by the link frames from here on; its stream is subscribed to
        // again when the cable comes out.
        session.session.unsubscribe(peer);
        let publisher = SessionLinkPublisher { session: Arc::clone(&session.session), target: peer, errors: Vec::new() };
        // Both machines run the pair at the host's speed: the lockstep paces them together.
        self.set_core_speed(session.link_speed);
        self.core.link(
            lent,
            LinkSettings { delay_frames: delay, local_is_first, local_start_frame: my_frame, partner_start_frame: their.frame, partner_start_input: their.input },
            Arc::clone(&inbox),
            Box::new(publisher)
        );
        if let Some(LinkPhase::Starting { plugged, .. }) = session.link.as_mut() {
            *plugged = Some(Plugged { inbox, delay });
        }
        session.bump();
    }

    /// End whatever link there is (`reason` is what the other player is told, when `tell`):
    /// the lent core goes back to its thread and its stream is subscribed to again, the sink
    /// and the inbox close, and the speed is the player's own again.
    pub(super) fn unlink_cable(&mut self, session: &mut PlayTogetherSession, reason: UnlinkReason, tell: bool, note: String) {
        let Some(phase) = session.link.take() else {
            return
        };
        let peer = phase.peer();
        match phase {
            LinkPhase::Requesting { .. } => {
                if tell {
                    // Clears the request at the host, whether or not it was relayed yet.
                    let _ = session.session.send_link(LinkMessage::Unlink { target: peer, reason });
                }
            }
            LinkPhase::Incoming { nonce, .. } => {
                if tell {
                    let decline = if reason == UnlinkReason::Timeout { LinkDeclineReason::Timeout } else { LinkDeclineReason::Declined };
                    let _ = session.session.send_link(LinkMessage::Decline { target: peer, nonce, reason: decline });
                }
            }
            LinkPhase::Starting { plugged: None, inbox, .. } => {
                self.core.link_release();
                inbox.end();
                session.session.set_link_sink(peer, None);
                if tell {
                    let _ = session.session.send_link(LinkMessage::Unlink { target: peer, reason });
                }
            }
            LinkPhase::Starting { plugged: Some(plugged), .. } | LinkPhase::Linked { plugged, .. } => {
                // Blocking, but a one-off: the partner's loop has to be back on its own thread
                // before anything else touches it.
                self.core.unlink();
                plugged.inbox.end();
                session.session.set_link_sink(peer, None);
                if tell {
                    let _ = session.session.send_link(LinkMessage::Unlink { target: peer, reason });
                }
                self.reset_speed();
                // Their stream again, from a fresh snapshot.
                let feeder = session.peers.iter().find(|p| p.peer_id == peer && p.core.is_some()).and_then(|p| p.feeder.clone());
                if let Some(feeder) = feeder {
                    match session.session.subscribe(peer, Box::new(FeederSink { feeder })) {
                        Ok(()) => {
                            if let Some(p) = session.peer_mut(peer) {
                                p.snapshot_requested_at = Some(Instant::now());
                            }
                        }
                        Err(e) => {
                            let name = session.name_of(peer);
                            session.note_error(format!("Could not follow {name}'s game again: {e}"));
                        }
                    }
                }
            }
        }
        if !note.is_empty() {
            session.last_link_reason = note;
        }
        session.bump();
    }

    /// A link cable message addressed to us, or a change in who is linked with whom.
    pub(super) fn handle_link_event(&mut self, session: &mut PlayTogetherSession, event: LinkEvent) {
        match event {
            LinkEvent::Requested { from, nonce, .. } => {
                let name = session.name_of(from);
                match self.link_precondition(session, from) {
                    Ok(()) => {
                        session.link = Some(LinkPhase::Incoming { peer: from, nonce, since: Instant::now() });
                        session.last_link_reason.clear();
                        session.bump();
                        if self.settings.play_together.link_auto_accept {
                            let _ = self.link_respond_in(session, nonce, true);
                        }
                    }
                    Err(why) => {
                        let reason = match session.link.as_ref() {
                            Some(_) => LinkDeclineReason::Busy,
                            None if session.peers.iter().any(|p| p.peer_id == from && p.core.is_some()) => LinkDeclineReason::NotFollowing,
                            None => LinkDeclineReason::Unavailable
                        };
                        let _ = session.session.send_link(LinkMessage::Decline { target: from, nonce, reason });
                        session.note_error(format!("{name} wanted to plug a link cable into your game, but it was declined: {why}"));
                    }
                }
            }
            LinkEvent::Accepted { from, nonce } => {
                match session.link.as_ref() {
                    Some(LinkPhase::Requesting { peer, nonce: ours, .. }) if *peer == from && *ours == nonce => {
                        self.hold_for_link(session, from, nonce);
                    }
                    // Nothing we asked for (a stale request): the host recorded the pair, so
                    // undo that.
                    _ => {
                        let _ = session.session.send_link(LinkMessage::Unlink { target: from, reason: UnlinkReason::Failed });
                    }
                }
            }
            LinkEvent::Declined { from, nonce, reason } => {
                if let Some(LinkPhase::Requesting { peer, nonce: ours, .. }) = session.link.as_ref() && *peer == from && *ours == nonce {
                    let name = session.name_of(from);
                    session.link = None;
                    session.last_link_reason = format!("{name} {}.", describe_decline(reason));
                    session.bump();
                }
            }
            LinkEvent::Started { from, nonce, frame, input, rtt_millis, delay_setting, speed } => {
                if let Some(LinkPhase::Starting { peer, nonce: ours, their, .. }) = session.link.as_mut() && *peer == from && *ours == nonce {
                    *their = Some(PartnerStart { frame, input, rtt_millis, delay_setting, speed });
                    self.plug_in(session);
                }
            }
            LinkEvent::Unlinked { from, reason } => {
                if session.link.as_ref().is_some_and(|l| l.peer() == from) {
                    let name = session.name_of(from);
                    self.unlink_cable(session, reason, false, format!("{name} {}.", describe_unlink(reason)));
                }
            }
            LinkEvent::PeerLinked { a, b } => {
                for p in session.peers.iter_mut() {
                    if p.peer_id == a {
                        p.linked_with = Some(b);
                    }
                    else if p.peer_id == b {
                        p.linked_with = Some(a);
                    }
                }
                session.bump();
            }
            LinkEvent::PeerUnlinked { a, b } => {
                for p in session.peers.iter_mut() {
                    if (p.peer_id == a && p.linked_with == Some(b)) || (p.peer_id == b && p.linked_with == Some(a)) {
                        p.linked_with = None;
                    }
                }
                session.bump();
            }
        }
    }

    /// Timeouts and the core's link status, once a tick.
    pub(super) fn tick_link(&mut self, session: &mut PlayTogetherSession, errors: &mut String) {
        for e in self.core.get_link_errors() {
            errors.push_str(&format!("- LINK CABLE: {e}\n"));
        }
        let (peer, name) = match session.link.as_ref() {
            Some(phase) => (phase.peer(), session.name_of(phase.peer())),
            None => return
        };
        let now = Instant::now();
        let status = match session.link.as_ref() {
            Some(LinkPhase::Starting { plugged: Some(_), .. } | LinkPhase::Linked { .. }) => Some(self.core.link_status()),
            _ => None
        };

        // Decided with the phase borrowed, acted on after.
        let mut unlink: Option<(UnlinkReason, String)> = None;
        let mut failed: Option<LinkFailure> = None;
        let mut now_linked: Option<(u64, bool)> = None;
        let mut bump = false;
        match session.link.as_mut() {
            None => return,
            Some(LinkPhase::Requesting { since, .. }) => {
                if now.duration_since(*since) > LINK_REQUEST_TIMEOUT {
                    unlink = Some((UnlinkReason::Timeout, format!("{name} did not answer the link request.")));
                }
            }
            Some(LinkPhase::Incoming { since, .. }) => {
                if now.duration_since(*since) > LINK_REQUEST_TIMEOUT {
                    unlink = Some((UnlinkReason::Timeout, format!("{name}'s link request expired.")));
                }
            }
            Some(LinkPhase::Starting { plugged: None, since, .. }) => {
                if now.duration_since(*since) > LINK_START_TIMEOUT {
                    unlink = Some((UnlinkReason::Timeout, format!("{name}'s game did not start the link in time.")));
                }
            }
            Some(LinkPhase::Starting { plugged: Some(_), since, .. }) => match status {
                Some(LinkStatus::Linked { link_frame, stalled, .. }) => now_linked = Some((link_frame, stalled)),
                Some(LinkStatus::Failed(failure)) => failed = Some(failure),
                _ => {
                    // The core has its own five-second limit on bringing the follower to the
                    // agreed frame; this is the backstop.
                    if now.duration_since(*since) > LINK_START_TIMEOUT + Duration::from_secs(5) {
                        unlink = Some((UnlinkReason::Timeout, format!("Could not bring {name}'s game to the agreed frame in time.")));
                    }
                }
            },
            Some(LinkPhase::Linked { link_frame, stalled_since, .. }) => match status {
                Some(LinkStatus::Linked { link_frame: frame, stalled, .. }) => {
                    let was_stalled = stalled_since.is_some_and(|t| now.duration_since(t) >= LINK_STALL_SHOWN_AFTER);
                    *link_frame = frame;
                    if stalled {
                        stalled_since.get_or_insert(now);
                    }
                    else {
                        *stalled_since = None;
                    }
                    let is_stalled = stalled_since.is_some_and(|t| now.duration_since(t) >= LINK_STALL_SHOWN_AFTER);
                    bump = is_stalled != was_stalled;
                }
                Some(LinkStatus::Failed(failure)) => failed = Some(failure),
                _ => unlink = Some((UnlinkReason::Failed, String::from("The link ended unexpectedly.")))
            }
        }

        if let Some((reason, note)) = unlink {
            self.unlink_cable(session, reason, true, note);
        }
        else if let Some(failure) = failed {
            self.link_failed(session, failure);
        }
        else if let Some((link_frame, stalled)) = now_linked {
            if let Some(LinkPhase::Starting { plugged: Some(plugged), .. }) = session.link.take() {
                session.link = Some(LinkPhase::Linked { peer, plugged, link_frame, stalled_since: stalled.then_some(now), since: now });
            }
            session.bump();
        }
        else if bump {
            session.bump();
        }
    }

    /// The core ended the link on its own.
    fn link_failed(&mut self, session: &mut PlayTogetherSession, failure: LinkFailure) {
        let name = session.link.as_ref().map(|l| session.name_of(l.peer())).unwrap_or_default();
        let (reason, note) = match failure {
            LinkFailure::PairHashMismatch { frame } => (UnlinkReason::Desync, format!("The link cable was unplugged: your game and {name}'s went out of sync (at link frame {frame}).")),
            LinkFailure::PartnerEnded => (UnlinkReason::Unplugged, format!("The link cable was unplugged: {name}'s frames stopped.")),
            LinkFailure::Timeout => (UnlinkReason::Timeout, format!("The link cable was unplugged: {name}'s frames stopped arriving.")),
            LinkFailure::Emulator(e) => (UnlinkReason::Failed, format!("The link cable was unplugged: {e}"))
        };
        self.unlink_cable(session, reason, true, note);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_delay_covers_the_trip_through_the_host() {
        // Two players on a LAN: a frame of slack over the (sub-frame) one-way time.
        assert_eq!(compute_link_delay(2, 3, 0, 0, 1.0), 2);
        // 40 ms + 60 ms round trips: 50 ms one way is three frames, plus one.
        assert_eq!(compute_link_delay(40, 60, 0, 0, 1.0), 4);
        // A player asking for more gets it; the larger setting wins.
        assert_eq!(compute_link_delay(2, 3, 6, 0, 1.0), 6);
        assert_eq!(compute_link_delay(2, 3, 2, 8, 1.0), 8);
        // Never below 1, never above the limit.
        assert_eq!(compute_link_delay(0, 0, 0, 0, 1.0), 1);
        assert_eq!(compute_link_delay(5000, 5000, 0, 0, 1.0), MAX_LINK_DELAY as u64);
        assert_eq!(compute_link_delay(0, 0, 200, 0, 1.0), MAX_LINK_DELAY as u64);
        // Faster games have shorter frames: the same 50 ms one way is 12 frames at 4x, plus one.
        assert_eq!(compute_link_delay(40, 60, 0, 0, 4.0), 13);
        assert_eq!(compute_link_delay(40, 60, 0, 0, 0.5), 3);
        assert_eq!(compute_link_delay(40, 60, 0, 0, 8.0), MAX_LINK_DELAY as u64);
        assert_eq!(compute_link_delay(0, 0, 0, 0, 0.0), 1);
    }
}
