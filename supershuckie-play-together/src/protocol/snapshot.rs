//! Snapshots: a publisher's full save state plus the metadata a follower needs to start
//! applying its stream at `frame`. Also the host's start state, a save state carried the same
//! way for everyone's own game to be loaded from.

use std::fmt;

use supershuckie_replay_recorder::{compress_data, decompress_data, InputBuffer, Speed};

use crate::error::DecodeError;
use crate::{Blake3Hash, PeerId};

/// Longest decompressed save state a snapshot may carry.
pub const MAX_STATE_LENGTH: u64 = 48 << 20;

/// zstd level used for snapshot states (fast; states compress well at any level).
pub const SNAPSHOT_ZSTD_LEVEL: i32 = 3;

/// A publisher's state at one frame, as the application sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct SnapshotData {
    /// The publisher's frame count at the snapshot. Streams that follow start here.
    pub frame: u64,
    /// The publisher's elapsed (emulated) milliseconds at the snapshot.
    pub elapsed_millis: u64,
    /// The input held at the snapshot.
    pub input: InputBuffer,
    /// The speed at the snapshot.
    pub speed: Speed,
    /// Counter values at the snapshot.
    pub counters: Vec<(String, i64)>,
    /// The raw (decompressed) save state.
    pub state: Vec<u8>,
}

/// How `Snapshot.state` is encoded on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum StateEncoding {
    /// `state` is the save state itself; `state.len() == state_len`.
    Raw = 0,
    /// `state` is one zstd frame that decompresses to `state_len` bytes.
    Zstd = 1,
}

impl TryFrom<u8> for StateEncoding {
    type Error = DecodeError;
    fn try_from(value: u8) -> Result<Self, DecodeError> {
        match value {
            0 => Ok(StateEncoding::Raw),
            1 => Ok(StateEncoding::Zstd),
            other => Err(DecodeError::BadEnum { what: "state encoding", value: u32::from(other) }),
        }
    }
}

/// A `Snapshot` message as it travels: the state still encoded.
#[derive(Clone, Debug, PartialEq)]
pub struct WireSnapshot {
    /// The publisher.
    pub from: PeerId,
    /// Who it is for; 0 = every other participant.
    pub target: PeerId,
    /// See [`SnapshotData::frame`].
    pub frame: u64,
    /// See [`SnapshotData::elapsed_millis`].
    pub elapsed_millis: u64,
    /// See [`SnapshotData::input`].
    pub input: InputBuffer,
    /// See [`SnapshotData::speed`].
    pub speed: Speed,
    /// See [`SnapshotData::counters`].
    pub counters: Vec<(String, i64)>,
    /// How `state` is encoded.
    pub encoding: StateEncoding,
    /// The decompressed length of the state.
    pub state_len: u64,
    /// The encoded state.
    pub state: Vec<u8>,
}

/// Compress a state as a snapshot carries it. `None` when zstd cannot (out of memory).
pub fn compress_state(state: &[u8]) -> Option<Vec<u8>> {
    compress_data(state, SNAPSHOT_ZSTD_LEVEL).ok()
}

/// Decode a state carried as `encoding`, `state_len`, `state`: `state_len` is checked against
/// [`MAX_STATE_LENGTH`] before anything is allocated, a zstd frame whose own header disagrees
/// with `state_len` is refused, and a raw state must be exactly `state_len` bytes.
fn decode_state(encoding: StateEncoding, state_len: u64, state: Vec<u8>) -> Result<Vec<u8>, DecodeError> {
    if state_len > MAX_STATE_LENGTH {
        return Err(DecodeError::StateTooLarge(state_len));
    }
    match encoding {
        StateEncoding::Raw => {
            if state.len() as u64 != state_len {
                return Err(DecodeError::BadState(format!("raw state is {} bytes but state_len says {}", state.len(), state_len)));
            }
            Ok(state)
        }
        StateEncoding::Zstd => decompress_data(&state, state_len as usize).map_err(|e| DecodeError::BadState(e.into_owned())),
    }
}

