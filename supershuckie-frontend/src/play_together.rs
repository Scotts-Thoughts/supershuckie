//! Play Together: playing alongside other players over the network.
//!
//! One player hosts, the others join. Everyone publishes their own game as a live replay stream
//! (see `supershuckie_core::stream`) and follows everyone else's on a core of its own (see
//! `supershuckie_core::live_replay`), driven straight from the network reader threads; the
//! frontend only handles the roster, snapshot requests, the race-start countdown, sync pause,
//! the host's start state, status and the other players' screens and replay files.
//!
//! Two players can also plug a link cable between their games (trading and battling in the
//! Game Boy and Game Boy Advance Pokémon games); see [`link`].

pub mod link;

use crate::settings::{PlayTogetherSettings, PokeAByteSettings};
use crate::util::UTF8CString;
use crate::{ScreenInfo, SuperShuckieEmulatorType, SuperShuckieFrontend, REPLAY_EXTENSION};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufWriter;
use std::num::NonZeroU8;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use supershuckie_core::emulator::PartialReplayRecordMetadata;
use supershuckie_core::live_replay::{live_replay_channel, FollowerStatsSnapshot, LiveReplayFeeder};
use supershuckie_core::stream::{SnapshotRequestReason, StreamPublisherFns};
use supershuckie_core::{AudioOutput, CoreThreadRole, ElapsedTimeStats, ThreadedSuperShuckieCore};
use supershuckie_play_together::{
    describe_incompatibility, follow_compatibility, is_valid_color, probe_local_ip, ClientConfig, ClientSession, FollowCompatibility, FollowerSink,
    HostConfig, HostSession, JoinCode, LeaveReason, LocalParticipant, ParticipantInfo, PublishError, PublisherHandle, PublisherInfo, Session,
    SessionEvent, SnapshotData, StartStateData, PROTOCOL_VERSION
};
pub use link::LinkView;
pub use supershuckie_play_together::{color_entry, PlayerColor, COLOR_COUNT, COLOR_RANDOM, PALETTE};
use supershuckie_replay_recorder::replay_file::{blake3_hash_to_ascii, ReplayConsoleType, ReplayFileMetadata, ReplayHeaderBlake3Hash, ReplayPatchFormat};
use supershuckie_replay_recorder::{append_packet, blake3_hash, ByteVec, Counter, InputBuffer, KeyframeMetadata, Packet, SignedInteger, TimestampMillis, UnsignedInteger};

pub use supershuckie_play_together::PeerId;
use supershuckie_play_together::session::host::HOST_PEER_ID;

/// Longest ROM file considered when looking for another player's ROM by hash.
const MAX_ROM_BYTES: u64 = 64 << 20;

/// Longest a snapshot request stays "in flight" for the status before it counts as unanswered.
const SNAPSHOT_REQUEST_GRACE: Duration = Duration::from_secs(10);

/// A follower idles between frames whenever it has caught up; only a lull this long counts as
/// "waiting" for the status (the other player paused, or the network is behind).
const WAITING_AFTER: Duration = Duration::from_millis(400);

/// What this player is in the session.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum PlayTogetherRole {
    /// Joining: the host has not answered yet.
    Connecting,
    /// Hosting.
    Host,
    /// Joined somebody else's session.
    Client
}

impl PlayTogetherRole {
    fn as_str(self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Host => "host",
            Self::Client => "client"
        }
    }
}

/// Where another player's game stands on this machine.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum PeerStatus {
    /// Their ROM is not on this machine (or not known); the user must locate it.
    NeedsRom,
    /// The core is built and waiting for their first state.
    Starting,
    /// Running in step.
    Following,
    /// Nothing to run: they are paused, or the network is behind.
    Waiting,
    /// A fresh state was asked for and has not arrived yet.
    Resyncing,
    /// Their stream ended (they left, or the session did).
    Ended,
    /// Cannot follow them (see the status text).
    Error
}

impl PeerStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::NeedsRom => "needs_rom",
            Self::Starting => "starting",
            Self::Following => "following",
            Self::Waiting => "waiting",
            Self::Resyncing => "resyncing",
            Self::Ended => "ended",
            Self::Error => "error"
        }
    }
}

/// The replay file another player's game is written to.
struct PeerReplayFile {
    name: String,
    final_path: PathBuf,
    temp_path: PathBuf,
    /// How many frames their follower had emulated when the file started, so a file that never
    /// got a frame can be told apart from a following that had run before it.
    frames_at_start: u64
}

/// Another player in the session and, when their ROM is on this machine, the core following
/// their game.
pub struct PeerInstance {
    pub peer_id: PeerId,
    pub name: String,
    pub info: ParticipantInfo,
    pub emulator_type: Option<SuperShuckieEmulatorType>,
    pub status: PeerStatus,
    pub status_text: String,
    core: Option<ThreadedSuperShuckieCore>,
    snapshot_requests: Option<Receiver<SnapshotRequestReason>>,
    audio: Option<Arc<AudioOutput>>,
    replay: Option<PeerReplayFile>,
    local_rom_path: Option<PathBuf>,
    last_stats: ElapsedTimeStats,
    last_follow: FollowerStatsSnapshot,
    fps_window: Option<(Instant, u64)>,
    fps: f64,
    video_scale: NonZeroU8,
    /// When a snapshot was last asked for on their behalf and not yet answered.
    snapshot_requested_at: Option<Instant>,
    /// When their game last advanced here.
    last_advance: Instant,
    /// Whether the window for this peer should be shown (the UI's hint; not persisted).
    pub window_hidden: bool,
    /// The UDP port their game is served to Poke-A-Byte on, while it is.
    pokeabyte_port: Option<u16>,
    /// The producer half of their follower's queue, kept so their stream can be subscribed to
    /// again after a link cable is unplugged.
    feeder: Option<LiveReplayFeeder>,
    /// Who they are linked with by cable, as the host announces it.
    pub linked_with: Option<PeerId>
}

/// The whole session as the frontend sees it.
pub struct PlayTogetherSession {
    session: Arc<dyn Session>,
    role: PlayTogetherRole,
    code: String,
    local_peer_id: PeerId,
    local_name: String,
    local_color: PlayerColor,
    peers: Vec<PeerInstance>,
    publishing: bool,
    /// Who asked for the next snapshot (shared with the publisher tee, which picks the target).
    pending_requesters: Arc<Mutex<Vec<PeerId>>>,
    /// The race-start countdown in progress: its id and deadline.
    pending_reset: Option<(u32, Instant)>,
    /// Sync pause as the session has it: the host's setting (a client learns it from the host).
    sync_pause: bool,
    /// The pause state the session last agreed on, while sync pause is on. The local game
    /// pausing or unpausing away from it is what gets published; adopting a remote pause moves
    /// it, so nothing echoes.
    synced_paused: bool,
    /// Who paused everyone (the local player included), while sync pause holds the game paused.
    paused_by: Option<PeerId>,
    /// What we publish (console, ROM, core, BIOS), for compatibility checks.
    local_metadata: ReplayFileMetadata,
    /// The host's start state, while one is set: the save state everyone's own game was loaded
    /// from, and is loaded from again at a race start.
    start_state: Option<Arc<Vec<u8>>>,
    /// The session host's game speed, which every linked pair runs at (a client learns it from
    /// the host; the host's own is kept here as it changes).
    link_speed: supershuckie_core::Speed,
    /// Where this player stands with a link cable (see [`link`]).
    link: Option<link::LinkPhase>,
    /// Why the last link cable came out, or why a request came to nothing, for the UI.
    last_link_reason: String,
    /// Our last round-trip time to the host, in milliseconds (0 for the host itself): half the
    /// link cable's input delay estimate.
    local_rtt_ms: u32,
    /// The next link request's nonce.
    next_link_nonce: u32,
    errors: Vec<String>,
    generation: u64
}

impl PlayTogetherSession {
    fn bump(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    fn peer_mut(&mut self, peer_id: PeerId) -> Option<&mut PeerInstance> {
        self.peers.iter_mut().find(|p| p.peer_id == peer_id)
    }

    /// A participant's name for the UI: ours, a peer's, or a placeholder for someone gone.
    fn name_of(&self, peer_id: PeerId) -> String {
        if peer_id == self.local_peer_id {
            return self.local_name.clone()
        }
        self.peers.iter().find(|p| p.peer_id == peer_id).map(|p| p.name.clone()).unwrap_or_else(|| format!("player {peer_id}"))
    }

    fn note_error(&mut self, error: String) {
        if self.errors.len() < 16 {
            self.errors.push(error);
        }
        self.bump();
    }
}

/// The publisher tee: what the local core emits goes to the session's outbound queue, one
/// message per emulated frame.
struct SessionPublisher {
    handle: PublisherHandle,
    requesters: Arc<Mutex<Vec<PeerId>>>,
    /// This frame's packets so far.
    pending: Vec<u8>,
    /// The publisher's frame count before the first `NextFrame` in `pending`.
    first_frame: u64,
    /// The publisher's frame count after the last `NextFrame` published or snapshotted.
    frame: u64,
    last_millis: Option<u64>,
    errors: Vec<String>,
    ended: bool
}

impl SessionPublisher {
    fn note(&mut self, result: Result<(), PublishError>) {
        if let Err(e) = result && self.errors.len() < 8 {
            self.errors.push(format!("{e}"));
        }
    }

