//! Typed values: how bytes in memory are read as numbers or text, shown, and typed back in.

use crate::table::CharTable;
use serde::{Deserialize, Serialize};

/// The longest value (bytes or text) the tools handle.
pub const MAX_VALUE_SIZE: usize = 64;

/// What kind of value a run of bytes holds.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueType {
    U8,
    I8,
    U16,
    I16,
    U32,
    I32,
    F32,
    /// Binary-coded decimal, two digits per byte (Game Boy money and coins).
    Bcd,
    /// Raw bytes.
    Bytes,
    /// Text, through a character table.
    Text
}

impl ValueType {
    /// Every type, in the order the UI lists them.
    pub const ALL: [ValueType; 10] = [
        ValueType::U8, ValueType::I8, ValueType::U16, ValueType::I16, ValueType::U32, ValueType::I32,
        ValueType::F32, ValueType::Bcd, ValueType::Bytes, ValueType::Text
    ];

    /// Short lowercase name (`u16`, `bcd`, …).
    pub const fn name(self) -> &'static str {
        match self {
            ValueType::U8 => "u8",
            ValueType::I8 => "i8",
            ValueType::U16 => "u16",
            ValueType::I16 => "i16",
            ValueType::U32 => "u32",
            ValueType::I32 => "i32",
            ValueType::F32 => "f32",
            ValueType::Bcd => "bcd",
            ValueType::Bytes => "bytes",
            ValueType::Text => "text"
        }
    }

    /// The size of fixed-size types.
    pub const fn fixed_size(self) -> Option<u8> {
        match self {
            ValueType::U8 | ValueType::I8 => Some(1),
            ValueType::U16 | ValueType::I16 => Some(2),
            ValueType::U32 | ValueType::I32 | ValueType::F32 => Some(4),
            ValueType::Bcd | ValueType::Bytes | ValueType::Text => None
        }
    }

    /// Two's complement integers.
    pub const fn is_signed(self) -> bool {
        matches!(self, ValueType::I8 | ValueType::I16 | ValueType::I32)
    }

    /// Integers, floats and BCD (everything that compares as a number).
    pub const fn is_numeric(self) -> bool {
        !matches!(self, ValueType::Bytes | ValueType::Text)
    }

    /// Integers and BCD.
    pub const fn is_integer(self) -> bool {
        self.is_numeric() && !matches!(self, ValueType::F32)
    }

    /// Whether byte order matters.
    pub const fn has_endianness(self) -> bool {
        matches!(self, ValueType::U16 | ValueType::I16 | ValueType::U32 | ValueType::I32 | ValueType::F32 | ValueType::Bcd)
    }
}

/// A value type with its size and byte order.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ValueFormat {
    #[serde(rename = "type")]
    pub ty: ValueType,
    /// Size in bytes: fixed for integer and float types, 1-4 for BCD, 1-64 for bytes and text.
    pub size: u8,
    /// Most significant byte first.
    pub big_endian: bool
}

impl ValueFormat {
    /// A format with its size brought into range for the type.
    pub fn new(ty: ValueType, size: u8, big_endian: bool) -> Self {
        let size = match ty.fixed_size() {
            Some(fixed) => fixed,
            None if ty == ValueType::Bcd => size.clamp(1, 4),
            None => size.clamp(1, MAX_VALUE_SIZE as u8)
        };
        Self { ty, size, big_endian: big_endian && ty.has_endianness() }
    }

    /// Size in bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.size as usize
    }

    /// Short description (`u16 BE`, `bcd ×3`, `text ×8`).
    pub fn describe(&self) -> String {
        let mut out = self.ty.name().to_owned();
        if self.ty.fixed_size().is_none() {
            out += &format!(" ×{}", self.size);
        }
        if self.ty.has_endianness() && self.size > 1 {
            out += if self.big_endian { " BE" } else { " LE" };
        }
        out
    }
}

/// How numbers are shown.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayBase {
    #[default]
    Decimal,
    Hex,
    Binary
}

