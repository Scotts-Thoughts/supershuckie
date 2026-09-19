//! Link cables between two emulated consoles running in the same process.
//!
//! A [`LinkPort`] is what a console core exposes when its serial hardware can be wired to another
//! core of the same family. The wire never leaves the process: the port's
//! [`step_linked`](LinkPort::step_linked) runs one step of its console with the partner reachable
//! from the console's serial callbacks, so a bit or a word sent by one side lands in the other
//! side's registers at the moment it is sent, exactly as SameBoy's own frontend links two
//! windows. Which console steps next is decided by the caller (see
//! [`second_runs_next`]) from the two ports' [`link_time`](LinkPort::link_time)s, which count the
//! consoles' own emulated cycles since they were plugged in, so that two machines stepping the
//! same pair from the same inputs produce the same interleave.
//!
//! Everything a console *received* over the cable is logged per frame and handed out by
//! [`take_serial_in`](LinkPort::take_serial_in) as the bytes of a `SerialIn` replay packet. A port
//! that is not live can be fed those bytes back with [`queue_serial_in`](LinkPort::queue_serial_in)
//! (the port is then in *replay* mode): the console then sees the same traffic at the same points
//! of its instruction stream without the partner. That is how a replay file of a linked game, and
//! a third player's follower of one, reproduce a trade or a battle on their own.
//!
//! The bytes are console-specific. The Game Boy format is [`GbSerialEvents`]; the Game Boy
//! Advance format is mGBA's lockstep driver log (see `mgba-rs`'s `interface.cpp`).

use crate::emulator::{EmulatorCore, RunTime};
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::{Display, Formatter};

/// Why a link operation failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkError {
    /// The partner is not a console this one can be linked to (another family, or no port).
    IncompatiblePartner,
    /// The port is not connected (or already is, for `connect`).
    NotConnected,
    /// The recorded serial events could not be parsed.
    BadSerialData(String),
    /// Both consoles of the pair are waiting on each other and neither can run.
    Deadlock,
    /// The console core failed.
    Emulator(String)
}

impl Display for LinkError {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::IncompatiblePartner => f.write_str("the other console cannot be linked to this one"),
            Self::NotConnected => f.write_str("the link port is not connected"),
            Self::BadSerialData(what) => write!(f, "the recorded link traffic is malformed: {what}"),
            Self::Deadlock => f.write_str("both linked consoles are waiting on each other"),
            Self::Emulator(what) => write!(f, "the emulator failed: {what}")
        }
    }
}

/// A console's serial port, as far as linking two cores in one process needs.
///
/// Modes: **off** (the console sees no cable; the default), **live** ([`connect`](Self::connect):
/// the console talks to the partner handed to each [`step_linked`](Self::step_linked) and logs
/// what it receives) and **replay** (the console is fed what a live console once received, from
/// [`queue_serial_in`](Self::queue_serial_in); it is entered on the first call to that function).
pub trait LinkPort {
    /// Wire the console's serial hardware to `partner` for exactly one step of this console, run
    /// that step, and unwire. `paced` says whether this is the console's own paced step (it may
    /// sleep to hold its frame rate) or an unpaced one.
    ///
    /// The partner is not run. It must be a console of the same family
    /// ([`LinkError::IncompatiblePartner`] otherwise) whose own port is live.
    fn step_linked(&mut self, partner: &mut dyn EmulatorCore, paced: bool) -> Result<RunTime, LinkError>;

    /// Emulated time since [`connect`](Self::connect), in the console's own link unit (Game Boy:
    /// 8 MHz cycles; Game Boy Advance: 16 MHz cycles). Only comparable between two consoles of
    /// the same family.
    fn link_time(&self) -> u64;

    /// Whether the console is parked waiting for its partner and must not be stepped until the
    /// partner wakes it (only the Game Boy Advance's lockstep coordinator does this).
    fn is_asleep(&self) -> bool {
        false
    }

    /// Plug the cable in: live mode, the receive log on, `link_time` restarted at zero. `first`
    /// says whether this console is the pair's first (the one the interleave favours on ties;
    /// the Game Boy Advance's lockstep makes it the clock owner).
    fn connect(&mut self, first: bool) -> Result<(), LinkError>;