    fn append(&mut self, packet: &Packet) {
        if !self.ended {
            append_packet(packet, &mut self.pending);
        }
    }
}

impl StreamPublisherFns for SessionPublisher {
    fn snapshot(&mut self, metadata: KeyframeMetadata, state: Vec<u8>) {
        if self.ended {
            return
        }
        // Everything pending belongs to the frame after this snapshot; it stays pending.
        self.frame = metadata.elapsed_frames;
        self.first_frame = metadata.elapsed_frames;
        self.last_millis = Some(metadata.elapsed_millis.0);
        let requesters = core::mem::take(&mut *self.requesters.lock().unwrap_or_else(|p| p.into_inner()));
        let target = match requesters.as_slice() {
            [one] => *one,
            _ => 0
        };
        let snapshot = SnapshotData {
            frame: metadata.elapsed_frames,
            elapsed_millis: metadata.elapsed_millis.0,
            input: metadata.input,
            speed: metadata.speed,
            counters: metadata.counters.into_iter().map(|c| (c.name, c.value)).collect(),
            state
        };
        let result = self.handle.publish_snapshot(snapshot, target);
        self.note(result);
    }

    fn next_frame(&mut self, timestamp_millis: TimestampMillis) {
        if self.ended {
            return
        }
        let delta = timestamp_millis.0.saturating_sub(self.last_millis.unwrap_or(timestamp_millis.0));
        self.last_millis = Some(timestamp_millis.0);
        self.append(&Packet::NextFrame { timestamp_delta: TimestampMillis(delta) });
        let bytes = core::mem::take(&mut self.pending);
        let result = self.handle.publish(self.first_frame, bytes);
        self.note(result);
        self.frame += 1;
        self.first_frame = self.frame;
    }

    fn set_input(&mut self, input: InputBuffer) {
        self.append(&Packet::ChangeInput { data: input });
    }

    fn write_memory(&mut self, address: UnsignedInteger, data: ByteVec) {
        self.append(&Packet::WriteMemory { address, data });
    }

    fn reset_console(&mut self) {
        self.append(&Packet::ResetConsole);
    }

    fn load_save_state(&mut self, state: ByteVec) {
        self.append(&Packet::LoadSaveState { state });
    }

    fn change_counter(&mut self, name: String, delta: SignedInteger) {
        self.append(&Packet::IncrementCounter { name, delta });
    }

    fn serial_in(&mut self, data: ByteVec) {
        self.append(&Packet::SerialIn { data });
    }

    fn sync_hash(&mut self, frame: UnsignedInteger, hash: [u8; 32]) {
        if self.ended {
            return
        }
        let result = self.handle.publish_sync_hash(frame, hash);
        self.note(result);
    }

    fn end(&mut self) {
        self.ended = true;
        self.pending.clear();
    }

    fn poll_errors(&mut self) -> Vec<String> {
        core::mem::take(&mut self.errors)
    }
}

/// The follower side: what arrives from the network goes straight into the follower core's
/// queue, on the reader thread.
struct FeederSink {
    feeder: LiveReplayFeeder
}

impl FollowerSink for FeederSink {
    fn packets(&mut self, _first_frame: u64, packets: Vec<Packet>) {
        for packet in packets {
            self.feeder.push_packet(packet);
        }
    }

    fn snapshot(&mut self, snapshot: SnapshotData) {
        self.feeder.push_packet(Packet::Keyframe {
            metadata: KeyframeMetadata {
                input: snapshot.input,
                speed: snapshot.speed,
                elapsed_frames: snapshot.frame,
                elapsed_millis: TimestampMillis(snapshot.elapsed_millis),
                counters: snapshot.counters.into_iter().map(|(name, value)| Counter { name, value }).collect()
            },
            state: ByteVec::Heap(snapshot.state)
        });
    }

    fn sync_hash(&mut self, frame: u64, hash: [u8; 32]) {
        self.feeder.push_sync_hash(frame, hash);
    }

    fn ended(&mut self, _reason: LeaveReason) {
        self.feeder.end();
    }
}

/// A participant, for the UI.
#[derive(Serialize, Clone, Debug)]
pub struct PeerView {
    pub peer_id: PeerId,
    pub name: String,
    /// The player's colour: palette index, name and `#rrggbb`.
    pub color: PlayerColor,
    pub color_name: String,
    pub color_rgb: String,
    pub rom_name: String,
    pub console: String,
    pub status: &'static str,
    pub status_text: String,
    pub frames_behind: u64,
    pub waiting: bool,
    pub snapshots_applied: u64,
    pub hash_mismatches: u64,
    pub fps: f64,
    pub elapsed_frames: u64,
    pub elapsed_ms: u64,
    pub counters: BTreeMap<String, SignedInteger>,
    pub replay_file: Option<String>,
    pub video_scale: u8,
    pub audio: bool,
    pub window_hidden: bool,
    /// The UDP port their game is served to Poke-A-Byte on (`null` while it is not).
    pub pokeabyte_port: Option<u16>,
    /// Who they are linked with by cable (`null` when nobody).
    pub linked_with: Option<PeerId>,
    /// Whether a link cable could be plugged into their game right now.
    pub can_link: bool
}

/// The session, for the UI.
#[derive(Serialize, Clone, Debug)]
pub struct PlayTogetherStateView {
    pub active: bool,
    pub role: &'static str,
    pub code: String,
    pub local_name: String,
    /// Our colour as the session knows it (0 until connected, then a palette index).
    pub local_color: PlayerColor,
    pub local_color_name: String,
    pub local_color_rgb: String,
    pub local_peer_id: PeerId,
    pub reset_countdown_ms: u32,
    pub save_peer_replays: bool,
    /// Whether a friend's game is being written to a replay file here (see each participant's
    /// `replay_file`).
    pub recording_peers: bool,
    /// Whether one player's pause pauses everyone: the host's setting while in a session, the
    /// local setting (for hosting) otherwise.
    pub sync_pause: bool,
    /// Who paused everyone, while sync pause holds the game paused: a player's name, `"you"`,
    /// or empty.
    pub paused_by: String,
    /// Whether the host's start state is set (everyone's game was loaded from it, and a race
    /// start loads it again instead of resetting the console).
    pub start_state: bool,
    /// The link cable.
    pub link: LinkView,
    pub participants: Vec<PeerView>,
    pub errors: Vec<String>
}

/// A colour's name and `#rrggbb` for the UI (empty strings for 0 / unknown).
fn describe_color(color: PlayerColor) -> (String, String) {
    match color_entry(color) {
        Some(entry) => (entry.name.to_owned(), format!("#{:06X}", entry.rgb)),
        None => (String::new(), String::new())
    }
}

/// The display name as other players will see it: trimmed, without control characters, at most
/// [`PlayTogetherSettings::MAX_DISPLAY_NAME_BYTES`] bytes, never empty.
pub fn sanitize_display_name(requested: &str) -> String {
    let mut name: String = requested.trim().chars().filter(|c| !c.is_control()).collect();
    while name.len() > PlayTogetherSettings::MAX_DISPLAY_NAME_BYTES {
        name.pop();
    }
    let name = name.trim_end().to_owned();
    if name.is_empty() { String::from("Player") } else { name }
}

/// `text` as a file name component: the characters a user file name may not contain become
/// `_`, trailing dots and spaces go, and the result is at most 64 bytes and never empty.
pub fn sanitize_file_name_component(text: &str) -> String {
    let mut out: String = text.chars()
        .map(|c| if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c })
        .collect();
    while out.len() > 64 {
        out.pop();
    }
    let out = out.trim().trim_end_matches(['.', ' ']).to_owned();
    if out.is_empty() { String::from("Friend") } else { out }
}

/// `yyyy-mm-dd hh.mm.ss` in UTC (a stamp that sorts and is safe in a file name).
pub fn utc_stamp() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem / 60) % 60, rem % 60);
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02} {h:02}.{m:02}.{s:02}")
}

fn emulator_type_for_console(console: ReplayConsoleType) -> Option<SuperShuckieEmulatorType> {
    match console {
        ReplayConsoleType::GameBoy => Some(SuperShuckieEmulatorType::GameBoy),
        ReplayConsoleType::SuperGameBoy2 => Some(SuperShuckieEmulatorType::GameBoySGB2),
        ReplayConsoleType::GameBoyColor => Some(SuperShuckieEmulatorType::GameBoyColor),
        ReplayConsoleType::GameBoyAdvance => Some(SuperShuckieEmulatorType::GameBoyAdvance),
        ReplayConsoleType::NintendoDS => Some(SuperShuckieEmulatorType::NintendoDS),
        ReplayConsoleType::Unknown => None
    }
}