/// A decoded number.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Number {
    Int(i64),
    Float(f64)
}

impl Number {
    /// As a float, for mixed comparisons.
    #[inline]
    pub fn as_f64(self) -> f64 {
        match self {
            Number::Int(i) => i as f64,
            Number::Float(f) => f
        }
    }
}

/// The value's bits as an unsigned integer (bytes assembled per the byte order).
pub fn raw_bits(format: &ValueFormat, bytes: &[u8]) -> u64 {
    let bytes = &bytes[..bytes.len().min(format.len()).min(8)];
    let mut raw = 0u64;
    if format.big_endian {
        for &b in bytes {
            raw = (raw << 8) | b as u64;
        }
    }
    else {
        for &b in bytes.iter().rev() {
            raw = (raw << 8) | b as u64;
        }
    }
    raw
}

/// Decode a numeric value. `None` for bytes/text, a short buffer, or invalid BCD.
pub fn decode_number(format: &ValueFormat, bytes: &[u8]) -> Option<Number> {
    if bytes.len() < format.len() {
        return None
    }
    let raw = raw_bits(format, bytes);
    Some(match format.ty {
        ValueType::U8 | ValueType::U16 | ValueType::U32 => Number::Int(raw as i64),
        ValueType::I8 => Number::Int(raw as u8 as i8 as i64),
        ValueType::I16 => Number::Int(raw as u16 as i16 as i64),
        ValueType::I32 => Number::Int(raw as u32 as i32 as i64),
        ValueType::F32 => Number::Float(f32::from_bits(raw as u32) as f64),
        ValueType::Bcd => Number::Int(decode_bcd(format, bytes)?),
        ValueType::Bytes | ValueType::Text => return None
    })
}

fn decode_bcd(format: &ValueFormat, bytes: &[u8]) -> Option<i64> {
    let bytes = &bytes[..format.len()];
    let mut value = 0i64;
    let mut push = |byte: u8| {
        let (hi, lo) = (byte >> 4, byte & 0xF);
        if hi > 9 || lo > 9 {
            return false
        }
        value = value * 100 + (hi * 10 + lo) as i64;
        true
    };
    let ok = if format.big_endian { bytes.iter().all(|b| push(*b)) } else { bytes.iter().rev().all(|b| push(*b)) };
    ok.then_some(value)
}

/// Encode a number in `format`. `None` if it does not fit (or the type is not numeric).
pub fn encode_number(format: &ValueFormat, number: Number) -> Option<Vec<u8>> {
    let len = format.len();
    let raw: u64 = match (format.ty, number) {
        (ValueType::F32, n) => (n.as_f64() as f32).to_bits() as u64,
        (ValueType::Bcd, Number::Int(v)) => {
            if v < 0 || v >= 100i64.pow(len as u32) {
                return None
            }
            let mut v = v;
            let mut out = vec![0u8; len];
            for byte in out.iter_mut() {
                let two = (v % 100) as u8;
                *byte = (two / 10) << 4 | (two % 10);
                v /= 100;
            }
            // `out` is least significant first.
            if format.big_endian {
                out.reverse();
            }
            return Some(out)
        }
        (ty, Number::Int(v)) if ty.is_integer() => {
            let bits = len as u32 * 8;
            let min = -(1i64 << (bits - 1));
            let max = (1i64 << bits) - 1;
            if v < min || v > max {
                return None
            }
            v as u64
        }
        _ => return None
    };
    let mut out = raw.to_le_bytes()[..len].to_vec();
    if format.big_endian {
        out.reverse();
    }
    Some(out)
}

