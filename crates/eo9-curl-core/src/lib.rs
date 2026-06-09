//! The pure URL core of the `curl` example: parse `http://host[:port][/path]`,
//! resolve a redirect target, and refuse anything that could smuggle bytes into the
//! HTTP request line.
//!
//! Security rule (the reason this crate exists as a host-tested unit): the host and
//! path feed `GET <path> HTTP/1.1\r\nHost: <host>\r\n…` verbatim, so **spaces and
//! control bytes are refused** — a raw CR or LF would inject arbitrary
//! headers/requests. The rule is applied to BOTH attacker-influenced inputs:
//!
//! * the user-supplied URL ([`parse`]), and
//! * a server-supplied `Location` value ([`redirect_target`]) — the sneakier one: a
//!   bare LF survives CRLF-splitting of a response head, so a hostile or
//!   compromised server could otherwise steer the follow-up request.
//!
//! Spaces are refused alongside (a URL space must be percent-encoded anyway, and a
//! space in the request line desynchronizes it). Percent sequences are passed
//! through verbatim — this client never percent-decodes, so `%0d%0a` stays inert
//! text on the wire.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};

/// The default URL port (http).
pub const DEFAULT_PORT: u16 = 80;

/// A parsed `http://` URL: host (name or literal), port, absolute path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl Url {
    /// The `Host:` header value: the authority, port included only when it is not
    /// the default (RFC 7230 §5.4 shape).
    pub fn host_header(&self) -> String {
        if self.port == DEFAULT_PORT {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// The URL back as text (for narration).
    pub fn display(&self) -> String {
        format!("http://{}{}", self.host_header(), self.path)
    }
}

/// Why a URL (or a redirect target) is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlError {
    /// `https://`: TLS is a deferred decision — the caller names it in its message.
    Https,
    /// Not an `http://` URL at all.
    NotHttp,
    /// The host or path carries a space or a control byte — the request-injection
    /// refusal (never weakened: these bytes would reach the request line verbatim).
    ForbiddenByte,
    /// `user@host` authorities are not supported.
    Userinfo,
    /// Bracketed/multi-colon authorities (IPv6 literals) are not supported.
    Ipv6Literal,
    /// The port is not a number in 1..=65535; carries the offending text.
    BadPort(String),
    /// The authority is empty.
    NoHost,
}

/// Is `c` refused in a host or path? Control bytes (including DEL) and space — the
/// bytes that would desynchronize or extend the HTTP request line.
fn forbidden(c: char) -> bool {
    c <= ' ' || c == '\u{7f}'
}

/// Parse `http://host[:port][/path]`. `https://` is [`UrlError::Https`] (typed for
/// the caller to name the deferred-TLS decision); anything else that is not
/// `http://` is [`UrlError::NotHttp`].
pub fn parse(url: &str) -> Result<Url, UrlError> {
    let bytes = url.as_bytes();
    if bytes.len() >= 8 && bytes[..8].eq_ignore_ascii_case(b"https://") {
        return Err(UrlError::Https);
    }
    if !(bytes.len() >= 7 && bytes[..7].eq_ignore_ascii_case(b"http://")) {
        return Err(UrlError::NotHttp);
    }
    let rest = &url[7..];
    // A fragment never travels on the wire; drop it BEFORE the authority/path split
    // (it may follow the authority directly, with no path: `http://host#frag`).
    let rest = match rest.find('#') {
        Some(hash) => &rest[..hash],
        None => rest,
    };
    let (authority, path) = match rest.find('/') {
        Some(slash) => (&rest[..slash], &rest[slash..]),
        None => (rest, "/"),
    };
    if authority.contains('@') {
        return Err(UrlError::Userinfo);
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port_text)) => {
            if host.contains(':') {
                return Err(UrlError::Ipv6Literal);
            }
            let port = port_text
                .parse::<u16>()
                .ok()
                .filter(|&port| port != 0)
                .ok_or_else(|| UrlError::BadPort(port_text.to_string()))?;
            (host, port)
        }
        None => (authority, DEFAULT_PORT),
    };
    if host.is_empty() {
        return Err(UrlError::NoHost);
    }
    let path = if path.is_empty() { "/" } else { path };
    // The request-injection refusal, on everything that reaches the request line.
    if host.chars().any(forbidden) || path.chars().any(forbidden) {
        return Err(UrlError::ForbiddenByte);
    }
    Ok(Url {
        host: host.to_string(),
        port,
        path: path.to_string(),
    })
}

/// The redirect target: an absolute http:// URL, or a server-relative path on the
/// same authority. The `Location` value is server-supplied — it gets exactly the
/// same refusals as a user URL (https typed, control bytes/spaces refused), BEFORE
/// any connection is attempted.
pub fn redirect_target(current: &Url, location: &str) -> Result<Url, UrlError> {
    if location.starts_with('/') {
        if location.chars().any(forbidden) {
            return Err(UrlError::ForbiddenByte);
        }
        return Ok(Url {
            host: current.host.clone(),
            port: current.port,
            path: location.to_string(),
        });
    }
    parse(location)
}

