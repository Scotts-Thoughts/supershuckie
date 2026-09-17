//! Character tables: how a game's text bytes map to characters.
//!
//! Tables use the common `.tbl` format: one `XX=text` entry per line, where `XX` is one or more
//! hexadecimal bytes and `text` is what they stand for. Lines starting with `;` or `#` are
//! comments; entries starting with `/` or `*` (end and newline markers) are ignored.

use std::collections::BTreeMap;

use crate::value::ascii_only;

/// A character table.
#[derive(Clone, Debug)]
pub struct CharTable {
    name: String,
    /// What each single byte stands for.
    single: Vec<Option<String>>,
    /// Multi-byte sequences.
    multi: BTreeMap<Vec<u8>, String>,
    longest_key: usize,
    /// Text to bytes, for encoding.
    encode: BTreeMap<String, Vec<u8>>,
    longest_text: usize
}

impl CharTable {
    /// Printable ASCII.
    pub fn ascii() -> Self {
        let mut table = Self::empty("ASCII");
        for byte in 0x20u8..0x7F {
            table.insert(vec![byte], (byte as char).to_string());
        }
        table
    }

    fn empty(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            single: vec![None; 256],
            multi: BTreeMap::new(),
            longest_key: 1,
            encode: BTreeMap::new(),
            longest_text: 0
        }
    }

    fn insert(&mut self, key: Vec<u8>, text: String) {
        if key.len() == 1 {
            self.single[key[0] as usize] = Some(text.clone());
        }
        else {
            self.longest_key = self.longest_key.max(key.len());
            self.multi.insert(key.clone(), text.clone());
        }
        self.longest_text = self.longest_text.max(text.chars().count());
        // The first entry for a text wins when encoding, so earlier (usually canonical) codes are used.
        self.encode.entry(text).or_insert(key);
    }

    /// Parse a `.tbl` file.
    pub fn parse_tbl(name: &str, contents: &str) -> Result<Self, String> {
        let mut table = Self::empty(name);
        for (number, line) in contents.lines().enumerate() {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line.trim().is_empty() || line.starts_with(';') || line.starts_with('#') || line.starts_with('/') || line.starts_with('*') {
                continue
            }
            let Some((key, text)) = line.split_once('=') else {
                return Err(format!("line {}: expected XX=text", number + 1))
            };
            let key = key.trim();
            if key.is_empty() || key.len() % 2 != 0 || key.len() > 8 {
                return Err(format!("line {}: \"{key}\" is not 1-4 hexadecimal bytes", number + 1))
            }
            let key = ascii_only(key, "byte codes").map_err(|e| format!("line {}: {e}", number + 1))?;
            let bytes = (0..key.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&key[i..i + 2], 16))
                .collect::<Result<Vec<u8>, _>>()
                .map_err(|_| format!("line {}: \"{key}\" is not hexadecimal", number + 1))?;
            if text.is_empty() {
                return Err(format!("line {}: no text for {key}", number + 1))
            }
            table.insert(bytes, text.to_owned());
        }
        Ok(table)
    }

    /// The table's name.
    #[inline]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// What a single byte stands for, if anything.
    #[inline]
    pub fn glyph(&self, byte: u8) -> Option<&str> {
        self.single[byte as usize].as_deref()
    }

    /// Decode `bytes`, using `unknown` for bytes the table does not cover.
    pub fn decode(&self, bytes: &[u8], unknown: char) -> String {
        let mut out = String::new();
        let mut i = 0;
        'outer: while i < bytes.len() {
            for len in (2..=self.longest_key.min(bytes.len() - i)).rev() {
                if let Some(text) = self.multi.get(&bytes[i..i + len]) {
                    out.push_str(text);
                    i += len;
                    continue 'outer
                }
            }
            match self.glyph(bytes[i]) {
                Some(text) => out.push_str(text),
                None => out.push(unknown)
            }
            i += 1;
        }
        out
    }

    /// Encode `text`, matching the longest table entries first.
    pub fn encode(&self, text: &str) -> Result<Vec<u8>, String> {
        let chars: Vec<(usize, char)> = text.char_indices().collect();
        let mut out = Vec::new();
        let mut i = 0;
        'outer: while i < chars.len() {
            let start = chars[i].0;
            for count in (1..=self.longest_text.min(chars.len() - i)).rev() {
                let end = chars.get(i + count).map(|c| c.0).unwrap_or(text.len());
                if let Some(bytes) = self.encode.get(&text[start..end]) {
                    out.extend_from_slice(bytes);
                    i += count;
                    continue 'outer
                }
            }
            return Err(format!("the {} table has no code for \"{}\"", self.name, chars[i].1))
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_round_trip() {
        let t = CharTable::ascii();
        assert_eq!(t.decode(b"Hi!\x00", '.'), "Hi!.");
        assert_eq!(t.encode("Hi!"), Ok(b"Hi!".to_vec()));
        assert!(t.encode("é").is_err());
        assert_eq!(t.glyph(b'A'), Some("A"));
        assert_eq!(t.glyph(0), None);
    }

    #[test]
    fn tbl_files() {
        let t = CharTable::parse_tbl("gen3", "; comment\nBB=A\nBC=B\n0000=PKMN\nB4='\nF0=:\n/FF\n*FE\n3D==\n").unwrap();
        assert_eq!(t.decode(&[0xBB, 0xBC, 0x00, 0x00, 0x01], '?'), "ABPKMN?");
        assert_eq!(t.encode("ABPKMN"), Ok(vec![0xBB, 0xBC, 0x00, 0x00]));
        assert_eq!(t.glyph(0x3D), Some("="));
        assert!(CharTable::parse_tbl("bad", "ZZ=a").is_err());
        assert!(CharTable::parse_tbl("bad", "ABC=a").is_err());
        assert!(CharTable::parse_tbl("bad", "nothing").is_err());
    }

    #[test]
    fn non_ascii_byte_codes_are_an_error_not_a_panic() {
        // A multi-byte character on the byte-code side must not panic when sliced (H6).
        assert!(CharTable::parse_tbl("t", "aé=X").is_err());
        assert!(CharTable::parse_tbl("t", "é=X").is_err());
        // Non-ASCII text is legal; only the byte-code side is restricted.
        assert!(CharTable::parse_tbl("t", "41=é").is_ok());
    }
}