/// Show `bytes` as a value of `format`.
pub fn format_value(format: &ValueFormat, base: DisplayBase, bytes: &[u8], table: &CharTable) -> String {
    let len = format.len();
    if bytes.len() < len {
        return "??".to_owned()
    }
    let bytes = &bytes[..len];
    match format.ty {
        ValueType::Bytes => format_hex_bytes(bytes),
        ValueType::Text => table.decode(bytes, '·'),
        ValueType::F32 if base == DisplayBase::Decimal => {
            let Some(Number::Float(f)) = decode_number(format, bytes) else { unreachable!() };
            let f = f as f32;
            if f.is_finite() && f != 0.0 && (f.abs() >= 1e7 || f.abs() < 1e-4) {
                format!("{f:e}")
            }
            else {
                format!("{f}")
            }
        }
        ValueType::Bcd if base == DisplayBase::Decimal => match decode_number(format, bytes) {
            Some(Number::Int(v)) => v.to_string(),
            _ => format!("?{}", format_hex_bytes(bytes).replace(' ', ""))
        },
        _ => {
            let raw = raw_bits(format, bytes);
            match base {
                DisplayBase::Decimal => match decode_number(format, bytes) {
                    Some(Number::Int(v)) => v.to_string(),
                    _ => unreachable!()
                },
                DisplayBase::Hex => format!("0x{:0width$X}", raw, width = len * 2),
                DisplayBase::Binary => format!("0b{:0width$b}", raw, width = len * 8)
            }
        }
    }
}

/// `12 34 AB`.
pub fn format_hex_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out += &format!("{b:02X}");
    }
    out
}

/// Reject non-ASCII text before it is sliced by byte index, which would otherwise panic when a
/// multi-byte character straddles the cut (e.g. a fullwidth digit or an accented letter).
pub(crate) fn ascii_only<'a>(text: &'a str, what: &str) -> Result<&'a str, String> {
    if text.is_ascii() {
        Ok(text)
    }
    else {
        Err(format!("{what} must be ASCII (got \"{text}\")"))
    }
}

/// Parse hexadecimal bytes: `12 34 AB`, `1234AB` or `0x12, 0x34`.
pub fn parse_hex_bytes(text: &str) -> Result<Vec<u8>, String> {
    let cleaned: String = text
        .split(|c: char| c.is_whitespace() || c == ',')
        .map(|t| t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")).unwrap_or(t))
        .collect::<Vec<_>>()
        .join("");
    if cleaned.is_empty() {
        return Err("no bytes given".to_owned())
    }
    let cleaned = ascii_only(&cleaned, "hex bytes")?;
    if cleaned.len() % 2 != 0 {
        return Err("hexadecimal bytes need two digits each".to_owned())
    }
    (0..cleaned.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&cleaned[i..i + 2], 16).map_err(|_| format!("\"{}\" is not a hexadecimal byte", &cleaned[i..i + 2])))
        .collect()
}

/// One byte of a search pattern: the byte matches when `byte & mask == value`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatternByte {
    pub mask: u8,
    pub value: u8
}

/// Parse a byte pattern with wildcards: `12 ?? 3? A?`. Bytes are separated by spaces; each is two
/// hexadecimal digits, either of which may be `?`.
pub fn parse_pattern(text: &str) -> Result<Vec<PatternByte>, String> {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let tokens: Vec<String> = if tokens.len() == 1 && tokens[0].len() > 2 {
        let t = ascii_only(tokens[0], "the pattern")?;
        if t.len() % 2 != 0 {
            return Err("pattern bytes need two digits each".to_owned())
        }
        (0..t.len()).step_by(2).map(|i| t[i..i + 2].to_owned()).collect()
    }
    else {
        tokens.iter().map(|t| t.to_string()).collect()
    };
    if tokens.is_empty() {
        return Err("empty pattern".to_owned())
    }
    if tokens.len() > MAX_VALUE_SIZE {
        return Err(format!("patterns are limited to {MAX_VALUE_SIZE} bytes"))
    }
    tokens
        .iter()
        .map(|token| {
            let digits: Vec<char> = token.chars().collect();
            if digits.len() != 2 {
                return Err(format!("\"{token}\" is not a pattern byte (two hex digits or ?)"))
            }
            let mut mask = 0u8;
            let mut value = 0u8;
            for (i, c) in digits.iter().enumerate() {
                let shift = if i == 0 { 4 } else { 0 };
                if *c == '?' {
                    continue
                }
                let d = c.to_digit(16).ok_or_else(|| format!("\"{token}\" is not a pattern byte"))? as u8;
                mask |= 0xF << shift;
                value |= d << shift;
            }
            Ok(PatternByte { mask, value })
        })
        .collect()
}