/// A strict dotted quad, or `None` (the host then goes to DNS).
pub fn parse_ipv4(text: &str) -> Option<(u8, u8, u8, u8)> {
    let mut octets = [0u8; 4];
    let mut count = 0;
    for part in text.split('.') {
        if count == 4 {
            return None;
        }
        octets[count] = part.parse::<u8>().ok()?;
        count += 1;
    }
    if count != 4 {
        return None;
    }
    Some((octets[0], octets[1], octets[2], octets[3]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(host: &str, port: u16, path: &str) -> Url {
        Url {
            host: host.to_string(),
            port,
            path: path.to_string(),
        }
    }

    #[test]
    fn parses_the_plain_shapes() {
        assert_eq!(parse("http://example.com"), Ok(url("example.com", 80, "/")));
        assert_eq!(
            parse("http://10.0.2.2:8080/hello.txt"),
            Ok(url("10.0.2.2", 8080, "/hello.txt"))
        );
        assert_eq!(
            parse("HTTP://Example.com/a/b?q=1"),
            Ok(url("Example.com", 80, "/a/b?q=1"))
        );
        // Fragments never travel.
        assert_eq!(
            parse("http://example.com/page#frag"),
            Ok(url("example.com", 80, "/page"))
        );
        assert_eq!(
            parse("http://example.com#frag"),
            Ok(url("example.com", 80, "/"))
        );
    }

    #[test]
    fn refuses_https_typed_and_non_http_plainly() {
        assert_eq!(parse("https://example.com"), Err(UrlError::Https));
        assert_eq!(parse("HTTPS://example.com"), Err(UrlError::Https));
        assert_eq!(parse("ftp://example.com"), Err(UrlError::NotHttp));
        assert_eq!(parse("example.com"), Err(UrlError::NotHttp));
        assert_eq!(parse(""), Err(UrlError::NotHttp));
    }

    #[test]
    fn refuses_the_odd_authorities() {
        assert_eq!(parse("http://user@example.com"), Err(UrlError::Userinfo));
        assert_eq!(parse("http://[::1]:80/"), Err(UrlError::Ipv6Literal));
        assert_eq!(parse("http://"), Err(UrlError::NoHost));
        assert_eq!(parse("http:///x"), Err(UrlError::NoHost));
        assert_eq!(
            parse("http://example.com:notaport/"),
            Err(UrlError::BadPort(String::from("notaport")))
        );
        assert_eq!(
            parse("http://example.com:0/"),
            Err(UrlError::BadPort(String::from("0")))
        );
    }

    // --- the request-injection refusals (the reason this crate exists) -----------

    #[test]
    fn refuses_control_bytes_in_a_user_url() {
        // A raw CR/LF in the path would inject headers into the request line.
        assert_eq!(
            parse("http://example.com/\r\nX-Evil: 1"),
            Err(UrlError::ForbiddenByte)
        );
        // ...or in the host, into the Host header.
        assert_eq!(parse("http://exam\nple.com/"), Err(UrlError::ForbiddenByte));
        // Spaces desynchronize the request line; URLs must carry %20.
        assert_eq!(
            parse("http://example.com/a b"),
            Err(UrlError::ForbiddenByte)
        );
        // Percent sequences are NOT decoded — inert text, allowed through.
        assert_eq!(
            parse("http://example.com/%0d%0a"),
            Ok(url("example.com", 80, "/%0d%0a"))
        );
    }

    #[test]
    fn refuses_control_bytes_in_a_server_redirect() {
        let current = url("example.com", 80, "/old");
        // A bare LF survives CRLF-splitting of a response head, so a hostile
        // server's Location can carry one — refused before any request is built,
        // in both the relative and the absolute form.
        assert_eq!(
            redirect_target(&current, "/new\nX-Evil: 1"),
            Err(UrlError::ForbiddenByte)
        );
        assert_eq!(
            redirect_target(&current, "http://example.com/new\nX-Evil: 1"),
            Err(UrlError::ForbiddenByte)
        );
        // And an https redirect stays the typed TLS refusal.
        assert_eq!(
            redirect_target(&current, "https://example.com/new"),
            Err(UrlError::Https)
        );
    }

    #[test]
    fn resolves_redirect_targets() {
        let current = url("example.com", 8080, "/old?x=1");
        assert_eq!(
            redirect_target(&current, "/new"),
            Ok(url("example.com", 8080, "/new"))
        );
        assert_eq!(
            redirect_target(&current, "http://other.example/p"),
            Ok(url("other.example", 80, "/p"))
        );
        // A relative-without-slash target is not resolved (documented limitation);
        // it falls through to the plain-URL refusal.
        assert_eq!(
            redirect_target(&current, "new.html"),
            Err(UrlError::NotHttp)
        );
    }

    #[test]
    fn parses_strict_dotted_quads_only() {
        assert_eq!(parse_ipv4("10.0.2.2"), Some((10, 0, 2, 2)));
        assert_eq!(parse_ipv4("255.255.255.255"), Some((255, 255, 255, 255)));
        assert_eq!(parse_ipv4("example.com"), None);
        assert_eq!(parse_ipv4("1.2.3"), None);
        assert_eq!(parse_ipv4("1.2.3.4.5"), None);
        assert_eq!(parse_ipv4("256.1.1.1"), None);
        assert_eq!(parse_ipv4(""), None);
    }
}
