//! The pure URL core of the `curl` example: parse `http://host[:port][/path]` (with
//! the scheme optional for user-typed URLs — [`parse_with_default_scheme`] prepends
//! `http://` and then runs the SAME hardened parse, so normalization sits before the
//! refusals, never around them), resolve a redirect target, refuse anything that could
//! smuggle bytes into the HTTP request line — and decode `Transfer-Encoding: chunked`
//! body framing
//! ([`dechunk`]; real HTTP/1.1 servers answer our HTTP/1.1 request chunked, as
//! example.com did on the board bench).
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

/// Does `url` spell a scheme? True for `<scheme>://…` (any RFC 3986 scheme shape:
/// ALPHA, then ALPHA/DIGIT/`+`/`-`/`.`, before the `://`), and for the two schemes this
/// client knows by name even without the `//` (`http:…`, `https:…` — so a missing-slash
/// typo refuses as "not an http:// URL" rather than mis-parsing as host plus port).
/// False for everything else — including `host:port/...` shapes, whose colon belongs to
/// the authority.
fn has_scheme(url: &str) -> bool {
    let head = url.split(['/', '?', '#']).next().unwrap_or("");
    if let Some((scheme, _)) = head.split_once(':')
        && (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
    {
        return true;
    }
    match url.find("://") {
        Some(at) => {
            let scheme = &url[..at];
            let mut chars = scheme.chars();
            chars.next().is_some_and(|c| c.is_ascii_alphabetic())
                && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        }
        None => false,
    }
}

/// Parse a *user-typed* URL with the scheme optional: `yager.io`, `yager.io/path`, and
/// `host:8080/x` get `http://` prepended, then run through the SAME hardened [`parse`]
/// (normalization happens BEFORE the request-injection refusals, never around them — a
/// control byte or space in a scheme-less URL still refuses typed). A URL that does
/// spell a scheme passes through untouched: `https://` stays the typed TLS refusal,
/// any other scheme stays [`UrlError::NotHttp`]. Server-supplied redirect targets get
/// NO such default — they keep the strict [`redirect_target`] rules.
pub fn parse_with_default_scheme(url: &str) -> Result<Url, UrlError> {
    if has_scheme(url) {
        parse(url)
    } else {
        parse(&format!("http://{url}"))
    }
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

/// The largest chunk size a single chunk may claim (16 MiB). Far above anything the
/// example will ever keep (its caller stores tens of KiB), so a size past this is a
/// hostile or corrupt framing — refused typed, never trusted arithmetic.
pub const CHUNK_SIZE_LIMIT: u64 = 16 * 1024 * 1024;

/// Why chunked framing could not be decoded (both are server-side faults).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkError {
    /// A chunk-size line is not hexadecimal; carries the offending line (lossily,
    /// trimmed) for the message.
    BadSizeLine(String),
    /// A chunk's size claim exceeds [`CHUNK_SIZE_LIMIT`].
    Oversize(u64),
    /// The CRLF that must follow a chunk's data is not there.
    MissingDataTerminator,
}

/// A decoded chunked body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dechunked {
    /// The de-framed body bytes.
    pub body: alloc::vec::Vec<u8>,
    /// The terminating 0-size chunk was seen (trailers, if any, are ignored). When
    /// `false`, `raw` ended mid-framing — the caller truncated its receive or the
    /// server cut the stream — and `body` carries what decoded so far.
    pub complete: bool,
}