    /// Pull the cable: the console sees no cable from here on (a game in the middle of a transfer
    /// handles that as on hardware). Leaves replay mode too.
    fn disconnect(&mut self);

    /// Whether the port is live (plugged into a partner).
    fn is_live(&self) -> bool;

    /// Everything received over the cable since the last call (or since the frame boundary the
    /// log was last taken at), as `SerialIn` packet bytes, appended to `into`; nothing is appended
    /// when nothing was received. Cleared by the call.
    fn take_serial_in(&mut self, into: &mut Vec<u8>);

    /// Queue recorded traffic for the frame about to run (playback and following). Puts the port
    /// in replay mode if it is off. Refuses malformed data without changing anything.
    fn queue_serial_in(&mut self, data: &[u8]) -> Result<(), LinkError>;

    /// Replay bookkeeping: events that could not be delivered where they were recorded (the
    /// console asked for a bit the recording did not have, or an event's moment had passed).
    /// A count above zero on a follower is a desync indicator.
    fn serial_replay_misses(&self) -> u64;
}

/// Which console of a linked pair steps next.
///
/// The console that is behind in its own emulated time steps, so the two timelines are merged
/// cycle by cycle (ties go to `first`); a console its lockstep coordinator has put to sleep never
/// steps until the other wakes it. `Err(Deadlock)` when both are asleep.
pub fn second_runs_next(first: &dyn LinkPort, second: &dyn LinkPort) -> Result<bool, LinkError> {
    match (first.is_asleep(), second.is_asleep()) {
        (true, true) => Err(LinkError::Deadlock),
        (true, false) => Ok(true),
        (false, true) => Ok(false),
        (false, false) => Ok(second.link_time() < first.link_time())
    }
}

// ---------------------------------------------------------------------------------------------
// Game Boy serial event log

/// One thing a Game Boy received over its link cable during a frame.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GbSerialEvent {
    /// A bit the console received while it was the transfer's master (its own clock): the value
    /// its `serial_transfer_bit_end` callback returned. Ordered; no timing needed, the console's
    /// own clock decides when.
    MasterBit(bool),
    /// A bit shifted into the console while it was the slave (the partner's clock), `cycles`
    /// 8 MHz cycles after the frame boundary (the console's link time at the step boundary the
    /// bit arrived at).
    SlaveBit {
        /// Link cycles since the frame boundary.
        cycles: u64,
        /// The bit.
        bit: bool
    },
    /// The cable was plugged in (`true`) or pulled (`false`).
    Connected(bool)
}

/// Tag bytes of the Game Boy `SerialIn` stream.
const TAG_MASTER: u8 = 0x01;
const TAG_SLAVE: u8 = 0x02;
const TAG_INFRARED: u8 = 0x03;
const TAG_CONNECTED: u8 = 0x04;

/// Most events one `SerialIn` packet may describe: a frame of the fastest transfer (the Game Boy
/// Color's 512 KHz clock) is under 9,000 bits.
const MAX_GB_EVENTS: usize = 65_536;

/// Encode and decode a frame's Game Boy events.
///
/// The bytes are a run of tagged groups, in event order:
///
/// ```text
/// 0x01  count varint  bits packed MSB first, count/8 rounded up bytes     master bits received
/// 0x02  count varint  (cycle_delta varint, bit u8)*                        slave bits received
/// 0x04  connected u8                                                       plug / unplug
/// 0x03  (reserved for infrared)
/// ```
///
/// Varints are LEB128. A slave bit's `cycle_delta` is relative to the previous slave bit of the
/// group (the first, to the frame boundary). Master and slave groups are emitted in the order
/// the events happened, so a run of master bits is followed by any slave bits that came after
/// them and so on; consecutive events of the same kind share a group.
pub struct GbSerialEvents;

