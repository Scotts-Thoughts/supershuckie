use std::ffi::{CStr, CString};
use std::fmt::Formatter;
use std::str::FromStr;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde::de::{Error, Visitor};

/// A string that is guaranteed to be UTF-8 while also having its buffer being null-terminated.
#[repr(transparent)]
#[derive(Clone, PartialEq, Default, Debug)]
pub struct UTF8CString {
    inner: CString
}

impl core::fmt::Display for UTF8CString {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for UTF8CString {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for UTF8CString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>
    {
        deserializer.deserialize_str(UTF8CStringVisitor)
    }
}

struct UTF8CStringVisitor;
impl<'de> Visitor<'de> for UTF8CStringVisitor {
    type Value = UTF8CString;

    fn expecting(&self, formatter: &mut Formatter) -> std::fmt::Result {
        formatter.write_str("a string (with no nul bytes)")
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: Error
    {
        if v.contains(|i| i == 0u8 as char) {
            return Err(Error::custom("nul char found (not allowed)"))
        }

        Ok(UTF8CString::from_str(v))
    }

    fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
    where
        E: Error
    {
        if v.contains(|i| i == 0u8 as char) {
            return Err(Error::custom("nul char found (not allowed)"))
        }

        Ok(UTF8CString::from_str(v.as_str()))
    }
}

impl UTF8CString {
    #[inline]
    pub fn new<T: Into<Vec<u8>>>(what: T) -> Self {
        Self { inner: CString::new(what).expect("UTF8CString::new failed") }
    }

    #[inline]
    pub fn from_str(str: &str) -> Self {
        Self { inner: CString::from_str(str).expect("UTF8CString::from_str failed") }
    }

    #[inline]
    pub fn from_cstr(str: &CStr) -> Self {
        Self { inner: str.to_owned() }
    }

    #[inline]
    pub fn as_str(&self) -> &str {
        self.inner.to_str().unwrap()
    }

    #[inline]
    pub fn as_c_str(&self) -> &CStr {
        self.inner.as_c_str()
    }
}

impl AsRef<str> for UTF8CString {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// Reject a user-supplied file "name" (a save state, replay, etc.) that is not a bare filename: an
/// empty string, `.`/`..`, any path separator or other character Windows/POSIX filesystems treat
/// specially, a control character, or a trailing period/space (which Windows silently strips,
/// letting `"foo."` and `"foo"` collide and letting a name smuggle a differently-named file in).
/// See H4: without this a name like `../../evil` escapes the save/replay directory entirely.
pub(crate) fn check_user_file_name(name: &str) -> Result<(), UTF8CString> {
    if name.is_empty() {
        return Err("The name must not be empty.".into())
    }
    if name == "." || name == ".." {
        return Err(format!("{name:?} is not a valid name.").into())
    }
    const FORBIDDEN: [char; 9] = ['/', '\\', ':', '*', '?', '"', '<', '>', '|'];
    if let Some(c) = name.chars().find(|c| FORBIDDEN.contains(c) || c.is_control()) {
        return Err(format!("{name:?} contains a character that is not allowed ({c:?}).").into())
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err(format!("{name:?} must not end with a period or a space.").into())
    }
    Ok(())
}

/// Keep the last `max` bytes of `bytes`, decoding lossily. A cut that lands mid-codepoint leaves
/// `String::from_utf8_lossy` a stray leading U+FFFD replacement character standing in for the
/// truncated bytes; that is noise here (the string is already known to be a truncated tail, not a
/// decoding error), so it is stripped.
pub(crate) fn tail_utf8(bytes: &[u8], max: usize) -> String {
    let start = bytes.len().saturating_sub(max);
    let text = String::from_utf8_lossy(&bytes[start..]);
    let text: &str = text.strip_prefix('\u{FFFD}').unwrap_or(&text);
    text.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_utf8_is_always_valid_and_has_no_leading_replacement_char() {
        // 2500 repetitions of a 2-byte character is exactly 5000 bytes; an odd `max` cuts the
        // first character in half.
        let bytes = "é".repeat(2500).into_bytes();
        assert_eq!(bytes.len(), 5000);

        let tail = tail_utf8(&bytes, 4999);
        assert!(std::str::from_utf8(tail.as_bytes()).is_ok(), "must be valid UTF-8");
        assert!(!tail.starts_with('\u{FFFD}'), "the split leading byte must not surface as a replacement char");
        // The rest of the (now whole) characters are untouched.
        assert!(tail.chars().all(|c| c == 'é'));
    }

    #[test]
    fn tail_utf8_keeps_everything_when_under_the_limit() {
        let bytes = b"hello world".to_vec();
        assert_eq!(tail_utf8(&bytes, 4096), "hello world");
    }

    #[test]
    fn user_file_names_reject_traversal_and_reserved_characters() {
        assert!(check_user_file_name("My Save 1").is_ok());
        assert!(check_user_file_name("").is_err());
        assert!(check_user_file_name(".").is_err());
        assert!(check_user_file_name("..").is_err());
        assert!(check_user_file_name("../../evil").is_err(), "H4: path traversal must be rejected");
        assert!(check_user_file_name("..\\evil").is_err());
        assert!(check_user_file_name("a/b").is_err());
        assert!(check_user_file_name("a\\b").is_err());
        assert!(check_user_file_name("a:b").is_err());
        assert!(check_user_file_name("a*b").is_err());
        assert!(check_user_file_name("a?b").is_err());
        assert!(check_user_file_name("a\"b").is_err());
        assert!(check_user_file_name("a<b").is_err());
        assert!(check_user_file_name("a>b").is_err());
        assert!(check_user_file_name("a|b").is_err());
        assert!(check_user_file_name("a\tb").is_err(), "control characters must be rejected");
        assert!(check_user_file_name("trailing.").is_err());
        assert!(check_user_file_name("trailing ").is_err());
    }
}

impl From<&str> for UTF8CString {
    fn from(value: &str) -> Self {
        Self::from_str(value)
    }
}

impl From<String> for UTF8CString {
    fn from(value: String) -> Self {
        let mut bytes = value.into_bytes();
        bytes.push(0);
        let inner = CString::from_vec_with_nul(bytes).expect("UTF8CString::from::<String> fail");
        Self { inner }
    }
}