/// Parse an integer: decimal (`-12`), hexadecimal (`0x1F`, `$1F`) or binary (`0b101`).
pub fn parse_integer(text: &str) -> Result<i64, String> {
    let text = text.trim().replace('_', "");
    let (negative, body) = match text.strip_prefix('-') {
        Some(rest) => (true, rest.trim_start()),
        None => (false, text.as_str())
    };
    let parsed = if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")).or_else(|| body.strip_prefix('$')) {
        u64::from_str_radix(hex, 16)
    }
    else if let Some(bin) = body.strip_prefix("0b").or_else(|| body.strip_prefix("0B")) {
        u64::from_str_radix(bin, 2)
    }
    else {
        body.parse::<u64>()
    }
    .map_err(|_| format!("\"{text}\" is not a number"))?;
    let value = i64::try_from(parsed).map_err(|_| format!("{text} is too large"))?;
    Ok(if negative { -value } else { value })
}

/// Parse a number for `format`'s type: integers for integer types and BCD, decimals for f32.
pub fn parse_number(format: &ValueFormat, text: &str) -> Result<Number, String> {
    match format.ty {
        ValueType::F32 => {
            let t = text.trim();
            t.parse::<f64>().map(Number::Float).map_err(|_| format!("\"{t}\" is not a number"))
        }
        ty if ty.is_integer() => parse_integer(text).map(Number::Int),
        ty => Err(format!("{} values are not numbers", ty.name()))
    }
}