fn console_name(console: ReplayConsoleType) -> &'static str {
    match emulator_type_for_console(console) {
        Some(t) => t.name(),
        None => "Unknown"
    }
}

impl SuperShuckieFrontend {
    /// Whether a Play Together session is active (hosting, joining or joined).
    #[inline]
    pub fn is_play_together_active(&self) -> bool {
        self.play_together.is_some()
    }

    /// Changes whenever the roster, a participant's status, the countdown or the errors change;
    /// poll [`Self::play_together_state`] when it does.
    #[inline]
    pub fn play_together_generation(&self) -> u64 {
        self.play_together.as_ref().map(|s| s.generation).unwrap_or(0)
    }

    /// Everything the UI shows about the session.
    pub fn play_together_state(&self) -> PlayTogetherStateView {
        let Some(s) = self.play_together.as_ref() else {
            return PlayTogetherStateView {
                active: false,
                role: "none",
                code: String::new(),
                local_name: self.settings.play_together.display_name.clone(),
                local_color: COLOR_RANDOM,
                local_color_name: String::new(),
                local_color_rgb: String::new(),
                local_peer_id: 0,
                reset_countdown_ms: 0,
                save_peer_replays: self.settings.play_together.save_peer_replays,
                recording_peers: false,
                sync_pause: self.settings.play_together.sync_pause,
                paused_by: String::new(),
                start_state: false,
                link: LinkView::none(String::new()),
                participants: Vec::new(),
                errors: Vec::new()
            }
        };
        let (local_color_name, local_color_rgb) = describe_color(s.local_color);
        PlayTogetherStateView {
            active: true,
            role: s.role.as_str(),
            code: s.code.clone(),
            local_name: s.local_name.clone(),
            local_color: s.local_color,
            local_color_name,
            local_color_rgb,
            local_peer_id: s.local_peer_id,
            reset_countdown_ms: self.play_together_reset_countdown_ms(),
            save_peer_replays: self.settings.play_together.save_peer_replays,
            recording_peers: s.peers.iter().any(|p| p.replay.is_some() && p.core.is_some()),
            sync_pause: s.sync_pause,
            paused_by: match s.paused_by {
                Some(id) if s.sync_pause && s.synced_paused => if id == s.local_peer_id { String::from("you") } else { s.name_of(id) },
                _ => String::new()
            },
            start_state: s.start_state.is_some(),
            link: self.play_together_link_state(),
            participants: s.peers.iter().map(|p| {
                let (color_name, color_rgb) = describe_color(p.info.color);
                let can_link = self.link_precondition(s, p.peer_id).is_ok();
                PeerView {
                peer_id: p.peer_id,
                name: p.name.clone(),
                color: p.info.color,
                color_name,
                color_rgb,
                rom_name: p.info.publisher.metadata.rom_name.clone(),
                console: console_name(p.info.publisher.metadata.console_type).to_owned(),
                status: p.status.as_str(),
                status_text: p.status_text.clone(),
                frames_behind: p.last_follow.frames_behind,
                waiting: p.last_follow.waiting,
                snapshots_applied: p.last_follow.snapshots_applied,
                hash_mismatches: p.last_follow.hash_mismatches,
                fps: p.fps,
                elapsed_frames: p.last_stats.frames as u64,
                elapsed_ms: p.last_stats.milliseconds as u64,
                counters: p.core.as_ref().map(|c| c.get_replay_counters()).unwrap_or_default(),
                replay_file: p.replay.as_ref().map(|r| r.name.clone()),
                video_scale: p.video_scale.get(),
                audio: p.audio.is_some(),
                window_hidden: p.window_hidden,
                pokeabyte_port: p.pokeabyte_port,
                linked_with: p.linked_with,
                can_link
            }}).collect(),
            errors: s.errors.clone()
        }
    }

    /// Milliseconds until the race-start reset fires, or 0 when no countdown is running.
    pub fn play_together_reset_countdown_ms(&self) -> u32 {
        self.play_together.as_ref()
            .and_then(|s| s.pending_reset)
            .map(|(_, deadline)| deadline.saturating_duration_since(Instant::now()).as_millis().min(u32::MAX as u128) as u32)
            .unwrap_or(0)
    }

    fn play_together_local_participant(&self, display_name: &str, color: PlayerColor) -> Result<LocalParticipant, UTF8CString> {
        self.refuse_if_exporting()?;
        let Some(emulator_type) = self.emulator_type else {
            return Err("Load a game first.".into())
        };
        if emulator_type == SuperShuckieEmulatorType::NintendoDS && !self.settings.play_together.allow_nintendo_ds {
            return Err("Nintendo DS games are not supported in Play Together yet.".into())
        }
        if self.play_together.is_some() {
            return Err("Already in a Play Together session; leave it first.".into())
        }
        if self.current_replay.is_some() {
            return Err("Close the replay first: a replay's game cannot be played together.".into())
        }
        let rom_name = self.get_current_rom_name().expect("game running without a name").to_owned();
        let bios = self.get_bios_for_core(emulator_type);
        let metadata = ReplayFileMetadata {
            console_type: emulator_type.replay_console_type(),
            rom_name: rom_name.clone(),
            rom_filename: rom_name,
            rom_checksum: *self.core.rom_checksum(),
            bios_checksum: blake3_hash(&bios),
            emulator_core_name: self.core.core_name().to_owned(),
            patch_format: ReplayPatchFormat::Unpatched,
            patch_target_checksum: ReplayHeaderBlake3Hash::default(),
            crop_start: None,
            crop_end: None,
            timer_offset: None
        };
        let stats = self.core.get_elapsed_time();
        Ok(LocalParticipant {
            display_name: sanitize_display_name(display_name),
            color: if is_valid_color(color) { color } else { COLOR_RANDOM },
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            publisher: PublisherInfo {
                metadata,
                initial_input: InputBuffer::new(),
                speed: stats.speed,
                frame: stats.frames as u64
            }
        })
    }

    fn install_session(&mut self, session: Arc<dyn Session>, role: PlayTogetherRole, code: String, display_name: &str, color: PlayerColor, local_metadata: ReplayFileMetadata) {
        self.settings.play_together.display_name = sanitize_display_name(display_name);
        self.settings.play_together.color = if is_valid_color(color) { color } else { COLOR_RANDOM };
        self.mark_settings_dirty();
        self.play_together = Some(PlayTogetherSession {
            session,
            role,
            code,
            local_peer_id: 0,
            local_name: self.settings.play_together.display_name.clone(),
            local_color: COLOR_RANDOM,
            peers: Vec::new(),
            publishing: false,
            pending_requesters: Arc::new(Mutex::new(Vec::new())),
            pending_reset: None,
            sync_pause: false,
            synced_paused: false,
            paused_by: None,
            local_metadata,
            start_state: None,
            link_speed: supershuckie_core::Speed::default(),
            link: None,
            last_link_reason: String::new(),
            local_rtt_ms: 0,
            next_link_nonce: (SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(1) | 1),
            errors: Vec::new(),
            generation: 1
        });
    }

    /// Host a session on `port` (0 = the configured port) as `display_name` in `color` (a
    /// palette index; 0 = any). The current game is published from now on. Returns the code to
    /// share.
    pub fn play_together_host(&mut self, port: u16, display_name: &str, color: PlayerColor) -> Result<UTF8CString, UTF8CString> {
        let local = self.play_together_local_participant(display_name, color)?;
        let port = if port == 0 { self.settings.play_together.host_port } else { port };
        let config = HostConfig {
            bind_address: self.settings.play_together.bind_address.clone(),
            port,
            allow_nintendo_ds: self.settings.play_together.allow_nintendo_ds,
            ..HostConfig::default()
        };
        let local_metadata = local.publisher.metadata.clone();
        let session = HostSession::bind(config, local).map_err(|e| UTF8CString::from(format!("{e}")))?;
        let bound_port = session.local_addr().port();
        let code = JoinCode {
            host: probe_local_ip().map(|ip| ip.to_string()).unwrap_or_else(|| String::from("127.0.0.1")),
            port: bound_port
        }.format();
        self.settings.play_together.host_port = bound_port;
        self.install_session(Arc::new(session), PlayTogetherRole::Host, code.clone(), display_name, color, local_metadata);
        self.push_sync_pause_to_session();
        self.push_link_speed_to_session();
        Ok(code.into())
    }

    /// Join the session at `code` (`host:port`) as `display_name` in `color` (a palette index;
    /// 0 = any; the host gives another when it is taken). Returns at once; the outcome shows up
    /// in the state (role becomes `client`, or an error).
    pub fn play_together_join(&mut self, code: &str, display_name: &str, color: PlayerColor) -> Result<(), UTF8CString> {
        let local = self.play_together_local_participant(display_name, color)?;
        let join_code = JoinCode::parse(code).map_err(|e| UTF8CString::from(format!("{e}")))?;
        let local_metadata = local.publisher.metadata.clone();
        let session = ClientSession::connect(join_code.clone(), ClientConfig::default(), local);
        self.settings.play_together.last_join_code = join_code.format();
        self.install_session(Arc::new(session), PlayTogetherRole::Connecting, join_code.format(), display_name, color, local_metadata);
        Ok(())
    }

