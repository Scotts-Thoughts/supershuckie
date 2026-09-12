//! The watch list: named, typed addresses (optionally through pointers) with change logging, pause
//! conditions and freezes, saved per ROM as JSON.

use crate::region::{format_address, parse_address, RegionInfo};
use crate::value::{format_hex_bytes, parse_hex_bytes, DisplayBase, ValueFormat, MAX_VALUE_SIZE};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Pointer dereferences a watch address may go through.
pub const MAX_POINTER_DEPTH: usize = 4;

/// Version written to watch files.
pub const WATCH_FILE_VERSION: u32 = 1;

/// Where a watch's value is: `base`, then for each offset, the little-endian 32-bit pointer read at
/// the current address plus the offset.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WatchAddress {
    #[serde(with = "hex_u32")]
    pub base: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub offsets: Vec<i32>
}

impl WatchAddress {
    pub fn direct(address: u32) -> Self {
        Self { base: address, offsets: Vec::new() }
    }

    pub fn is_pointer(&self) -> bool {
        !self.offsets.is_empty()
    }
}

/// `[0x02101D2C]+0xC`, or the plain address.
pub fn format_watch_address(address: &WatchAddress, regions: &[RegionInfo]) -> String {
    let mut out = "[".repeat(address.offsets.len());
    out += &format_address(address.base, regions);
    for offset in &address.offsets {
        out.push(']');
        if *offset != 0 {
            out += &if *offset < 0 { format!("-0x{:X}", offset.unsigned_abs()) } else { format!("+0x{offset:X}") };
        }
    }
    out
}

/// Parse `0x02024284`, `EWRAM:24284`, `[0x02101D2C]+0xC`, `[[EWRAM:1D2C]+4]-8`… Offsets are
/// hexadecimal.
pub fn parse_watch_address(text: &str, regions: &[RegionInfo]) -> Result<WatchAddress, String> {
    let text = text.trim();
    let depth = text.chars().take_while(|c| *c == '[').count();
    if depth > MAX_POINTER_DEPTH {
        return Err(format!("at most {MAX_POINTER_DEPTH} pointers deep"))
    }
    let mut rest = &text[depth..];
    if depth == 0 {
        return Ok(WatchAddress::direct(parse_address(rest, regions)?))
    }

    let close = rest.find(']').ok_or("missing ]")?;
    let base = parse_address(&rest[..close], regions)?;
    rest = &rest[close..];

    let mut offsets = Vec::with_capacity(depth);
    for level in 0..depth {
        rest = rest.strip_prefix(']').ok_or_else(|| "missing ]".to_owned())?.trim_start();
        let end = rest.find(']').unwrap_or(rest.len());
        let term = rest[..end].trim();
        let offset = if term.is_empty() {
            0
        }
        else {
            let (negative, digits) = match term.as_bytes()[0] {
                b'+' => (false, &term[1..]),
                b'-' => (true, &term[1..]),
                _ => return Err(format!("expected +offset or -offset after ], found \"{term}\""))
            };
            let digits = digits.trim();
            let digits = digits.strip_prefix("0x").or_else(|| digits.strip_prefix("0X")).unwrap_or(digits);
            let value = i64::from_str_radix(digits, 16).map_err(|_| format!("\"{term}\" is not a hexadecimal offset"))?;
            let value = if negative { -value } else { value };
            i32::try_from(value).map_err(|_| format!("offset {term} is too large"))?
        };
        offsets.push(offset);
        rest = &rest[end..];
        if level + 1 == depth && !rest.trim().is_empty() {
            return Err(format!("unexpected \"{}\"", rest.trim()))
        }
    }
    Ok(WatchAddress { base, offsets })
}

/// When a traced watch pauses emulation (see `supershuckie_core::memory_monitor::TraceCondition`).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "when", content = "value", rename_all = "snake_case")]
pub enum WatchCondition {
    Changes,
    Equals(i64),
    NotEquals(i64),
    GreaterThan(i64),
    LessThan(i64),
    IncreasedBy(i64),
    DecreasedBy(i64)
}

