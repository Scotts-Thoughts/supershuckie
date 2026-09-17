use std::borrow::Cow;
use byteorder::{ByteOrder, LittleEndian};
use num_enum::TryFromPrimitive;
use tinyvec::ArrayVec;
use crate::PokeAByteError;
use crate::shared_memory::MAX_SHARED_MEMORY_LENGTH;

const PROTOCOL_VERSION: u8 = 1;

#[derive(Copy, Clone, PartialEq, Debug, TryFromPrimitive)]
#[repr(u8)]
pub enum Instruction {
    NoOp = 0,
    Ping = 1,
    Setup = 2,
    Write = 3,
    Freeze = 4,
    Unfreeze = 5,
    Close = 0xFF
}

pub const METADATA_HEADER_SIZE: usize = 32;

#[derive(Copy, Clone, Debug)]
pub struct MetadataHeader {
    #[expect(unused)]
    pub protocol_version: u8,
    pub instruction: Instruction,
    pub is_response: bool
}

impl MetadataHeader {
    pub const fn new_response(instruction: Instruction) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            instruction,
            is_response: true
        }
    }

    pub const fn into_bytes(self) -> [u8; METADATA_HEADER_SIZE] {
        [
            PROTOCOL_VERSION, 0, 0, 0, self.instruction as u8, self.is_response as u8, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0,
        ]
    }

    pub fn from_client_bytes(bytes: [u8; METADATA_HEADER_SIZE]) -> Result<Self, PokeAByteError> {
        let protocol_byte = bytes[0];
        if protocol_byte != PROTOCOL_VERSION {
            return Err(PokeAByteError::BadPacketFromClient { explanation: Cow::Owned(format!("Unknown protocol {protocol_byte} (expected {PROTOCOL_VERSION})")) })
        }

        let is_response = bytes[5];
        if is_response != 0 {
            return Err(PokeAByteError::BadPacketFromClient { explanation: Cow::Owned(format!("Bad IsResponse value {is_response}")) })
        }

        let instruction = bytes[4];
        let instruction = Instruction::try_from(instruction)
            .map_err(|_| PokeAByteError::BadPacketFromClient { explanation: Cow::Owned(format!("Bad Instruction {instruction}")) })?;

        Ok(Self {
            is_response: false,
            protocol_version: protocol_byte,
            instruction
        })
    }
}

const READ_BLOCK_SIZE: usize = 0xC;
pub const MAX_NUMBER_OF_READ_BLOCKS: usize = 128;

pub enum PokeAByteProtocolRequestPacket<'a> {
    NoOp,
    Ping,
    Setup {
        frame_skip: Option<u32>,
        blocks: ArrayVec<[PokeAByteProtocolRequestReadBlock; MAX_NUMBER_OF_READ_BLOCKS]>
    },
    Write {
        address: u64,
        data: &'a [u8]
    },
    Freeze {
        address: u64,
        data: &'a [u8]
    },
    Unfreeze {
        address: u64
    },
    Close,
}

#[derive(Default, Clone, PartialEq, Debug)]
pub struct PokeAByteProtocolRequestReadBlock {
    pub range: core::ops::Range<usize>,
    pub game_address: u32
}

