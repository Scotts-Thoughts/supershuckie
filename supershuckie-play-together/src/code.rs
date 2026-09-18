//! Join codes (`host:port` strings) and the local-address probe.

use std::fmt;
use std::net::{IpAddr, Ipv6Addr, UdpSocket};

use crate::DEFAULT_PORT;

/// Where a host can be reached: what the host shows and what a client types.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct JoinCode {
    /// Host name, IPv4 address or bare IPv6 address (no brackets).
    pub host: String,
    /// TCP port.
    pub port: u16,
}

/// Why a join code does not parse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JoinCodeError {
    /// Nothing but whitespace.
    Empty,
    /// A bracketed IPv6 address with no `:port` after the bracket, or a trailing colon.
    MissingPort,
    /// The part after the last colon is not a port number (1-65535).
    BadPort,
    /// The host part is not a plausible host name or address.
    BadHost(String),
    /// Looks like a URL (`http://...`).
    HasScheme,
    /// Whitespace inside the code.
    HasWhitespace,
}

impl fmt::Display for JoinCodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JoinCodeError::Empty => f.write_str("the join code is empty"),
            JoinCodeError::MissingPort => f.write_str("the join code needs a port after the host"),
            JoinCodeError::BadPort => f.write_str("the port is not a number between 1 and 65535"),
            JoinCodeError::BadHost(host) => write!(f, "'{host}' is not a valid host name or address"),
            JoinCodeError::HasScheme => f.write_str("the join code is not a URL: leave out the 'http://' part"),
            JoinCodeError::HasWhitespace => f.write_str("the join code must not contain spaces"),
        }
    }
}

impl std::error::Error for JoinCodeError {}

impl JoinCode {
    /// Parse `host:port`, `[v6]:port`, or a bare host (which gets [`DEFAULT_PORT`]).
    ///
    /// Leading and trailing whitespace is trimmed; whitespace inside, a URL scheme, and an empty
    /// code are refused.
    pub fn parse(s: &str) -> Result<JoinCode, JoinCodeError> {
        let s = s.trim();
        if s.is_empty() {
            return Err(JoinCodeError::Empty);
        }
        if s.contains("://") {
            return Err(JoinCodeError::HasScheme);
        }
        if s.chars().any(char::is_whitespace) {
            return Err(JoinCodeError::HasWhitespace);
        }

        // Bracketed IPv6: "[::1]" or "[::1]:port".
        if let Some(rest) = s.strip_prefix('[') {
            let Some((inside, after)) = rest.split_once(']') else {
                return Err(JoinCodeError::BadHost(s.to_owned()));
            };
            let host = match inside.parse::<Ipv6Addr>() {
                Ok(addr) => addr.to_string(),
                Err(_) => return Err(JoinCodeError::BadHost(inside.to_owned())),
            };
            let port = match after {
                "" => DEFAULT_PORT,
                _ => match after.strip_prefix(':') {
                    Some("") => return Err(JoinCodeError::MissingPort),
                    Some(port) => parse_port(port)?,
                    None => return Err(JoinCodeError::BadHost(s.to_owned())),
                },
            };
            return Ok(JoinCode { host, port });
        }

        // A bare IPv6 address (two or more colons, no brackets) has no port.
        if s.matches(':').count() >= 2 {
            return match s.parse::<Ipv6Addr>() {
                Ok(addr) => Ok(JoinCode { host: addr.to_string(), port: DEFAULT_PORT }),
                Err(_) => Err(JoinCodeError::BadHost(s.to_owned())),
            };
        }

        let (host, port) = match s.rsplit_once(':') {
            Some((host, "")) => (host, Err(JoinCodeError::MissingPort)),
            Some((host, port)) => (host, parse_port(port)),
            None => (s, Ok(DEFAULT_PORT)),
        };
        if !valid_host(host) {
            return Err(JoinCodeError::BadHost(host.to_owned()));
        }
        Ok(JoinCode { host: host.to_owned(), port: port? })
    }

    /// The string form: `host:port`, with an IPv6 address in brackets.
    pub fn format(&self) -> String {
        if self.host.parse::<Ipv6Addr>().is_ok() {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

impl fmt::Display for JoinCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.format())
    }
}

fn parse_port(s: &str) -> Result<u16, JoinCodeError> {
    match s.parse::<u16>() {
        Ok(0) | Err(_) => Err(JoinCodeError::BadPort),
        Ok(port) => Ok(port),
    }
}

/// A host name or IPv4 literal: letters, digits, `-`, `.` and `_`, non-empty labels, no leading
/// or trailing dot, at most 253 bytes.
fn valid_host(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    if host.parse::<IpAddr>().is_ok() {
        return true;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    })
}

/// The IP address the local machine would use to reach the internet, if any.
///
/// Uses the UDP "connect" trick: connecting a UDP socket to `192.0.2.1:9` (TEST-NET-1, never
/// routed) makes the OS pick the outgoing interface without sending a packet, and the socket's
/// local address is that interface's. `None` when offline or when the OS refuses.
pub fn probe_local_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let addr = socket.local_addr().ok()?.ip();
    if addr.is_unspecified() { None } else { Some(addr) }
}