/// Parse text typed by the user into the bytes of a `format` value.
///
/// Numbers are range-checked for the size (integer types accept both the signed and unsigned
/// range, so `0xFF` can be typed into an `i8`); bytes must be exactly `size` long; text may be
/// shorter than `size` (only the encoded bytes are returned).
pub fn parse_value(format: &ValueFormat, text: &str, table: &CharTable) -> Result<Vec<u8>, String> {
    match format.ty {
        ValueType::Bytes => {
            let bytes = parse_hex_bytes(text)?;
            if bytes.len() != format.len() {
                return Err(format!("expected {} bytes, got {}", format.len(), bytes.len()))
            }
            Ok(bytes)
        }
        ValueType::Text => {
            let bytes = table.encode(text)?;
            if bytes.len() > format.len() {
                return Err(format!("that text needs {} bytes; the value holds {}", bytes.len(), format.len()))
            }
            Ok(bytes)
        }
        ValueType::Bcd => {
            let t = text.trim();
            if t.is_empty() || !t.chars().all(|c| c.is_ascii_digit()) {
                return Err("BCD values are decimal digits".to_owned())
            }
            if t.len() > format.len() * 2 {
                return Err(format!("{} BCD bytes hold at most {} digits", format.len(), format.len() * 2))
            }
            let value = t.parse::<i64>().map_err(|_| format!("\"{t}\" is not a number"))?;
            encode_number(format, Number::Int(value)).ok_or_else(|| "value out of range".to_owned())
        }
        _ => {
            let number = parse_number(format, text)?;
            encode_number(format, number).ok_or_else(|| {
                let bits = format.len() * 8;
                format!("{} does not fit in {} ({} to {})", text.trim(), format.ty.name(), -(1i128 << (bits - 1)), (1i128 << bits) - 1)
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(ty: ValueType, size: u8, be: bool) -> ValueFormat {
        ValueFormat::new(ty, size, be)
    }

    #[test]
    fn normalizes_formats() {
        assert_eq!(fmt(ValueType::U16, 9, true).size, 2);
        assert_eq!(fmt(ValueType::Bcd, 9, true).size, 4);
        assert_eq!(fmt(ValueType::Bytes, 0, false).size, 1);
        assert!(!fmt(ValueType::U8, 1, true).big_endian);
        assert_eq!(fmt(ValueType::U16, 2, true).describe(), "u16 BE");
        assert_eq!(fmt(ValueType::Text, 8, false).describe(), "text ×8");
    }

    #[test]
    fn decodes_numbers() {
        assert_eq!(decode_number(&fmt(ValueType::U16, 2, false), &[0x34, 0x12]), Some(Number::Int(0x1234)));
        assert_eq!(decode_number(&fmt(ValueType::U16, 2, true), &[0x12, 0x34]), Some(Number::Int(0x1234)));
        assert_eq!(decode_number(&fmt(ValueType::I16, 2, false), &[0xFE, 0xFF]), Some(Number::Int(-2)));
        assert_eq!(decode_number(&fmt(ValueType::I8, 1, false), &[0x80]), Some(Number::Int(-128)));
        assert_eq!(decode_number(&fmt(ValueType::U32, 4, false), &[0xFF; 4]), Some(Number::Int(0xFFFF_FFFF)));
        assert_eq!(decode_number(&fmt(ValueType::F32, 4, false), &1.5f32.to_le_bytes()), Some(Number::Float(1.5)));
        assert_eq!(decode_number(&fmt(ValueType::Bcd, 3, true), &[0x01, 0x23, 0x45]), Some(Number::Int(12345)));
        assert_eq!(decode_number(&fmt(ValueType::Bcd, 2, false), &[0x45, 0x23]), Some(Number::Int(2345)));
        assert_eq!(decode_number(&fmt(ValueType::Bcd, 1, false), &[0x1A]), None);
        assert_eq!(decode_number(&fmt(ValueType::U32, 4, false), &[1, 2]), None);
    }

    #[test]
    fn encodes_numbers() {
        assert_eq!(encode_number(&fmt(ValueType::U16, 2, true), Number::Int(0x1234)), Some(vec![0x12, 0x34]));
        assert_eq!(encode_number(&fmt(ValueType::I8, 1, false), Number::Int(-1)), Some(vec![0xFF]));
        assert_eq!(encode_number(&fmt(ValueType::I8, 1, false), Number::Int(255)), Some(vec![0xFF]));
        assert_eq!(encode_number(&fmt(ValueType::U8, 1, false), Number::Int(256)), None);
        assert_eq!(encode_number(&fmt(ValueType::U8, 1, false), Number::Int(-129)), None);
        assert_eq!(encode_number(&fmt(ValueType::Bcd, 3, true), Number::Int(999_999)), Some(vec![0x99, 0x99, 0x99]));
        assert_eq!(encode_number(&fmt(ValueType::Bcd, 3, false), Number::Int(12345)), Some(vec![0x45, 0x23, 0x01]));
        assert_eq!(encode_number(&fmt(ValueType::Bcd, 1, true), Number::Int(100)), None);
        assert_eq!(encode_number(&fmt(ValueType::F32, 4, true), Number::Float(1.5)), Some(1.5f32.to_be_bytes().to_vec()));
        for ty in [ValueType::U8, ValueType::I16, ValueType::U32, ValueType::Bcd] {
            let f = fmt(ty, 2, true);
            let bytes = encode_number(&f, Number::Int(42)).unwrap();
            assert_eq!(decode_number(&f, &bytes), Some(Number::Int(42)), "{ty:?}");
        }
    }

    #[test]
    fn formats_values() {
        let t = CharTable::ascii();
        assert_eq!(format_value(&fmt(ValueType::U16, 2, false), DisplayBase::Hex, &[0x34, 0x12], &t), "0x1234");
        assert_eq!(format_value(&fmt(ValueType::I8, 1, false), DisplayBase::Decimal, &[0xFF], &t), "-1");
        assert_eq!(format_value(&fmt(ValueType::I8, 1, false), DisplayBase::Hex, &[0xFF], &t), "0xFF");
        assert_eq!(format_value(&fmt(ValueType::U8, 1, false), DisplayBase::Binary, &[5], &t), "0b00000101");
        assert_eq!(format_value(&fmt(ValueType::Bcd, 2, true), DisplayBase::Decimal, &[0x12, 0x34], &t), "1234");
        assert_eq!(format_value(&fmt(ValueType::Bcd, 1, true), DisplayBase::Decimal, &[0x1F], &t), "?1F");
        assert_eq!(format_value(&fmt(ValueType::Bytes, 3, false), DisplayBase::Decimal, &[1, 0xAB, 0], &t), "01 AB 00");
        assert_eq!(format_value(&fmt(ValueType::Text, 3, false), DisplayBase::Decimal, b"Hi\x00", &t), "Hi·");
        assert_eq!(format_value(&fmt(ValueType::F32, 4, false), DisplayBase::Decimal, &2.25f32.to_le_bytes(), &t), "2.25");
        assert_eq!(format_value(&fmt(ValueType::U32, 4, false), DisplayBase::Decimal, &[1], &t), "??");
    }

    #[test]
    fn parses_values() {
        let t = CharTable::ascii();
        assert_eq!(parse_value(&fmt(ValueType::U16, 2, false), "0x1234", &t), Ok(vec![0x34, 0x12]));
        assert_eq!(parse_value(&fmt(ValueType::U16, 2, true), "4660", &t), Ok(vec![0x12, 0x34]));
        assert_eq!(parse_value(&fmt(ValueType::I16, 2, false), "-2", &t), Ok(vec![0xFE, 0xFF]));
        assert_eq!(parse_value(&fmt(ValueType::I8, 1, false), "$FF", &t), Ok(vec![0xFF]));
        assert!(parse_value(&fmt(ValueType::U8, 1, false), "300", &t).is_err());
        assert_eq!(parse_value(&fmt(ValueType::Bcd, 3, true), "3000", &t), Ok(vec![0x00, 0x30, 0x00]));
        assert!(parse_value(&fmt(ValueType::Bcd, 1, true), "123", &t).is_err());
        assert!(parse_value(&fmt(ValueType::Bcd, 2, true), "-1", &t).is_err());
        assert_eq!(parse_value(&fmt(ValueType::Bytes, 2, false), "ab cd", &t), Ok(vec![0xAB, 0xCD]));
        assert!(parse_value(&fmt(ValueType::Bytes, 2, false), "ab", &t).is_err());
        assert_eq!(parse_value(&fmt(ValueType::Text, 4, false), "Hi", &t), Ok(b"Hi".to_vec()));
        assert!(parse_value(&fmt(ValueType::Text, 1, false), "Hi", &t).is_err());
        assert_eq!(parse_value(&fmt(ValueType::F32, 4, false), "0.5", &t), Ok(0.5f32.to_le_bytes().to_vec()));
        assert_eq!(parse_integer("0b101"), Ok(5));
        assert_eq!(parse_integer("- 7"), Ok(-7));
    }

    #[test]
    fn parses_patterns() {
        assert_eq!(parse_pattern("12 ?? 3? ?4"), Ok(vec![
            PatternByte { mask: 0xFF, value: 0x12 },
            PatternByte { mask: 0x00, value: 0x00 },
            PatternByte { mask: 0xF0, value: 0x30 },
            PatternByte { mask: 0x0F, value: 0x04 },
        ]));
        assert_eq!(parse_pattern("12??").unwrap().len(), 2);
        assert!(parse_pattern("123").is_err());
        assert!(parse_pattern("1G").is_err());
        assert_eq!(parse_hex_bytes("0x12, 0x34"), Ok(vec![0x12, 0x34]));
    }

    #[test]
    fn non_ascii_input_is_an_error_not_a_panic() {
        // A multi-byte character straddling a byte-index cut must not panic (H6).
        assert!(parse_hex_bytes("aéa").is_err());
        assert!(parse_hex_bytes("ｆｆ").is_err());
        assert!(parse_pattern("ａａ").is_err());
        assert!(parse_pattern("1é").is_err());
    }
}