    /// Leave the session (or stop hosting it): other players' games are closed and their replay
    /// files finished; the local game keeps running.
    pub fn play_together_leave(&mut self) {
        let Some(mut session) = self.play_together.take() else {
            return
        };
        // The cable first: a lent follower core has to be back on its own thread before it is
        // shut down.
        self.unlink_cable(&mut session, supershuckie_play_together::UnlinkReason::Unplugged, true, String::from("You left the session."));
        if session.publishing {
            self.core.stop_stream_publishing();
        }
        for mut peer in core::mem::take(&mut session.peers) {
            session.session.unsubscribe(peer.peer_id);
            self.shutdown_peer(&mut peer);
        }
        session.session.leave();
    }

    /// Host only: reset everyone's console after `countdown_seconds` (a race start). Every
    /// participant, including this one, resets its own console when the countdown ends.
    pub fn play_together_reset_all(&mut self, countdown_seconds: u32) -> Result<(), UTF8CString> {
        let Some(s) = self.play_together.as_mut() else {
            return Err("Not in a Play Together session.".into())
        };
        if s.role != PlayTogetherRole::Host {
            return Err("Only the host can reset everyone.".into())
        }
        let countdown = Duration::from_secs(countdown_seconds.min(60) as u64);
        s.session.send_reset_all(countdown).map(|_| ()).map_err(|e| UTF8CString::from(format!("{e}")))
    }

    /// Start recording everyone's replay at once: this player's own, and a new file for every
    /// friend whose game is followed here (a file already being written for them is finished
    /// first, so every file starts now). Fails if the local recording cannot start; a friend's
    /// file failing is noted in the session's errors and named in the error, the rest go on.
    pub fn play_together_start_recording_everyone(&mut self) -> Result<(), UTF8CString> {
        let Some(mut session) = self.play_together.take() else {
            return Err("Not in a Play Together session.".into())
        };
        let outcome = (|| {
            self.start_recording_replay(None)?;
            let errors_before = session.errors.len();
            for index in 0..session.peers.len() {
                Self::finish_peer_replay(&mut session.peers[index]);
                // Their core is lent out for the call so the session can take the errors.
                let (core, path, name, metadata) = {
                    let peer = &mut session.peers[index];
                    let (Some(core), Some(path)) = (peer.core.take(), peer.local_rom_path.clone()) else {
                        continue
                    };
                    (core, path, peer.name.clone(), peer.info.publisher.metadata.clone())
                };
                let replay = self.start_peer_replay(&mut session, &core, &path, &name, &metadata);
                let peer = &mut session.peers[index];
                peer.core = Some(core);
                peer.replay = replay;
            }
            let failed: Vec<String> = session.errors[errors_before..].to_vec();
            if failed.is_empty() { Ok(()) } else { Err(UTF8CString::from(failed.join("\n"))) }
        })();
        session.bump();
        self.play_together = Some(session);
        outcome
    }

    /// Stop recording everyone's replay: this player's own and every friend's file being written.
    pub fn play_together_stop_recording_everyone(&mut self) -> Result<(), UTF8CString> {
        let Some(s) = self.play_together.as_mut() else {
            return Err("Not in a Play Together session.".into())
        };
        for peer in s.peers.iter_mut() {
            Self::finish_peer_replay(peer);
        }
        s.bump();
        self.stop_recording_replay()
    }

    /// Whether a friend's game is being written to a replay file here.
    pub fn play_together_is_recording_peers(&self) -> bool {
        self.play_together.as_ref().is_some_and(|s| s.peers.iter().any(|p| p.replay.is_some() && p.core.is_some()))
    }

    /// Whether one player's pause pauses everyone: the host's setting while in a session, the
    /// local setting (used when hosting) otherwise.
    pub fn get_play_together_sync_pause(&self) -> bool {
        match self.play_together.as_ref() {
            Some(s) if s.role != PlayTogetherRole::Connecting => s.sync_pause,
            _ => self.settings.play_together.sync_pause
        }
    }

    /// Set whether one player's pause pauses everyone. In a session only the host can: the
    /// setting is told to every client, and everyone adopts the host's current pause state.
    pub fn set_play_together_sync_pause(&mut self, enabled: bool) -> Result<(), UTF8CString> {
        if self.play_together.as_ref().is_some_and(|s| s.role != PlayTogetherRole::Host) {
            return Err("Only the host can change sync pause; it applies to everyone in the session.".into())
        }
        self.settings.play_together.sync_pause = enabled;
        self.mark_settings_dirty();
        self.push_sync_pause_to_session();
        Ok(())
    }

    /// Host only: tell the session (and every client) the sync-pause setting, with the host's
    /// current pause state as the one everyone adopts.
    fn push_sync_pause_to_session(&mut self) {
        let enabled = self.settings.play_together.sync_pause;
        let paused = self.is_paused();
        let Some(s) = self.play_together.as_mut() else {
            return
        };
        if s.role != PlayTogetherRole::Host {
            return
        }
        s.sync_pause = enabled;
        s.synced_paused = paused;
        // Our id, not `local_peer_id`: right after binding, `Connected` has not been handled yet.
        s.paused_by = (enabled && paused).then_some(HOST_PEER_ID);
        if let Err(e) = s.session.set_sync_pause(enabled, paused) {
            s.note_error(format!("Could not set sync pause: {e}"));
        }
        s.bump();
    }

    /// Host only: tell the session (and every client) the host's game speed, which every linked
    /// pair runs at. Nothing is sent when it has not changed.
    pub(crate) fn push_link_speed_to_session(&mut self) {
        let speed = self.current_speed;
        let Some(s) = self.play_together.as_mut() else {
            return
        };
        if s.role != PlayTogetherRole::Host || s.link_speed == speed {
            return
        }
        s.link_speed = speed;
        if let Err(e) = s.session.set_link_speed(speed) {
            s.note_error(format!("Could not set the link cable speed: {e}"));
        }
        s.bump();
    }

    /// Whether the game's speed is the session host's to set right now: a client with a link
    /// cable in runs its pair at the host's speed, and its own speed controls do nothing.
    pub(crate) fn link_speed_is_the_hosts(&self) -> bool {
        self.play_together.as_ref().is_some_and(|s| s.role != PlayTogetherRole::Host && s.link.as_ref().is_some_and(|l| l.is_plugged()))
    }

    /// Adopt a pause state the session agreed on (another player's pause, or the host's state
    /// on joining or when the setting changed), without publishing it back.
    fn adopt_synced_pause(&mut self, session: &mut PlayTogetherSession, paused: bool, by: PeerId) {
        self.set_paused(paused);
        session.synced_paused = self.is_paused();
        session.paused_by = paused.then_some(by);
        session.bump();
    }

    /// Whether the host's start state is set: everyone's game was loaded from the host's save
    /// state, a race start loads it again, and only players with the host's game may join.
    pub fn get_play_together_start_state(&self) -> bool {
        self.play_together.as_ref().is_some_and(|s| s.start_state.is_some())
    }

