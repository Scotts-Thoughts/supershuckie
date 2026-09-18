//! Snapshots: a publisher's full save state plus the metadata a follower needs to start
//! applying its stream at `frame`.

use supershuckie_replay_recorder::{compress_data, decompress_data, InputBuffer, Speed};

use crate::error::DecodeError;
use crate::PeerId;

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
        if self.state_len > MAX_STATE_LENGTH {
            return Err(DecodeError::StateTooLarge(self.state_len));
        }
        let state = match self.encoding {
            StateEncoding::Raw => {
                if self.state.len() as u64 != self.state_len {
                    return Err(DecodeError::BadState(format!(
                        "raw state is {} bytes but state_len says {}",
                        self.state.len(),
                        self.state_len
                    )));
                }
                self.state
            }
            StateEncoding::Zstd => decompress_data(&self.state, self.state_len as usize).map_err(|e| DecodeError::BadState(e.into_owned()))?,
        };
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
