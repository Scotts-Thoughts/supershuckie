//! Memory regions and address text.

use serde::{Deserialize, Serialize};

/// A contiguous block of console memory, as listed by the running core.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionInfo {
    /// Human-readable name.
    pub name: String,
    /// Short name used in `SHORT:offset` addresses.
    pub short_name: String,
    /// First address.
    pub base: u32,
    /// Size in bytes.
    pub len: u32,
    /// Whether multi-byte values default to big-endian here.
    pub big_endian: bool,
    /// Whether the tools may write here.
    pub writable: bool
}

impl RegionInfo {
    /// One past the last address.
    #[inline]
    pub fn end(&self) -> u64 {
        self.base as u64 + self.len as u64
    }

    /// Whether `address` lies in the region.
    #[inline]
    pub fn contains(&self, address: u32) -> bool {
        address >= self.base && (address as u64) < self.end()
    }

    /// Offset of `address` if `[address, address + len)` lies entirely in the region.
    #[inline]
    pub fn offset_of(&self, address: u32, len: usize) -> Option<u32> {
        let offset = address.checked_sub(self.base)?;
        (offset as u64 + len as u64 <= self.len as u64).then_some(offset)
    }
}

/// Index of the region containing `address`.
pub fn find_region(regions: &[RegionInfo], address: u32) -> Option<usize> {
    regions.iter().position(|r| r.contains(address))
}

/// Hex digits to show every address of `regions` in: at least 4, and a full 8 once past 6.
pub fn address_width(regions: &[RegionInfo]) -> usize {
    let max = regions.iter().map(|r| r.end().saturating_sub(1)).max().unwrap_or(0xFFFF);
    let digits = (64 - max.leading_zeros() as usize).div_ceil(4).max(4);
    if digits > 6 { 8 } else { digits }
}

/// `0x02024284`, zero-padded to [`address_width`].
pub fn format_address(address: u32, regions: &[RegionInfo]) -> String {
    format!("0x{:0width$X}", address, width = address_width(regions))
}

/// `EWRAM:24284`, or the plain address if it is in no region.
pub fn format_region_address(address: u32, regions: &[RegionInfo]) -> String {
    match find_region(regions, address) {
        Some(index) => {
            let region = &regions[index];
            let width = (32 - region.len.saturating_sub(1).leading_zeros() as usize).div_ceil(4).max(1);
            format!("{}:{:0width$X}", region.short_name, address - region.base)
        }
        None => format_address(address, regions)
    }
}

fn parse_hex_u32(text: &str) -> Result<u32, String> {
    let text = text.trim();
    let digits = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .or_else(|| text.strip_prefix('$'))
        .unwrap_or(text)
        .replace('_', "");
    if digits.is_empty() {
        return Err("expected a hexadecimal number".to_owned())
    }
    u32::from_str_radix(&digits, 16).map_err(|_| format!("\"{text}\" is not a hexadecimal number"))
}

/// Parse an address typed by the user.
///
/// Accepts `0x02024284`, `$2024284`, `2024284` (always hexadecimal) and region-relative forms
/// `EWRAM:24284` or `EWRAM+0x24284` (short names are case-insensitive; the offset must lie inside
/// the region).
pub fn parse_address(text: &str, regions: &[RegionInfo]) -> Result<u32, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("no address given".to_owned())
    }

    if let Some(split) = text.find([':', '+']) {
        let (name, offset) = (text[..split].trim(), &text[split + 1..]);
        if name.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
            let region = regions
                .iter()
                .find(|r| r.short_name.eq_ignore_ascii_case(name) || r.name.eq_ignore_ascii_case(name))
                .ok_or_else(|| format!("no region called \"{name}\""))?;
            let offset = parse_hex_u32(offset)?;
            if offset >= region.len {
                return Err(format!("offset 0x{offset:X} is past the end of {} (0x{:X} bytes)", region.short_name, region.len))
            }
            return Ok(region.base + offset)
        }
    }

    parse_hex_u32(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regions() -> Vec<RegionInfo> {
        vec![
            RegionInfo { name: "EWRAM".into(), short_name: "EWRAM".into(), base: 0x0200_0000, len: 0x40000, big_endian: false, writable: true },
            RegionInfo { name: "Palette RAM".into(), short_name: "PAL".into(), base: 0x0500_0000, len: 0x400, big_endian: false, writable: true },
        ]
    }

    #[test]
    fn parses_addresses() {
        let r = regions();
        assert_eq!(parse_address("0x02024284", &r), Ok(0x02024284));
        assert_eq!(parse_address("2024284", &r), Ok(0x02024284));
        assert_eq!(parse_address(" $2024284 ", &r), Ok(0x02024284));
        assert_eq!(parse_address("EWRAM:24284", &r), Ok(0x02024284));
        assert_eq!(parse_address("ewram+0x24284", &r), Ok(0x02024284));
        assert_eq!(parse_address("Palette RAM:10", &r), Ok(0x05000010));
        assert!(parse_address("PAL:400", &r).is_err());
        assert!(parse_address("VRAM:0", &r).is_err());
        assert!(parse_address("zz", &r).is_err());
        assert!(parse_address("", &r).is_err());
    }

    #[test]
    fn formats_addresses() {
        let r = regions();
        assert_eq!(format_address(0x02024284, &r), "0x02024284");
        assert_eq!(format_region_address(0x02024284, &r), "EWRAM:24284");
        assert_eq!(format_region_address(0x05000010, &r), "PAL:010");
        let gb = vec![RegionInfo { name: "HRAM".into(), short_name: "HRAM".into(), base: 0xFF80, len: 0x7F, big_endian: true, writable: true }];
        assert_eq!(format_address(0xFF80, &gb), "0xFF80");
        assert_eq!(address_width(&regions()), 8);
    }
}