/// The host's start state: the save state every participant's own game is loaded from, as the
/// application sees it.
#[derive(Clone, PartialEq)]
pub struct StartStateData {
    /// The ROM it belongs to (the host's).
    pub rom_checksum: Blake3Hash,
    /// The raw (decompressed) save state.
    pub state: Vec<u8>,
}

impl fmt::Debug for StartStateData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StartStateData").field("rom_checksum", &self.rom_checksum).field("state_len", &self.state.len()).finish()
    }
}

/// A `StartState` message as it travels: the state still encoded. A `state_len` of 0 with an
/// empty `state` means the host cleared its start state.
#[derive(Clone, Debug, PartialEq)]
pub struct WireStartState {
    /// See [`StartStateData::rom_checksum`] (all zeros when cleared).
    pub rom_checksum: Blake3Hash,
    /// How `state` is encoded.
    pub encoding: StateEncoding,
    /// The decompressed length of the state.
    pub state_len: u64,
    /// The encoded state.
    pub state: Vec<u8>,
}

impl WireStartState {
    /// The message that clears the start state.
    pub fn cleared() -> WireStartState {
        WireStartState { rom_checksum: [0; 32], encoding: StateEncoding::Raw, state_len: 0, state: Vec::new() }
    }

    /// Wrap a start state with its state sent raw.
    pub fn raw(data: &StartStateData) -> WireStartState {
        WireStartState { rom_checksum: data.rom_checksum, encoding: StateEncoding::Raw, state_len: data.state.len() as u64, state: data.state.clone() }
    }

    /// Wrap a start state around an already-encoded state.
    pub fn with_state(data: &StartStateData, encoding: StateEncoding, state: Vec<u8>) -> WireStartState {
        WireStartState { rom_checksum: data.rom_checksum, encoding, state_len: data.state.len() as u64, state }
    }

    /// Whether this clears the start state.
    pub fn is_cleared(&self) -> bool {
        self.state_len == 0 && self.state.is_empty()
    }

    /// Decode the state (`None` when cleared); see [`WireSnapshot::into_snapshot`] for the checks.
    pub fn into_state(self) -> Result<Option<StartStateData>, DecodeError> {
        if self.is_cleared() {
            return Ok(None);
        }
        let state = decode_state(self.encoding, self.state_len, self.state)?;
        Ok(Some(StartStateData { rom_checksum: self.rom_checksum, state }))
    }
}

impl WireSnapshot {
    /// Wrap a snapshot with its state sent raw.
    pub fn raw(snapshot: &SnapshotData, from: PeerId, target: PeerId) -> WireSnapshot {
        Self::with_state(snapshot, from, target, StateEncoding::Raw, snapshot.state.clone())
    }

    /// Wrap a snapshot with its state zstd-compressed (raw if compression fails).
    pub fn compressed(snapshot: &SnapshotData, from: PeerId, target: PeerId) -> WireSnapshot {
        match compress_state(&snapshot.state) {
            Some(state) => Self::with_state(snapshot, from, target, StateEncoding::Zstd, state),
            None => Self::raw(snapshot, from, target),
        }
    }

    /// Wrap a snapshot around an already-encoded state.
    pub fn with_state(snapshot: &SnapshotData, from: PeerId, target: PeerId, encoding: StateEncoding, state: Vec<u8>) -> WireSnapshot {
        WireSnapshot {
            from,
            target,
            frame: snapshot.frame,
            elapsed_millis: snapshot.elapsed_millis,
            input: snapshot.input.clone(),
            speed: snapshot.speed,
            counters: snapshot.counters.clone(),
            encoding,
            state_len: snapshot.state.len() as u64,
            state,
        }
    }

    /// Decode the state: `state_len` is checked against [`MAX_STATE_LENGTH`] before anything is
    /// allocated, a zstd frame whose own header disagrees with `state_len` is refused, and a
    /// raw state must be exactly `state_len` bytes.
    pub fn into_snapshot(self) -> Result<SnapshotData, DecodeError> {
        let state = decode_state(self.encoding, self.state_len, self.state)?;
        Ok(SnapshotData {
            frame: self.frame,
            elapsed_millis: self.elapsed_millis,
            input: self.input,
            speed: self.speed,
            counters: self.counters,
            state,
        })
    }
}