/// First position of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Decode `Transfer-Encoding: chunked` framing (RFC 9112 §7.1): repeated
/// `<hex size>[;ext]\r\n<data>\r\n`, terminated by a 0-size chunk; trailers are
/// ignored. A truncated input is NOT an error — the caller bounds what it stores —
/// it yields `complete: false` with everything decoded so far. Malformed or
/// oversize framing IS an error (typed by the caller, never a trap).
pub fn dechunk(raw: &[u8]) -> Result<Dechunked, ChunkError> {
    let mut body = alloc::vec::Vec::new();
    let mut at = 0usize;
    loop {
        // The size line. No CRLF yet means the input ended mid-line: incomplete.
        let Some(line_len) = find(&raw[at..], b"\r\n") else {
            return Ok(Dechunked {
                body,
                complete: false,
            });
        };
        let line = &raw[at..at + line_len];
        // Chunk extensions (`;name=value`) are allowed and ignored.
        let size_text = match line.iter().position(|&b| b == b';') {
            Some(semi) => &line[..semi],
            None => line,
        };
        let size_text = size_text.trim_ascii();
        if size_text.is_empty() {
            return Err(ChunkError::BadSizeLine(
                String::from_utf8_lossy(line).into_owned(),
            ));
        }
        let mut size: u64 = 0;
        for &byte in size_text {
            let digit = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => {
                    return Err(ChunkError::BadSizeLine(
                        String::from_utf8_lossy(line).into_owned(),
                    ));
                }
            };
            size = size * 16 + u64::from(digit);
            if size > CHUNK_SIZE_LIMIT {
                return Err(ChunkError::Oversize(size));
            }
        }
        if size == 0 {
            // The last chunk; trailers (if any) are deliberately ignored.
            return Ok(Dechunked {
                body,
                complete: true,
            });
        }
        let data_start = at + line_len + 2;
        let available = raw.len().saturating_sub(data_start);
        if (size as usize) > available {
            // The input ends inside this chunk's data: incomplete, keep the prefix.
            body.extend_from_slice(&raw[data_start.min(raw.len())..]);
            return Ok(Dechunked {
                body,
                complete: false,
            });
        }
        let data_end = data_start + size as usize;
        body.extend_from_slice(&raw[data_start..data_end]);
        if raw.len() < data_end + 2 {
            // The CRLF after the data did not arrive: incomplete.
            return Ok(Dechunked {
                body,
                complete: false,
            });
        }
        if &raw[data_end..data_end + 2] != b"\r\n" {
            return Err(ChunkError::MissingDataTerminator);
        }
        at = data_end + 2;
    }
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

    // --- the scheme-optional user spelling ----------------------------------------

    #[test]
    fn defaults_the_scheme_for_user_urls() {
        assert_eq!(
            parse_with_default_scheme("yager.io"),
            Ok(url("yager.io", 80, "/"))
        );
        assert_eq!(
            parse_with_default_scheme("yager.io/a/b?q=1"),
            Ok(url("yager.io", 80, "/a/b?q=1"))
        );
        // A bare authority colon is a port, not a scheme.
        assert_eq!(
            parse_with_default_scheme("localhost:8080/x"),
            Ok(url("localhost", 8080, "/x"))
        );
        assert_eq!(
            parse_with_default_scheme("10.0.2.2:8080/hello.txt"),
            Ok(url("10.0.2.2", 8080, "/hello.txt"))
        );
        // A spelled scheme passes through to the strict parse untouched.
        assert_eq!(
            parse_with_default_scheme("http://yager.io/p"),
            Ok(url("yager.io", 80, "/p"))
        );
        // Fragments still never travel, scheme spelled or not.
        assert_eq!(
            parse_with_default_scheme("yager.io#frag"),
            Ok(url("yager.io", 80, "/"))
        );
    }

    #[test]
    fn keeps_the_typed_refusals_under_the_default_scheme() {
        // https stays the typed TLS refusal; other schemes stay not-http.
        assert_eq!(
            parse_with_default_scheme("https://yager.io"),
            Err(UrlError::Https)
        );
        assert_eq!(
            parse_with_default_scheme("HTTPS://yager.io"),
            Err(UrlError::Https)
        );
        assert_eq!(
            parse_with_default_scheme("ftp://yager.io"),
            Err(UrlError::NotHttp)
        );
        assert_eq!(
            parse_with_default_scheme("gopher+x://y"),
            Err(UrlError::NotHttp)
        );
        // The missing-slashes typo refuses as not-http (never mis-parsed as a port).
        assert_eq!(
            parse_with_default_scheme("http:yager.io"),
            Err(UrlError::NotHttp)
        );
        assert_eq!(
            parse_with_default_scheme("https:yager.io"),
            Err(UrlError::NotHttp)
        );
        // The odd-authority refusals run on the normalized form too.
        assert_eq!(
            parse_with_default_scheme("user@yager.io"),
            Err(UrlError::Userinfo)
        );
        assert_eq!(
            parse_with_default_scheme("[::1]:80/"),
            Err(UrlError::Ipv6Literal)
        );
        assert_eq!(
            parse_with_default_scheme("yager.io:0"),
            Err(UrlError::BadPort(String::from("0")))
        );
        assert_eq!(parse_with_default_scheme(""), Err(UrlError::NoHost));
    }

    #[test]
    fn refuses_injection_bytes_through_the_default_scheme() {
        // Normalization happens BEFORE the hardened parse: a scheme-less URL carrying
        // CR/LF/space hits exactly the same request-injection refusal.
        assert_eq!(
            parse_with_default_scheme("yager.io/\r\nX-Evil: 1"),
            Err(UrlError::ForbiddenByte)
        );
        assert_eq!(
            parse_with_default_scheme("ya\nger.io/"),
            Err(UrlError::ForbiddenByte)
        );
        assert_eq!(
            parse_with_default_scheme("yager.io/a b"),
            Err(UrlError::ForbiddenByte)
        );
        // Percent sequences stay inert text, exactly as on the spelled-scheme path.
        assert_eq!(
            parse_with_default_scheme("yager.io/%0d%0a"),
            Ok(url("yager.io", 80, "/%0d%0a"))
        );
    }

    #[test]
    fn the_default_scheme_never_applies_to_redirect_targets() {
        // A server-supplied Location keeps the strict rules: a scheme-less absolute
        // target is still refused (the documented limitation), not silently fetched.
        let current = url("yager.io", 80, "/old");
        assert_eq!(
            redirect_target(&current, "evil.example/payload"),
            Err(UrlError::NotHttp)
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

    // --- chunked-framing decode (the board round's example.com answered chunked) --

    #[test]
    fn dechunk_decodes_an_exact_body() {
        // Two chunks + terminator: the de-framed body, byte for byte, complete.
        let raw = b"5\r\nhello\r\n8\r\n, world\n\r\n0\r\n\r\n";
        assert_eq!(
            dechunk(raw),
            Ok(Dechunked {
                body: b"hello, world\n".to_vec(),
                complete: true,
            })
        );
        // Hex sizes (upper and lower case) and chunk extensions are honored; the
        // body may itself contain CRLF (framing is by count, not by delimiter).
        let raw = b"A;ext=1\r\n0123456789\r\n1F\r\nabcdefghijklmnopqrstuvwxyz\r\n123\r\n0\r\n\r\n";
        let decoded = dechunk(raw).expect("valid framing");
        assert!(decoded.complete);
        assert_eq!(decoded.body.len(), 10 + 31);
        assert!(decoded.body.starts_with(b"0123456789abcdef"));
        // Trailers after the 0 chunk are ignored.
        let raw = b"3\r\nabc\r\n0\r\nExpires: never\r\n\r\n";
        assert_eq!(
            dechunk(raw),
            Ok(Dechunked {
                body: b"abc".to_vec(),
                complete: true,
            })
        );
    }

    #[test]
    fn dechunk_reports_truncation_honestly() {
        // Cut mid-data: the prefix decodes, complete stays false.
        let raw = b"10\r\nonly-seven";
        assert_eq!(
            dechunk(raw),
            Ok(Dechunked {
                body: b"only-seven".to_vec(),
                complete: false,
            })
        );
        // Cut mid-size-line, and cut between the data and its CRLF.
        assert!(!dechunk(b"3\r\nabc\r\n1A").unwrap().complete);
        assert!(!dechunk(b"3\r\nabc").unwrap().complete);
        assert_eq!(
            dechunk(b"").unwrap(),
            Dechunked {
                body: alloc::vec::Vec::new(),
                complete: false
            }
        );
    }

    #[test]
    fn dechunk_refuses_malformed_and_oversize_framing() {
        // A size line that is not hex (e.g. a Content-Length-style decimal would
        // pass — hex digits — but letters past 'f' cannot).
        assert_eq!(
            dechunk(b"zz\r\ndata\r\n0\r\n\r\n"),
            Err(ChunkError::BadSizeLine(String::from("zz")))
        );
        // An empty size line.
        assert_eq!(
            dechunk(b"\r\nabc\r\n0\r\n\r\n"),
            Err(ChunkError::BadSizeLine(String::new()))
        );
        // The byte after a chunk's data must be CRLF.
        assert_eq!(
            dechunk(b"3\r\nabcXX0\r\n\r\n"),
            Err(ChunkError::MissingDataTerminator)
        );
        // A size claim past the 16 MiB limit is refused, not trusted (the check
        // runs during accumulation, so huge claims trip on their first excess
        // digit and can never overflow).
        assert_eq!(
            dechunk(b"1000001\r\n"),
            Err(ChunkError::Oversize(0x100_0001))
        );
        assert_eq!(
            dechunk(b"FFFFFFFFFFFFFFFFFF\r\n"),
            Err(ChunkError::Oversize(0xFFF_FFFF))
        );
        // ...and the limit itself is fine as a claim (truncated input, not error).
        assert!(!dechunk(b"1000000\r\nx").unwrap().complete);
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