    /// Host only. Set: pause, take a save state, and have everyone's own game load it (they
    /// pause too); refused while someone in the session plays a different game (console, ROM,
    /// core or BIOS), since they could not load it. Clear: nobody's game changes, and a race
    /// start resets consoles again.
    pub fn set_play_together_start_state(&mut self, enabled: bool) -> Result<(), UTF8CString> {
        self.refuse_if_exporting()?;
        let Some(s) = self.play_together.as_ref() else {
            return Err("Not in a Play Together session.".into())
        };
        if s.role != PlayTogetherRole::Host {
            return Err("Only the host can set the start state; everyone starts from the host's save state.".into())
        }
        if !enabled {
            let s = self.play_together.as_mut().expect("checked above");
            s.start_state = None;
            if let Err(e) = s.session.set_start_state(None) {
                s.note_error(format!("Could not clear the start state: {e}"));
            }
            s.bump();
            return Ok(())
        }
        let mismatched: Vec<String> = s.peers.iter()
            .map(|p| (p, follow_compatibility(&s.local_metadata, &p.info.publisher.metadata)))
            .filter(|(_, compat)| *compat != FollowCompatibility::Ok)
            .map(|(p, compat)| describe_incompatibility(&compat, &p.name))
            .collect();
        if !mismatched.is_empty() {
            return Err(format!("Everyone must be playing your game to start from your save state.

{}", mismatched.join("
")).into())
        }
        // Paused first, so the state everyone gets is the one on the host's screen.
        self.set_paused(true);
        let state = Arc::new(self.core.create_save_state().ok_or_else(|| UTF8CString::from("The emulator did not produce a save state"))?);
        let rom_checksum = *self.core.rom_checksum();
        // The host's own game loads it too: a state that has been loaded is not byte for byte
        // the one that was saved (the emulator settles a few fields on loading), and everyone
        // else's game is at the loaded one.
        self.load_start_state(&state);
        let s = self.play_together.as_mut().expect("checked above");
        s.start_state = Some(Arc::clone(&state));
        if let Err(e) = s.session.set_start_state(Some(StartStateData { rom_checksum, state: state.to_vec() })) {
            s.note_error(format!("Could not send the start state: {e}"));
        }
        s.bump();
        Ok(())
    }

    /// Load the start state into the local game (kept in the save-state history, so it can be
    /// undone). Nothing while exporting, like a reset.
    fn load_start_state(&mut self, state: &Arc<Vec<u8>>) {
        if self.current_export.is_some() {
            return
        }
        self.push_save_state_history();
        self.core.load_save_state(state.to_vec());
    }

    /// Use the ROM at `path` for `peer` (after checking it is the same ROM they are playing),
    /// and start following them.
    pub fn play_together_locate_rom(&mut self, peer_id: PeerId, path: &Path) -> Result<(), UTF8CString> {
        let Some(mut session) = self.play_together.take() else {
            return Err("Not in a Play Together session.".into())
        };
        let result = (|| {
            let peer = session.peer_mut(peer_id).ok_or_else(|| UTF8CString::from("No such player."))?;
            let wanted = peer.info.publisher.metadata.rom_checksum;
            let data = std::fs::read(path).map_err(|e| UTF8CString::from(format!("Cannot read {}: {e}", path.display())))?;
            let hash = blake3_hash(&data);
            if hash != wanted {
                return Err(format!(
                    "{} is not the ROM {} is playing ({}).\n\n  This file: {}\n  Theirs:    {}",
                    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                    peer.name,
                    peer.info.publisher.metadata.rom_name,
                    blake3_hash_to_ascii(hash),
                    blake3_hash_to_ascii(wanted)
                ).into())
            }
            Ok((data, hash))
        })();
        let outcome = match result {
            Ok((data, hash)) => {
                self.remember_rom(hash, path);
                self.build_peer_core(&mut session, peer_id, &data, path.to_owned());
                Ok(())
            }
            Err(e) => Err(e)
        };
        session.bump();
        self.play_together = Some(session);
        outcome
    }

    /// Set the display scale of `peer`'s window, or of every window (and the default) with
    /// `None`.
    pub fn play_together_set_video_scale(&mut self, peer_id: Option<PeerId>, scale: NonZeroU8) {
        let scale = scale.min(NonZeroU8::new(PlayTogetherSettings::MAX_PEER_VIDEO_SCALE).unwrap());
        if peer_id.is_none() {
            self.settings.play_together.peer_video_scale = scale;
            self.mark_settings_dirty();
        }
        let Some(mut session) = self.play_together.take() else {
            return
        };
        for peer in session.peers.iter_mut() {
            if peer_id.is_none_or(|id| id == peer.peer_id) && peer.video_scale != scale {
                peer.video_scale = scale;
                if let Some(core) = peer.core.as_ref() {
                    let infos: Vec<ScreenInfo> = core.read_screens(|screens| {
                        screens.iter().map(|s| ScreenInfo { width: s.width, height: s.height, encoding: s.encoding }).collect()
                    });
                    self.callbacks.peer_change_video_mode(peer.peer_id, &infos, scale);
                }
            }
        }
        self.play_together = Some(session);
    }

    /// The default display scale of other players' windows.
    #[inline]
    pub fn play_together_video_scale(&self) -> NonZeroU8 {
        self.settings.play_together.peer_video_scale
    }

    /// Hear (or stop hearing) `peer`'s game.
    pub fn play_together_set_peer_audio_enabled(&mut self, peer_id: PeerId, enabled: bool) -> Result<(), UTF8CString> {
        let latency = self.settings.audio.latency_ms as u32;
        let mute_when_sped_up = self.settings.audio.mute_when_sped_up;
        let Some(s) = self.play_together.as_mut() else {
            return Err("Not in a Play Together session.".into())
        };
        let Some(peer) = s.peer_mut(peer_id) else {
            return Err("No such player.".into())
        };
        let Some(core) = peer.core.as_ref() else {
            return Err("That player's game is not running here.".into())
        };
        if enabled && peer.audio.is_none() {
            let ring = Arc::new(AudioOutput::new(latency));
            core.set_audio_output(Some(ring.clone()));
            core.set_audio_mute_when_sped_up(mute_when_sped_up);
            core.set_audio_enabled(true);
            peer.audio = Some(ring);
        }
        else if !enabled && peer.audio.is_some() {
            core.set_audio_enabled(false);
            core.set_audio_output(None);
            peer.audio = None;
        }
        s.bump();
        Ok(())
    }

    /// The ring `peer`'s audio goes to, once enabled.
    pub fn play_together_peer_audio_output(&self, peer_id: PeerId) -> Option<Arc<AudioOutput>> {
        self.play_together.as_ref()?.peers.iter().find(|p| p.peer_id == peer_id)?.audio.clone()
    }

    /// Remember that `peer`'s window is hidden (or shown), for the UI. Their screen is not
    /// uploaded while hidden; showing it again uploads the current one at the next tick.
    pub fn play_together_set_window_hidden(&mut self, peer_id: PeerId, hidden: bool) {
        if let Some(s) = self.play_together.as_mut() && let Some(p) = s.peer_mut(peer_id) && p.window_hidden != hidden {
            p.window_hidden = hidden;
            if !hidden {
                p.last_stats.screen_generation = p.last_stats.screen_generation.wrapping_sub(1);
            }
            s.bump();
        }
    }

    /// Paths that may hold another player's ROM (the UI's favourites), tried after the recent
    /// ROMs when looking for one by hash.
    pub fn play_together_add_rom_candidates(&mut self, paths: Vec<PathBuf>) {
        self.play_together_rom_candidates.extend(paths);
        self.play_together_rom_candidates.dedup();
    }

    #[inline]
    pub fn get_play_together_save_peer_replays(&self) -> bool {
        self.settings.play_together.save_peer_replays
    }

    /// Whether other players' games are written to replay files (applies to players who join
    /// from now on).
    pub fn set_play_together_save_peer_replays(&mut self, save: bool) {
        self.settings.play_together.save_peer_replays = save;
        self.mark_settings_dirty();
    }

    #[inline]
    pub fn get_play_together_display_name(&self) -> &str {
        &self.settings.play_together.display_name
    }

    /// The colour last asked for (0 = any).
    #[inline]
    pub fn get_play_together_color(&self) -> PlayerColor {
        self.settings.play_together.color
    }

    #[inline]
    pub fn get_play_together_host_port(&self) -> u16 {
        self.settings.play_together.host_port
    }

    #[inline]
    pub fn get_play_together_last_join_code(&self) -> &str {
        &self.settings.play_together.last_join_code
    }

    /// The addresses other players on the same network can reach this machine at (the primary
    /// one only; the router's public address is not known here).
    pub fn play_together_local_addresses(&self) -> Vec<String> {
        probe_local_ip().map(|ip| vec![ip.to_string()]).unwrap_or_default()
    }

    /// The Play Together protocol version this build speaks.
    #[inline]
    pub fn play_together_protocol_version(&self) -> u32 {
        PROTOCOL_VERSION
    }

    /// Remember where a ROM with `hash` lives.
    pub(crate) fn remember_rom(&mut self, hash: ReplayHeaderBlake3Hash, path: &Path) {
        let Some(path) = path.to_str() else {
            return
        };
        let known = &mut self.settings.play_together.known_roms;
        known.insert(blake3_hash_to_ascii(hash), UTF8CString::from_str(path));
        while known.len() > PlayTogetherSettings::MAX_KNOWN_ROMS {
            let first = known.keys().next().cloned().expect("non-empty");
            known.remove(&first);
        }
        self.mark_settings_dirty();
    }

    /// A ROM on this machine whose blake3 is `hash`: a remembered path first, then the recent
    /// ROMs, then the UI's candidates. Files are re-hashed, so a changed or moved file is never
    /// trusted.
    pub fn find_rom_by_hash(&mut self, hash: &ReplayHeaderBlake3Hash) -> Option<(PathBuf, Vec<u8>)> {
        let key = blake3_hash_to_ascii(*hash);
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(known) = self.settings.play_together.known_roms.get(&key) {
            candidates.push(PathBuf::from(known.as_str()));
        }
        candidates.extend(self.settings.recent_roms.recent_roms.iter().map(|p| PathBuf::from(p.as_str())));
        candidates.extend(self.play_together_rom_candidates.iter().cloned());

        let mut stale_known = false;
        for (i, candidate) in candidates.iter().enumerate() {
            let Ok(meta) = std::fs::metadata(candidate) else {
                stale_known |= i == 0;
                continue
            };
            if !meta.is_file() || meta.len() > MAX_ROM_BYTES {
                continue
            }
            let Ok(data) = std::fs::read(candidate) else {
                continue
            };
            if blake3_hash(&data) == *hash {
                let path = candidate.clone();
                self.remember_rom(*hash, &path);
                return Some((path, data))
            }
            stale_known |= i == 0 && self.settings.play_together.known_roms.contains_key(&key);
        }
        if stale_known {
            self.settings.play_together.known_roms.remove(&key);
            self.mark_settings_dirty();
        }
        None
    }

    /// Start publishing the local game into the session (once connected).
    fn start_publishing(&mut self, session: &mut PlayTogetherSession) {
        if session.publishing {
            return
        }
        let publisher = SessionPublisher {
            handle: session.session.publisher(),
            requesters: session.pending_requesters.clone(),
            pending: Vec::new(),
            first_frame: 0,
            frame: 0,
            last_millis: None,
            errors: Vec::new(),
            ended: false
        };
        match self.core.start_stream_publishing(Box::new(publisher)) {
            Ok(()) => session.publishing = true,
            Err(e) => session.note_error(format!("Could not publish the game: {e}"))
        }
    }

    /// Re-attach the publisher after the local core was replaced (a reload of the same game):
    /// the followers get a fresh snapshot.
    pub(crate) fn republish_after_core_switch(&mut self) {
        let Some(mut session) = self.play_together.take() else {
            return
        };
        if session.publishing {
            session.publishing = false;
            self.start_publishing(&mut session);
        }
        self.play_together = Some(session);
    }

    fn add_peer(&mut self, session: &mut PlayTogetherSession, info: ParticipantInfo) {
        if info.peer_id == session.local_peer_id || session.peers.iter().any(|p| p.peer_id == info.peer_id) {
            return
        }
        let name = sanitize_display_name(&info.display_name);
        let emulator_type = emulator_type_for_console(info.publisher.metadata.console_type);
        let mut peer = PeerInstance {
            peer_id: info.peer_id,
            name,
            emulator_type,
            status: PeerStatus::NeedsRom,
            status_text: String::new(),
            core: None,
            snapshot_requests: None,
            audio: None,
            replay: None,
            local_rom_path: None,
            last_stats: ElapsedTimeStats::default(),
            last_follow: FollowerStatsSnapshot::default(),
            fps_window: None,
            fps: 0.0,
            video_scale: self.settings.play_together.peer_video_scale,
            snapshot_requested_at: None,
            last_advance: Instant::now(),
            window_hidden: false,
            pokeabyte_port: None,
            feeder: None,
            linked_with: None,
            info
        };
        match emulator_type {
            None => {
                peer.status = PeerStatus::Error;
                peer.status_text = String::from("Unknown console.");
            }
            Some(SuperShuckieEmulatorType::NintendoDS) if !self.settings.play_together.allow_nintendo_ds => {
                peer.status = PeerStatus::Error;
                peer.status_text = String::from("Nintendo DS games are not supported in Play Together yet.");
            }
            Some(_) => {}
        }
        let peer_id = peer.peer_id;
        let rom_checksum = peer.info.publisher.metadata.rom_checksum;
        session.peers.push(peer);
        session.bump();

        if matches!(session.peers.last().map(|p| p.status), Some(PeerStatus::NeedsRom)) {
            match self.find_rom_by_hash(&rom_checksum) {
                Some((path, data)) => self.build_peer_core(session, peer_id, &data, path),
                None => {
                    if let Some(p) = session.peer_mut(peer_id) {
                        p.status_text = format!("{} is not on this machine; locate it to follow {}.", p.info.publisher.metadata.rom_name, p.name);
                    }
                }
            }
        }
    }

    /// Build the core that follows `peer`'s game from `rom` (at `path`), subscribe to their
    /// stream and, if wanted, open their replay file.
    fn build_peer_core(&mut self, session: &mut PlayTogetherSession, peer_id: PeerId, rom: &[u8], path: PathBuf) {
        let save_replays = self.settings.play_together.save_peer_replays;
        let base_speed = self.settings.emulation.base_speed_multiplier;

        let Some(index) = session.peers.iter().position(|p| p.peer_id == peer_id) else {
            return
        };
        let (emulator_type, metadata, name) = {
            let peer = &session.peers[index];
            let Some(t) = peer.emulator_type else { return };
            (t, peer.info.publisher.metadata.clone(), peer.name.clone())
        };

        let outcome: Result<(ThreadedSuperShuckieCore, Receiver<SnapshotRequestReason>, Option<PeerReplayFile>, String, LiveReplayFeeder), String> = (|| {
            // Their BIOS if it is one this build carries, else this machine's; a mismatch is
            // reported by the attach below rather than silently desyncing.
            let bios = self.compute_builtin_bios_override(metadata.bios_checksum).unwrap_or_else(|| self.default_bios_for(emulator_type));
            let bios_note = if blake3_hash(&bios) != metadata.bios_checksum { String::from("BIOS differs from theirs.") } else { String::new() };
            let core = self.make_new_core_with_bios(rom, None, emulator_type, bios, false).map_err(|e| e.to_string())?;
            let core = ThreadedSuperShuckieCore::new_with_role(core, CoreThreadRole::Follower);
            core.set_audio_enabled(false);
            core.set_audio_output(None);
            core.set_speed(supershuckie_core::Speed::from_multiplier_float(base_speed));
            core.set_ignore_speed_changes_in_replay(true);

            let stats = Arc::new(supershuckie_core::live_replay::FollowerStats::default());
            let (upstream, requests) = channel();
            let (feeder, source) = live_replay_channel(stats, upstream);
            let mut core = core;
            core.attach_live_replay_source(source, metadata.clone(), false).map_err(|e| e.to_string())?;

            let replay = if save_replays { self.start_peer_replay(session, &core, &path, &name, &metadata) } else { None };

            session.session.subscribe(peer_id, Box::new(FeederSink { feeder: feeder.clone() })).map_err(|e| e.to_string())?;
            core.start();
            Ok((core, requests, replay, bios_note, feeder))
        })();

        let scale = session.peers[index].video_scale;
        let serve_pokeabyte = self.settings.pokeabyte.enabled && self.settings.pokeabyte.serve_friends;
        let base_port = self.settings.pokeabyte.port;
        let peer = &mut session.peers[index];
        match outcome {
            Ok((core, requests, replay, note, feeder)) => {
                let infos: Vec<ScreenInfo> = core.read_screens(|screens| {
                    screens.iter().map(|s| ScreenInfo { width: s.width, height: s.height, encoding: s.encoding }).collect()
                });
                peer.core = Some(core);
                peer.snapshot_requests = Some(requests);
                peer.feeder = Some(feeder);
                peer.replay = replay;
                peer.local_rom_path = Some(path);
                peer.status = PeerStatus::Starting;
                peer.status_text = note;
                peer.snapshot_requested_at = Some(Instant::now());
                self.callbacks.peer_change_video_mode(peer_id, &infos, scale);
                if serve_pokeabyte {
                    Self::serve_peer_to_pokeabyte(session, index, base_port, true);
                }
            }
            Err(e) => {
                peer.status = PeerStatus::Error;
                peer.status_text = e;
            }
        }
        session.bump();
    }

    /// Serve (or stop serving) the game of the peer at `index` to Poke-A-Byte. The port is the
    /// lowest one above `base_port` (the player's own) that no other friend holds and that can be
    /// bound. A failure is noted in the session's errors, not returned: their game runs either way.
    fn serve_peer_to_pokeabyte(session: &mut PlayTogetherSession, index: usize, base_port: u16, serve: bool) {
        if session.peers[index].core.is_none() {
            return
        }
        if !serve {
            let peer = &mut session.peers[index];
            if peer.pokeabyte_port.take().is_some() {
                let _ = peer.core.as_ref().expect("checked above").set_pokeabyte_port(None);
                session.bump();
            }
            return
        }
        if session.peers[index].pokeabyte_port.is_some() {
            return
        }

        let taken: Vec<u16> = session.peers.iter().filter_map(|p| p.pokeabyte_port).collect();
        let mut last_error = String::from("no port left");
        let mut bound = None;
        {
            let core = session.peers[index].core.as_ref().expect("checked above");
            for offset in 1..=PokeAByteSettings::MAX_FRIEND_PORTS {
                let Some(port) = base_port.checked_add(offset) else { break };
                if taken.contains(&port) {
                    continue
                }
                match core.set_pokeabyte_port(Some(port)) {
                    Ok(()) => {
                        bound = Some(port);
                        break
                    }
                    Err(e) => last_error = e
                }
            }
        }
        match bound {
            Some(port) => {
                session.peers[index].pokeabyte_port = Some(port);
                session.bump();
            }
            None => {
                let name = session.peers[index].name.clone();
                session.note_error(format!("{name}'s game could not be served to Poke-A-Byte: {last_error}"));
            }
        }
    }

    /// Serve (or stop serving) `peer`'s game to Poke-A-Byte, regardless of the setting. Shown in
    /// the state as `pokeabyte_port`.
    pub fn play_together_set_peer_pokeabyte_enabled(&mut self, peer_id: PeerId, enabled: bool) -> Result<(), UTF8CString> {
        let base_port = self.settings.pokeabyte.port;
        let Some(s) = self.play_together.as_mut() else {
            return Err("Not in a Play Together session.".into())
        };
        let Some(index) = s.peers.iter().position(|p| p.peer_id == peer_id) else {
            return Err("No such player.".into())
        };
        if s.peers[index].core.is_none() {
            return Err("That player's game is not running here.".into())
        }
        let errors_before = s.errors.len();
        Self::serve_peer_to_pokeabyte(s, index, base_port, enabled);
        if s.errors.len() > errors_before {
            return Err(s.errors.last().expect("just noted").as_str().into())
        }
        Ok(())
    }

    /// The port `peer`'s game is served to Poke-A-Byte on, while it is.
    pub fn play_together_peer_pokeabyte_port(&self, peer_id: PeerId) -> Option<u16> {
        self.play_together.as_ref()?.peers.iter().find(|p| p.peer_id == peer_id)?.pokeabyte_port
    }

    /// `len` bytes at `address` of `peer`'s game as it runs here (`None` when it does not).
    /// A round trip to the thread running their core: for tools and tests, not for every tick.
    pub fn play_together_peer_read_ram(&self, peer_id: PeerId, address: u32, len: usize) -> Option<Vec<u8>> {
        self.play_together.as_ref()?.peers.iter().find(|p| p.peer_id == peer_id)?.core.as_ref()?.read_ram(address, len)
    }

    /// Bring every friend's game in line with the Poke-A-Byte settings (after they change): serve
    /// the ones that are not served yet, or stop serving all of them.
    pub(crate) fn play_together_apply_pokeabyte_setting(&mut self) {
        let serve = self.settings.pokeabyte.enabled && self.settings.pokeabyte.serve_friends;
        let base_port = self.settings.pokeabyte.port;
        let Some(s) = self.play_together.as_mut() else {
            return
        };
        for index in 0..s.peers.len() {
            Self::serve_peer_to_pokeabyte(s, index, base_port, serve);
        }
    }

    /// Create `<friend> - <UTC stamp>.replay` (and its temp sibling) in the replays folder of
    /// the local copy of their ROM.
    fn open_peer_replay_file(&mut self, rom_path: &Path, friend: &str) -> Result<(PeerReplayFile, File, File), String> {
        let rom_filename = rom_path.file_name().and_then(|n| n.to_str()).ok_or_else(|| String::from("the ROM path has no file name"))?.to_owned();
        self.create_userdata_for_rom(&rom_filename).map_err(|e| e.to_string())?;
        let dir = self.get_replays_dir_for_rom(&rom_filename);
        let base = format!("{} - {}", sanitize_file_name_component(friend), utc_stamp());
        for attempt in 0..100u32 {
            let name = if attempt == 0 { base.clone() } else { format!("{base} ({})", attempt + 1) };
            let final_path = dir.join(format!("{name}.{REPLAY_EXTENSION}"));
            let temp_path = dir.join(format!("temp-{name}.{REPLAY_EXTENSION}"));
            let final_file = match File::create_new(&final_path) {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("cannot create {}: {e}", final_path.display()))
            };
            let temp_file = match File::create_new(&temp_path) {
                Ok(f) => f,
                Err(e) => {
                    let _ = std::fs::remove_file(&final_path);
                    return Err(format!("cannot create {}: {e}", temp_path.display()))
                }
            };
            return Ok((PeerReplayFile { name: format!("{name}.{REPLAY_EXTENSION}"), final_path, temp_path, frames_at_start: 0 }, final_file, temp_file))
        }
        Err(String::from("too many replay files with that name"))
    }

    /// Start writing `core` (following `friend`'s game, whose ROM is at `rom_path`) to a replay
    /// file of its own, from now. A failure is noted in the session's errors, not returned: their
    /// game runs either way.
    fn start_peer_replay(&mut self, session: &mut PlayTogetherSession, core: &ThreadedSuperShuckieCore, rom_path: &Path, friend: &str, metadata: &ReplayFileMetadata) -> Option<PeerReplayFile> {
        let (mut file, final_file, temp_file) = match self.open_peer_replay_file(rom_path, friend) {
            Ok(opened) => opened,
            Err(e) => {
                session.note_error(format!("{friend}'s replay file could not be created: {e}"));
                return None
            }
        };
        let partial = PartialReplayRecordMetadata {
            rom_name: metadata.rom_name.clone(),
            rom_filename: metadata.rom_filename.clone(),
            settings: self.recorder_settings(),
            patch_format: ReplayPatchFormat::Unpatched,
            patch_target_checksum: ReplayHeaderBlake3Hash::default(),
            patch_data: ByteVec::default(),
            frames_per_keyframe: self.settings.replay.frames_per_keyframe,
            final_file: BufWriter::with_capacity(8 * 1024 * 1024, final_file),
            temp_file: BufWriter::with_capacity(8 * 1024 * 1024, temp_file)
        };
        file.frames_at_start = core.follower_stats().map(|s| s.emulated_frames).unwrap_or(0);
        match core.start_recording_follower_replay(partial, metadata.clone()) {
            Ok(()) => Some(file),
            Err(e) => {
                let _ = std::fs::remove_file(&file.final_path);
                let _ = std::fs::remove_file(&file.temp_path);
                session.note_error(format!("{friend}'s replay file could not be started: {e}"));
                None
            }
        }
    }

    /// Finish `peer`'s replay file while their game goes on being followed. A file that never
    /// got a frame is deleted.
    fn finish_peer_replay(peer: &mut PeerInstance) {
        let Some(file) = peer.replay.take() else {
            return
        };
        let Some(core) = peer.core.as_ref() else {
            return
        };
        let frames = core.follower_stats().map(|s| s.emulated_frames).unwrap_or(0);
        if core.stop_recording_replay() {
            Self::clean_up_peer_replay(&file, frames);
        }
    }

    /// Delete the finished file's temp sibling and, when nothing was recorded, the file itself.
    fn clean_up_peer_replay(file: &PeerReplayFile, follower_frames: u64) {
        let _ = std::fs::remove_file(&file.temp_path);
        if follower_frames <= file.frames_at_start {
            let _ = std::fs::remove_file(&file.final_path);
        }
    }

    /// Stop following `peer`: finish their replay file and drop their core.
    fn shutdown_peer(&mut self, peer: &mut PeerInstance) {
        let Some(core) = peer.core.take() else {
            return
        };
        let frames = core.follower_stats().map(|s| s.emulated_frames).unwrap_or(0);
        let alive = core.is_alive();
        core.detach_live_replay_source();
        drop(core);
        if let Some(file) = peer.replay.take() {
            if alive {
                Self::clean_up_peer_replay(&file, frames);
            }
        }
        peer.snapshot_requests = None;
        peer.feeder = None;
        peer.audio = None;
        peer.pokeabyte_port = None;
    }

    fn handle_session_event(&mut self, session: &mut PlayTogetherSession, event: SessionEvent) -> bool {
        match event {
            SessionEvent::Connected { local_peer_id, local_display_name, local_color, participants, .. } => {
                session.local_peer_id = local_peer_id;
                session.local_name = local_display_name;
                session.local_color = local_color;
                if session.role == PlayTogetherRole::Connecting {
                    session.role = PlayTogetherRole::Client;
                }
                session.bump();
                self.start_publishing(session);
                for info in participants {
                    self.add_peer(session, info);
                }
            }
            SessionEvent::Joined(info) => self.add_peer(session, info),
            SessionEvent::Left { peer_id, reason } => {
                if session.link.as_ref().is_some_and(|l| l.peer() == peer_id) {
                    let name = session.name_of(peer_id);
                    self.unlink_cable(session, supershuckie_play_together::UnlinkReason::PeerLeft, false, format!("{name} left the session."));
                }
                if let Some(index) = session.peers.iter().position(|p| p.peer_id == peer_id) {
                    let mut peer = session.peers.remove(index);
                    self.shutdown_peer(&mut peer);
                    session.note_error(format!("{} left ({reason}).", peer.name));
                }
            }
            SessionEvent::SnapshotRequested { requesters } => {
                session.pending_requesters.lock().unwrap_or_else(|p| p.into_inner()).extend(requesters);
                self.core.request_stream_snapshot();
            }
            SessionEvent::StreamOverrun { from, dropped_bytes } => {
                if let Some(p) = session.peer_mut(from) {
                    p.status_text = format!("Fell behind ({} MB dropped); resyncing.", dropped_bytes >> 20);
                }
                session.bump();
            }
            SessionEvent::ResetAll { race_id, deadline } => {
                session.pending_reset = Some((race_id, deadline));
                session.bump();
            }
            SessionEvent::SyncPauseChanged { enabled, paused } => {
                session.sync_pause = enabled;
                if enabled {
                    self.adopt_synced_pause(session, paused, HOST_PEER_ID);
                }
                else {
                    session.paused_by = None;
                    session.bump();
                }
            }
            SessionEvent::LinkSpeedChanged { speed } => {
                session.link_speed = speed;
                // The pair runs at the host's speed: applied right away while the cable is in.
                if session.link.as_ref().is_some_and(|l| l.is_plugged()) {
                    self.set_core_speed(speed);
                }
                session.bump();
            }
            // Only relayed by the host while sync pause is on, so it applies whether or not the
            // host's setting has been heard yet (a joiner's `SyncPauseChanged` follows it).
            SessionEvent::PauseChanged { from, paused } => self.adopt_synced_pause(session, paused, from),
            SessionEvent::StartStateChanged { state: Some(state) } => {
                if state.rom_checksum != *self.core.rom_checksum() {
                    session.note_error(String::from("The host sent a start state for a different ROM; ignored."));
                }
                else {
                    let state = Arc::new(state.state);
                    // Paused at the state, like the host; with sync pause on that is the state
                    // the session agreed on (the host paused too), not a pause to publish.
                    self.set_paused(true);
                    self.load_start_state(&state);
                    session.start_state = Some(state);
                    if session.sync_pause {
                        session.synced_paused = self.is_paused();
                        session.paused_by = Some(HOST_PEER_ID);
                    }
                    session.bump();
                }
            }
            SessionEvent::StartStateChanged { state: None } => {
                session.start_state = None;
                session.bump();
            }
            SessionEvent::RttUpdated { peer_id, rtt } => {
                if session.role == PlayTogetherRole::Client && peer_id == HOST_PEER_ID {
                    session.local_rtt_ms = rtt.as_millis().min(u32::MAX as u128) as u32;
                }
            }
            SessionEvent::Link(event) => self.handle_link_event(session, event),
            SessionEvent::Warning(text) => session.note_error(text),
            SessionEvent::Disconnected { reason } => {
                self.report_later(format!("Play Together: {reason}"));
                return false
            }
        }
        true
    }

    /// Service the session: roster and control events, the countdown, every follower's status
    /// and screen, their snapshot requests and replay errors. Called from `tick`.
    pub(crate) fn tick_play_together(&mut self, errors: &mut String) {
        let Some(mut session) = self.play_together.take() else {
            return
        };

        for event in session.session.poll_events() {
            if !self.handle_session_event(&mut session, event) {
                // Disconnected: the session is over.
                self.play_together = Some(session);
                self.play_together_leave();
                return
            }
        }

        if let Some((_, deadline)) = session.pending_reset && Instant::now() >= deadline {
            session.pending_reset = None;
            // With a start state set, the race starts from it rather than from power-on.
            match session.start_state.clone() {
                Some(state) => self.load_start_state(&state),
                None => self.hard_reset_console()
            }
            // Everyone unpauses by the protocol; nothing to publish.
            self.set_paused(false);
            session.synced_paused = self.is_paused();
            session.paused_by = None;
            session.bump();
        }

        // Sync pause: the local game pausing or unpausing (from the menu, a hotkey, a click, the
        // command server...) is published as a change from the state the session agreed on.
        // Read here once a tick rather than at every `set_paused`, so a pause-load-restore
        // sequence within one call is not two messages.
        if session.sync_pause && session.role != PlayTogetherRole::Connecting {
            let paused = self.is_paused();
            if paused != session.synced_paused {
                session.synced_paused = paused;
                session.paused_by = paused.then_some(session.local_peer_id);
                if let Err(e) = session.session.send_pause(paused) {
                    session.note_error(format!("Could not tell the others about the pause: {e}"));
                }
                session.bump();
            }
        }

        for e in self.core.get_stream_errors() {
            errors.push_str(&format!("- PLAY TOGETHER: {e}\n"));
        }

        self.tick_link(&mut session, errors);

        let now = Instant::now();
        for peer in session.peers.iter_mut() {
            let Some(core) = peer.core.as_mut() else {
                continue
            };
            if !core.is_alive() {
                peer.status = PeerStatus::Error;
                peer.status_text = String::from("Their emulator thread stopped.");
                self.shutdown_peer(peer);
                session.generation = session.generation.wrapping_add(1);
                continue
            }

            // Their screen (not uploaded while their window is closed: that is UI-thread work
            // nobody sees, taken from the player's own frames).
            let stats = core.get_elapsed_time();
            if stats.screen_generation != peer.last_stats.screen_generation && !peer.window_hidden {
                core.read_screens(|screens| self.callbacks.peer_refresh_screens(peer.peer_id, screens));
            }
            peer.last_stats = stats;

            // Their pace.
            let follow = core.follower_stats().unwrap_or_default();
            if follow.snapshots_applied > peer.last_follow.snapshots_applied {
                peer.snapshot_requested_at = None;
            }
            if follow.emulated_frames != peer.last_follow.emulated_frames {
                peer.last_advance = now;
            }
            peer.last_follow = follow;
            let frames = follow.emulated_frames;
            match peer.fps_window {
                Some((at, count)) => {
                    let elapsed = now.duration_since(at).as_secs_f64();
                    if elapsed >= 1.0 {
                        peer.fps = frames.saturating_sub(count) as f64 / elapsed;
                        peer.fps_window = Some((now, frames));
                    }
                }
                None => peer.fps_window = Some((now, frames))
            }

            // Their snapshot requests, forwarded upstream.
            if let Some(requests) = peer.snapshot_requests.as_ref() {
                let mut asked = false;
                while requests.try_recv().is_ok() {
                    asked = true;
                }
                if asked {
                    session.session.request_snapshot(peer.peer_id);
                    peer.snapshot_requested_at = Some(now);
                }
            }

            // Their replay file.
            for e in core.get_follower_errors() {
                errors.push_str(&format!("- PLAY TOGETHER ({}): {e}\n", peer.name));
            }
            for e in core.get_replay_recording_errors() {
                errors.push_str(&format!("- PLAY TOGETHER ({}): replay file: {e}\n", peer.name));
            }

            let status = if core.is_replay_playback_finished() {
                PeerStatus::Ended
            }
            else if follow.snapshots_applied == 0 {
                PeerStatus::Starting
            }
            else if peer.snapshot_requested_at.is_some_and(|at| now.duration_since(at) < SNAPSHOT_REQUEST_GRACE) {
                PeerStatus::Resyncing
            }
            else if follow.waiting && now.duration_since(peer.last_advance) >= WAITING_AFTER {
                PeerStatus::Waiting
            }
            else {
                PeerStatus::Following
            };
            if status != peer.status {
                peer.status = status;
                session.generation = session.generation.wrapping_add(1);
            }
        }

        self.play_together = Some(session);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_names_are_sanitised() {
        assert_eq!(sanitize_display_name("  Ash \n"), "Ash");
        assert_eq!(sanitize_display_name(""), "Player");
        assert_eq!(sanitize_display_name("\u{7}\u{8}"), "Player");
        let long = "a".repeat(100);
        assert_eq!(sanitize_display_name(&long).len(), PlayTogetherSettings::MAX_DISPLAY_NAME_BYTES);
        // Clamped on a character boundary.
        let wide = "é".repeat(40);
        let clamped = sanitize_display_name(&wide);
        assert!(clamped.len() <= PlayTogetherSettings::MAX_DISPLAY_NAME_BYTES);
        assert!(clamped.chars().all(|c| c == 'é'));
    }

    #[test]
    fn file_name_components_are_sanitised() {
        assert_eq!(sanitize_file_name_component("Ash: the/best?"), "Ash_ the_best_");
        assert_eq!(sanitize_file_name_component("dots..."), "dots");
        assert_eq!(sanitize_file_name_component(""), "Friend");
        assert_eq!(sanitize_file_name_component("\u{0}\u{1}"), "__");
    }

    #[test]
    fn utc_stamp_looks_like_a_date() {
        let stamp = utc_stamp();
        assert_eq!(stamp.len(), "2026-09-17 20.11.03".len(), "{stamp}");
        assert_eq!(&stamp[4..5], "-");
        assert_eq!(&stamp[10..11], " ");
        assert_eq!(&stamp[13..14], ".");
        let year: u32 = stamp[..4].parse().unwrap();
        assert!(year >= 2026);
    }

    #[test]
    fn consoles_map_to_emulator_types() {
        assert_eq!(emulator_type_for_console(ReplayConsoleType::GameBoyColor), Some(SuperShuckieEmulatorType::GameBoyColor));
        assert_eq!(emulator_type_for_console(ReplayConsoleType::Unknown), None);
        assert_eq!(console_name(ReplayConsoleType::GameBoyAdvance), "Game Boy Advance");
    }
}