impl<'a> PokeAByteProtocolRequestPacket<'a> {
    pub fn parse_bytes(bytes: &'a [u8]) -> Result<Self, PokeAByteError> {
        let Some(header) = bytes.get(..METADATA_HEADER_SIZE) else {
            return Err(PokeAByteError::BadPacketFromClient { explanation: Cow::Borrowed("too small to be header") })
        };
        let header_bytes: [u8; METADATA_HEADER_SIZE] = header.try_into().unwrap();
        let header = MetadataHeader::from_client_bytes(header_bytes)?;

        match header.instruction {
            Instruction::NoOp => Ok(Self::NoOp),
            Instruction::Ping => Ok(Self::Ping),
            Instruction::Setup => {
                let Some(_setup_data) = bytes.get(..0x20 + READ_BLOCK_SIZE * MAX_NUMBER_OF_READ_BLOCKS) else {
                    return Err(PokeAByteError::BadPacketFromClient { explanation: Cow::Borrowed("too small to be setup header") })
                };
                let block_count = LittleEndian::read_u32(&bytes[8..]) as usize;
                if block_count == 0 {
                    return Err(PokeAByteError::BadPacketFromClient { explanation: Cow::Borrowed("no read blocks") })
                }
                if block_count > MAX_NUMBER_OF_READ_BLOCKS {
                    return Err(PokeAByteError::BadPacketFromClient { explanation: Cow::Borrowed("too many read blocks") })
                }

                let frame_skip = u32::try_from(LittleEndian::read_i32(&bytes[12..])).ok();
                let blocks = (&bytes[32..])
                    .chunks_exact(0xC)
                    .take(block_count);

                let mut blocks_into = ArrayVec::new();

                for i in blocks {
                    let memory_map_file_offset: usize = LittleEndian::read_u32(&i[0..]) as usize;
                    let game_address = LittleEndian::read_u32(&i[4..]);
                    let length: usize = LittleEndian::read_u32(&i[8..]) as usize;

                    u32::try_from(length)
                        .ok()
                        .and_then(|i| i.checked_add(game_address))
                        .ok_or_else(|| PokeAByteError::BadPacketFromClient { explanation: Cow::Borrowed("length+address overflows") })?;

                    let end = length.checked_add(memory_map_file_offset)
                        .ok_or_else(|| PokeAByteError::BadPacketFromClient { explanation: Cow::Borrowed("length+offset overflows") })?;

                    // Cap the requested shared-memory size unconditionally: an unauthenticated UDP
                    // client should never be able to force an oversized allocation (M6).
                    if end > MAX_SHARED_MEMORY_LENGTH {
                        return Err(PokeAByteError::BadPacketFromClient { explanation: Cow::Borrowed("maximum shared memory size exceeded") });
                    }

                    let range = memory_map_file_offset .. end;

                    blocks_into.push(PokeAByteProtocolRequestReadBlock {
                        game_address, range
                    })
                }

                Ok(Self::Setup {
                    blocks: blocks_into,
                    frame_skip
                })
            },
            Instruction::Write => {
                let Some(_params) = bytes.get(0x8..0x14) else {
                    return Err(PokeAByteError::BadPacketFromClient { explanation: Cow::Borrowed("too small to be write header") })
                };

                let address = LittleEndian::read_u64(&bytes[0x8..]);
                let length: usize = LittleEndian::read_u32(&bytes[0x10..]) as usize;

                let Some(data) = bytes.get(0x20..) else {
                    return Err(PokeAByteError::BadPacketFromClient { explanation: Cow::Borrowed("failed to read data: no bytes after length") })
                };

                let Some(data) = data.get(..length) else {
                    return Err(PokeAByteError::BadPacketFromClient { explanation: Cow::Borrowed("failed to read data: insufficient length") })
                };

                Ok(Self::Write { data, address })
            },
            Instruction::Freeze => {
                let Some(_params) = bytes.get(0x8..0x14) else {
                    return Err(PokeAByteError::BadPacketFromClient { explanation: Cow::Borrowed("too small to be freeze header") })
                };

                let address = LittleEndian::read_u64(&bytes[0x8..]);
                let length: usize = LittleEndian::read_u32(&bytes[0x10..]) as usize;

                let Some(data) = bytes.get(0x20..) else {
                    return Err(PokeAByteError::BadPacketFromClient { explanation: Cow::Borrowed("failed to read data: no bytes after length") })
                };

                let Some(data) = data.get(..length) else {
                    return Err(PokeAByteError::BadPacketFromClient { explanation: Cow::Borrowed("failed to read data: insufficient length") })
                };

                Ok(Self::Freeze { address, data } )
            },
            Instruction::Unfreeze => {
                let Some(_params) = bytes.get(0x8..0x10) else {
                    return Err(PokeAByteError::BadPacketFromClient { explanation: Cow::Borrowed("too small to be unfreeze header") })
                };

                let address = LittleEndian::read_u64(&bytes[0x8..]);
                Ok(Self::Unfreeze { address })
            }
            Instruction::Close => Ok(Self::Close)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bare header: version/instruction/is_response set, everything else (including any body)
    /// zeroed.
    fn header_bytes(version: u8, instruction: u8, is_response: u8) -> [u8; METADATA_HEADER_SIZE] {
        let mut header = [0u8; METADATA_HEADER_SIZE];
        header[0] = version;
        header[4] = instruction;
        header[5] = is_response;
        header
    }

    /// A SETUP packet: `block_count` is the claimed count (may not match `blocks.len()`, to exercise
    /// bad shapes), and the block list is always padded to the fixed on-wire size.
    fn setup_packet(block_count: u32, blocks: &[(u32, u32, u32)]) -> Vec<u8> {
        let mut header = header_bytes(PROTOCOL_VERSION, Instruction::Setup as u8, 0);
        LittleEndian::write_u32(&mut header[8..12], block_count);
        LittleEndian::write_i32(&mut header[12..16], -1);

        let mut body = vec![0u8; READ_BLOCK_SIZE * MAX_NUMBER_OF_READ_BLOCKS];
        for (i, &(offset, address, length)) in blocks.iter().enumerate() {
            let base = i * READ_BLOCK_SIZE;
            LittleEndian::write_u32(&mut body[base..base + 4], offset);
            LittleEndian::write_u32(&mut body[base + 4..base + 8], address);
            LittleEndian::write_u32(&mut body[base + 8..base + 12], length);
        }

        let mut packet = header.to_vec();
        packet.extend_from_slice(&body);
        packet
    }

    /// A WRITE/FREEZE packet: a header carrying `address`/`length`, followed by `payload`.
    fn write_or_freeze_packet(instruction: Instruction, address: u64, length: u32, payload: &[u8]) -> Vec<u8> {
        let mut header = header_bytes(PROTOCOL_VERSION, instruction as u8, 0);
        LittleEndian::write_u64(&mut header[0x8..0x10], address);
        LittleEndian::write_u32(&mut header[0x10..0x14], length);
        let mut packet = header.to_vec();
        packet.extend_from_slice(payload);
        packet
    }

    #[test]
    fn header_is_validated() {
        assert!(PokeAByteProtocolRequestPacket::parse_bytes(&[]).is_err());
        assert!(PokeAByteProtocolRequestPacket::parse_bytes(&[0u8; METADATA_HEADER_SIZE - 1]).is_err());

        let wrong_version = header_bytes(PROTOCOL_VERSION + 1, Instruction::Ping as u8, 0);
        assert!(PokeAByteProtocolRequestPacket::parse_bytes(&wrong_version).is_err());

        let is_response = header_bytes(PROTOCOL_VERSION, Instruction::Ping as u8, 1);
        assert!(PokeAByteProtocolRequestPacket::parse_bytes(&is_response).is_err());

        let unknown_instruction = header_bytes(PROTOCOL_VERSION, 0x42, 0);
        assert!(PokeAByteProtocolRequestPacket::parse_bytes(&unknown_instruction).is_err());
    }

    #[test]
    fn setup_rejects_bad_shapes() {
        // Header only, no block-list body at all.
        let header_only = header_bytes(PROTOCOL_VERSION, Instruction::Setup as u8, 0);
        assert!(PokeAByteProtocolRequestPacket::parse_bytes(&header_only).is_err());

        assert!(PokeAByteProtocolRequestPacket::parse_bytes(&setup_packet(MAX_NUMBER_OF_READ_BLOCKS as u32 + 1, &[])).is_err());
        assert!(PokeAByteProtocolRequestPacket::parse_bytes(&setup_packet(0, &[])).is_err());
        // length + address overflows u32.
        assert!(PokeAByteProtocolRequestPacket::parse_bytes(&setup_packet(1, &[(0, 0xFFFF_FFF0, 0x20)])).is_err());
        // offset + length is absurd (and, regardless, blows past the size cap).
        assert!(PokeAByteProtocolRequestPacket::parse_bytes(&setup_packet(1, &[(0xFFFF_FFFF, 0, 0xFFFF_FFFF)])).is_err());
        // Comfortably valid otherwise, but past MAX_SHARED_MEMORY_LENGTH.
        assert!(PokeAByteProtocolRequestPacket::parse_bytes(&setup_packet(1, &[(0, 0, MAX_SHARED_MEMORY_LENGTH as u32 + 1)])).is_err());

        let valid_setup = setup_packet(1, &[(0, 0x0200_0000, 0x1000)]);
        let Ok(PokeAByteProtocolRequestPacket::Setup { blocks, .. }) = PokeAByteProtocolRequestPacket::parse_bytes(&valid_setup) else {
            panic!("expected a valid single-block Setup")
        };
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].game_address, 0x0200_0000);
        assert_eq!(blocks[0].range, 0..0x1000);
    }