/// A value held in place.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreezeState {
    #[serde(with = "hex_bytes")]
    pub value: Vec<u8>,
    /// Whether it is being applied. Freezes are saved but always load inactive.
    #[serde(default)]
    pub active: bool
}

/// A watched value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Watch {
    pub id: u32,
    #[serde(default)]
    pub label: String,
    pub address: WatchAddress,
    pub format: ValueFormat,
    #[serde(default)]
    pub display: DisplayBase,
    /// Character table name for text watches (empty: ASCII).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub table: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub group: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
    /// Log every change, frame-accurately.
    #[serde(default)]
    pub trace: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_when: Option<WatchCondition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freeze: Option<FreezeState>
}

impl Watch {
    /// Whether the core thread must compare this watch every frame.
    pub fn is_traced(&self) -> bool {
        self.trace || self.pause_when.is_some()
    }

    /// Check the watch's fields.
    pub fn validate(&self) -> Result<(), String> {
        if self.address.offsets.len() > MAX_POINTER_DEPTH {
            return Err(format!("at most {MAX_POINTER_DEPTH} pointers deep"))
        }
        if self.format.len() == 0 || self.format.len() > MAX_VALUE_SIZE {
            return Err(format!("values are 1 to {MAX_VALUE_SIZE} bytes"))
        }
        if let Some(freeze) = &self.freeze && freeze.value.len() != self.format.len() {
            return Err(format!("the frozen value is {} bytes but the watch is {}", freeze.value.len(), self.format.len()))
        }
        if self.pause_when.is_some_and(|c| c != WatchCondition::Changes) && !self.format.ty.is_integer() {
            return Err("only integer and BCD watches can pause on a value".to_owned())
        }
        Ok(())
    }
}

/// A saved watch list.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WatchFile {
    pub version: u32,
    /// Console the watches were made for (informational).
    #[serde(default)]
    pub console: String,
    /// BLAKE3 of the ROM they were made for, in hex (a mismatch is warned about, not refused:
    /// ROM hacks share layouts).
    #[serde(default)]
    pub rom_checksum: String,
    #[serde(default)]
    pub watches: Vec<Watch>
}

impl WatchFile {
    /// Parse a watch file. Freezes are loaded inactive, ids are made unique, invalid watches are
    /// dropped (and reported).
    pub fn parse(text: &str) -> Result<(WatchFile, Vec<String>), String> {
        let mut file: WatchFile = serde_json::from_str(text).map_err(|e| format!("not a watch list: {e}"))?;
        if file.version > WATCH_FILE_VERSION {
            return Err(format!("this watch list is version {}, newer than this version of Super Shuckie understands ({WATCH_FILE_VERSION})", file.version))
        }
        let mut problems = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut next_id = file.watches.iter().map(|w| w.id).max().unwrap_or(0).wrapping_add(1).max(1);
        file.watches.retain_mut(|watch| {
            if let Err(e) = watch.validate() {
                problems.push(format!("\"{}\": {e}", watch.label));
                return false
            }
            if watch.id == 0 || !seen.insert(watch.id) {
                watch.id = next_id;
                next_id += 1;
                seen.insert(watch.id);
            }
            if let Some(freeze) = watch.freeze.as_mut() {
                freeze.active = false;
            }
            true
        });
        Ok((file, problems))
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("watch files always serialize")
    }
}

mod hex_u32 {
    use super::*;

    pub fn serialize<S: Serializer>(value: &u32, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("0x{value:08X}"))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u32, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Either {
            Text(String),
            Number(u32)
        }
        match Either::deserialize(deserializer)? {
            Either::Number(n) => Ok(n),
            Either::Text(t) => {
                let digits = t.trim().strip_prefix("0x").or_else(|| t.trim().strip_prefix("0X")).unwrap_or(t.trim());
                u32::from_str_radix(digits, 16).map_err(serde::de::Error::custom)
            }
        }
    }
}