impl GbSerialEvents {
    /// Append the encoding of `events` (in order) to `into`. Nothing is appended for no events.
    pub fn encode(events: &[GbSerialEvent], into: &mut Vec<u8>) {
        let mut i = 0;
        while i < events.len() {
            match events[i] {
                GbSerialEvent::MasterBit(_) => {
                    let start = i;
                    while i < events.len() && matches!(events[i], GbSerialEvent::MasterBit(_)) {
                        i += 1;
                    }
                    let bits = &events[start..i];
                    into.push(TAG_MASTER);
                    push_varint(into, bits.len() as u64);
                    let mut byte = 0u8;
                    for (n, event) in bits.iter().enumerate() {
                        let GbSerialEvent::MasterBit(bit) = event else { unreachable!() };
                        byte |= (*bit as u8) << (7 - (n % 8));
                        if n % 8 == 7 {
                            into.push(byte);
                            byte = 0;
                        }
                    }
                    if bits.len() % 8 != 0 {
                        into.push(byte);
                    }
                }
                GbSerialEvent::SlaveBit { .. } => {
                    let start = i;
                    while i < events.len() && matches!(events[i], GbSerialEvent::SlaveBit { .. }) {
                        i += 1;
                    }
                    let bits = &events[start..i];
                    into.push(TAG_SLAVE);
                    push_varint(into, bits.len() as u64);
                    let mut previous = 0u64;
                    for event in bits {
                        let GbSerialEvent::SlaveBit { cycles, bit } = event else { unreachable!() };
                        // Never negative in a well-formed log (events are in cycle order); a
                        // saturating delta keeps a malformed one decodable rather than panicking.
                        push_varint(into, cycles.saturating_sub(previous));
                        previous = *cycles;
                        into.push(*bit as u8);
                    }
                }
                GbSerialEvent::Connected(connected) => {
                    into.push(TAG_CONNECTED);
                    into.push(connected as u8);
                    i += 1;
                }
            }
        }
    }

    /// Decode a packet's bytes into events, in order. Refuses trailing or malformed data.
    pub fn decode(mut data: &[u8]) -> Result<Vec<GbSerialEvent>, LinkError> {
        let mut events = Vec::new();
        while let Some((&tag, rest)) = data.split_first() {
            data = rest;
            match tag {
                TAG_MASTER => {
                    let count = read_varint(&mut data)?;
                    if count as usize > MAX_GB_EVENTS || events.len() + count as usize > MAX_GB_EVENTS {
                        return Err(LinkError::BadSerialData(alloc::format!("{count} master bits in one frame")))
                    }
                    let bytes = (count as usize).div_ceil(8);
                    let Some((packed, rest)) = data.split_at_checked(bytes) else {
                        return Err(LinkError::BadSerialData(String::from("truncated master bits")))
                    };
                    data = rest;
                    for n in 0..count as usize {
                        events.push(GbSerialEvent::MasterBit(packed[n / 8] & (0x80 >> (n % 8)) != 0));
                    }
                }
                TAG_SLAVE => {
                    let count = read_varint(&mut data)?;
                    if count as usize > MAX_GB_EVENTS || events.len() + count as usize > MAX_GB_EVENTS {
                        return Err(LinkError::BadSerialData(alloc::format!("{count} slave bits in one frame")))
                    }
                    let mut cycles = 0u64;
                    for _ in 0..count {
                        let delta = read_varint(&mut data)?;
                        cycles = cycles.checked_add(delta).ok_or_else(|| LinkError::BadSerialData(String::from("slave bit cycle overflow")))?;
                        let Some((&bit, rest)) = data.split_first() else {
                            return Err(LinkError::BadSerialData(String::from("truncated slave bit")))
                        };
                        data = rest;
                        if bit > 1 {
                            return Err(LinkError::BadSerialData(alloc::format!("slave bit value {bit}")))
                        }
                        events.push(GbSerialEvent::SlaveBit { cycles, bit: bit != 0 });
                    }
                }
                TAG_CONNECTED => {
                    let Some((&connected, rest)) = data.split_first() else {
                        return Err(LinkError::BadSerialData(String::from("truncated connected flag")))
                    };
                    data = rest;
                    if connected > 1 {
                        return Err(LinkError::BadSerialData(alloc::format!("connected flag {connected}")))
                    }
                    events.push(GbSerialEvent::Connected(connected != 0));
                }
                TAG_INFRARED => return Err(LinkError::BadSerialData(String::from("infrared events are not supported by this version"))),
                other => return Err(LinkError::BadSerialData(alloc::format!("unknown event tag 0x{other:02X}")))
            }
        }
        Ok(events)
    }
}