    #[test]
    fn write_and_freeze_are_bounds_checked() {
        // Too short to even read the address/length fields.
        let truncated = header_bytes(PROTOCOL_VERSION, Instruction::Write as u8, 0);
        assert!(PokeAByteProtocolRequestPacket::parse_bytes(&truncated[..10]).is_err());

        // Claims 5 bytes of payload but only 4 are present.
        assert!(PokeAByteProtocolRequestPacket::parse_bytes(&write_or_freeze_packet(Instruction::Write, 0x1000, 5, &[1, 2, 3, 4])).is_err());

        // A zero-length write/freeze is legal (empty data).
        let empty_write = write_or_freeze_packet(Instruction::Write, 0x1000, 0, &[]);
        let Ok(PokeAByteProtocolRequestPacket::Write { data, .. }) = PokeAByteProtocolRequestPacket::parse_bytes(&empty_write) else {
            panic!("expected an empty Write")
        };
        assert!(data.is_empty());

        // Length matches the payload exactly.
        let payload = [9u8, 8, 7, 6];
        let write = write_or_freeze_packet(Instruction::Write, 0x1000, 4, &payload);
        let Ok(PokeAByteProtocolRequestPacket::Write { data, address }) = PokeAByteProtocolRequestPacket::parse_bytes(&write) else {
            panic!("expected a Write with data")
        };
        assert_eq!(data, payload);
        assert_eq!(address, 0x1000);

        let freeze = write_or_freeze_packet(Instruction::Freeze, 0x2000, 4, &payload);
        let Ok(PokeAByteProtocolRequestPacket::Freeze { data, address }) = PokeAByteProtocolRequestPacket::parse_bytes(&freeze) else {
            panic!("expected a Freeze with data")
        };
        assert_eq!(data, payload);
        assert_eq!(address, 0x2000);

        // Unfreeze: the standard header carries enough bytes; one byte short does not.
        let mut unfreeze = header_bytes(PROTOCOL_VERSION, Instruction::Unfreeze as u8, 0);
        LittleEndian::write_u64(&mut unfreeze[0x8..0x10], 0x3000);
        let Ok(PokeAByteProtocolRequestPacket::Unfreeze { address }) = PokeAByteProtocolRequestPacket::parse_bytes(&unfreeze) else {
            panic!("expected Unfreeze")
        };
        assert_eq!(address, 0x3000);
        assert!(PokeAByteProtocolRequestPacket::parse_bytes(&unfreeze[..METADATA_HEADER_SIZE - 1]).is_err());
    }

    #[test]
    fn garbage_never_panics() {
        let max_len = METADATA_HEADER_SIZE + READ_BLOCK_SIZE * MAX_NUMBER_OF_READ_BLOCKS + 64;
        for &instruction in &[0u8, 1, 2, 3, 4, 5, 0xFF, 0x42] {
            for &fill in &[0xFFu8, 0x00u8] {
                for len in 0..=max_len {
                    let mut buffer = vec![fill; len];
                    // Stamp a "valid" version/instruction/is_response where there is room, so the
                    // per-instruction parsers are actually exercised rather than always bailing out
                    // in the generic header check.
                    if len > 0 {
                        buffer[0] = PROTOCOL_VERSION;
                    }
                    if len > 4 {
                        buffer[4] = instruction;
                    }
                    if len > 5 {
                        buffer[5] = 0;
                    }
                    // Only requirement: this returns, however it returns. No panic, no hang.
                    let _ = PokeAByteProtocolRequestPacket::parse_bytes(&buffer);
                }
            }
        }
    }
}