mod hex_bytes {
    use super::*;

    pub fn serialize<S: Serializer>(value: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format_hex_bytes(value))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        parse_hex_bytes(&text).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::ValueType;

    fn regions() -> Vec<RegionInfo> {
        vec![RegionInfo { name: "EWRAM".into(), short_name: "EWRAM".into(), base: 0x0200_0000, len: 0x40000, big_endian: false, writable: true }]
    }

    #[test]
    fn pointer_path_text() {
        let r = regions();
        assert_eq!(parse_watch_address("0x02024284", &r), Ok(WatchAddress::direct(0x02024284)));
        assert_eq!(parse_watch_address("EWRAM:24284", &r), Ok(WatchAddress::direct(0x02024284)));
        let one = parse_watch_address("[0x02101D2C]+0xC", &r).unwrap();
        assert_eq!(one, WatchAddress { base: 0x02101D2C, offsets: vec![0xC] });
        assert_eq!(format_watch_address(&one, &r), "[0x02101D2C]+0xC");
        let two = parse_watch_address("[[EWRAM:1D2C] + 4] - 10", &r).unwrap();
        assert_eq!(two, WatchAddress { base: 0x02001D2C, offsets: vec![4, -0x10] });
        assert_eq!(format_watch_address(&two, &r), "[[0x02001D2C]+0x4]-0x10");
        assert_eq!(parse_watch_address("[0x02000000]", &r).unwrap().offsets, vec![0]);
        assert!(parse_watch_address("[0x02000000", &r).is_err());
        assert!(parse_watch_address("[0x02000000]*4", &r).is_err());
        assert!(parse_watch_address("[[[[[0]]]]]", &r).is_err());
    }

    #[test]
    fn watch_files_round_trip() {
        let watch = Watch {
            id: 3,
            label: "Money".into(),
            address: WatchAddress { base: 0x02024284, offsets: vec![0x10] },
            format: ValueFormat::new(ValueType::Bcd, 3, true),
            display: DisplayBase::Decimal,
            table: String::new(),
            group: "Player".into(),
            notes: String::new(),
            trace: true,
            pause_when: Some(WatchCondition::Equals(999999)),
            freeze: Some(FreezeState { value: vec![0x99, 0x99, 0x99], active: true })
        };
        let file = WatchFile { version: WATCH_FILE_VERSION, console: "GBA".into(), rom_checksum: "abc".into(), watches: vec![watch.clone()] };
        let json = file.to_json();
        assert!(json.contains("\"0x02024284\""), "{json}");
        assert!(json.contains("\"99 99 99\""), "{json}");
        let (parsed, problems) = WatchFile::parse(&json).unwrap();
        assert!(problems.is_empty());
        let mut expected = watch;
        expected.freeze.as_mut().unwrap().active = false;
        assert_eq!(parsed.watches, vec![expected], "freezes load inactive");

        // Unknown fields are ignored, duplicate ids renumbered, bad watches dropped.
        let text = r#"{"version":1,"future":true,"watches":[
            {"id":1,"address":{"base":"0x10"},"format":{"type":"u8","size":1,"big_endian":false},"extra":1},
            {"id":1,"address":{"base":32},"format":{"type":"u16","size":2,"big_endian":false}},
            {"id":2,"label":"bad","address":{"base":"0x10"},"format":{"type":"bytes","size":2,"big_endian":false},"freeze":{"value":"01","active":true}}
        ]}"#;
        let (parsed, problems) = WatchFile::parse(text).unwrap();
        assert_eq!(parsed.watches.len(), 2);
        assert_ne!(parsed.watches[0].id, parsed.watches[1].id);
        assert_eq!(parsed.watches[1].address.base, 32);
        assert_eq!(problems.len(), 1);
        assert!(WatchFile::parse(r#"{"version":99}"#).is_err());
    }
}