/// Append `value` as a LEB128 varint.
pub fn push_varint(into: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            into.push(byte);
            return
        }
        into.push(byte | 0x80);
    }
}

/// Read a LEB128 varint, advancing `data`.
pub fn read_varint(data: &mut &[u8]) -> Result<u64, LinkError> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let Some((&byte, rest)) = data.split_first() else {
            return Err(LinkError::BadSerialData(String::from("truncated varint")))
        };
        *data = rest;
        if shift >= 64 || (shift == 63 && byte & 0x7E != 0) {
            return Err(LinkError::BadSerialData(String::from("varint too long")))
        }
        value |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Ok(value)
        }
        shift += 7;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_round_trip() {
        for value in [0u64, 1, 127, 128, 300, 16_383, 16_384, u32::MAX as u64, u64::MAX] {
            let mut bytes = Vec::new();
            push_varint(&mut bytes, value);
            let mut slice = bytes.as_slice();
            assert_eq!(read_varint(&mut slice).unwrap(), value);
            assert!(slice.is_empty());
        }
        let mut truncated: &[u8] = &[0x80, 0x80];
        assert!(read_varint(&mut truncated).is_err());
        let mut too_long: &[u8] = &[0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01];
        assert!(read_varint(&mut too_long).is_err());
    }

    #[test]
    fn gb_events_round_trip_in_order() {
        let events = alloc::vec![
            GbSerialEvent::Connected(true),
            GbSerialEvent::MasterBit(true),
            GbSerialEvent::MasterBit(false),
            GbSerialEvent::MasterBit(true),
            GbSerialEvent::MasterBit(true),
            GbSerialEvent::MasterBit(false),
            GbSerialEvent::MasterBit(false),
            GbSerialEvent::MasterBit(true),
            GbSerialEvent::MasterBit(false),
            GbSerialEvent::MasterBit(true),
            GbSerialEvent::SlaveBit { cycles: 12, bit: true },
            GbSerialEvent::SlaveBit { cycles: 12, bit: false },
            GbSerialEvent::SlaveBit { cycles: 4_000_000, bit: true },
            GbSerialEvent::MasterBit(false),
            GbSerialEvent::Connected(false),
        ];
        let mut bytes = Vec::new();
        GbSerialEvents::encode(&events, &mut bytes);
        assert_eq!(GbSerialEvents::decode(&bytes).unwrap(), events);

        // Nothing encodes to nothing, and nothing decodes to nothing.
        let mut empty = Vec::new();
        GbSerialEvents::encode(&[], &mut empty);
        assert!(empty.is_empty());
        assert!(GbSerialEvents::decode(&[]).unwrap().is_empty());
    }

    #[test]
    fn gb_events_refuse_malformed_data() {
        assert!(GbSerialEvents::decode(&[0x09]).is_err(), "unknown tag");
        assert!(GbSerialEvents::decode(&[TAG_MASTER, 9, 0xFF]).is_err(), "truncated master bits");
        assert!(GbSerialEvents::decode(&[TAG_SLAVE, 1, 5]).is_err(), "truncated slave bit");
        assert!(GbSerialEvents::decode(&[TAG_SLAVE, 1, 5, 2]).is_err(), "slave bit value 2");
        assert!(GbSerialEvents::decode(&[TAG_CONNECTED]).is_err(), "truncated connected flag");
        assert!(GbSerialEvents::decode(&[TAG_CONNECTED, 7]).is_err(), "connected flag 7");
        assert!(GbSerialEvents::decode(&[TAG_INFRARED, 0]).is_err(), "infrared is reserved");
        // An absurd count is refused before anything is allocated for it.
        let mut huge = alloc::vec![TAG_MASTER];
        push_varint(&mut huge, 1 << 40);
        assert!(GbSerialEvents::decode(&huge).is_err());
        let mut bits = alloc::vec![TAG_SLAVE];
        push_varint(&mut bits, 1 << 40);
        assert!(GbSerialEvents::decode(&bits).is_err());
    }
}
