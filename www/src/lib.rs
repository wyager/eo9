//! The eo9.org web server.
//!
//! This crate serves the static site in `site/` directly to the internet: plain HTTP for
//! local development, and HTTPS with certificates either obtained automatically from
//! Let's Encrypt (ACME, TLS-ALPN-01) or loaded from PEM files. The library contains
//! everything the binary does apart from reading `std::env`, so the request-handling logic
//! (path resolution, content types, cache headers) is unit-testable and the listeners are
//! integration-testable on ephemeral ports (see `tests/`).

pub mod server;
pub mod tls;

use std::fs;
use std::path::{Path, PathBuf};

/// Default HTTP bind address in plain-HTTP (development) mode.
pub const DEFAULT_HTTP_BIND: &str = "127.0.0.1:8080";
/// Default HTTP bind address in the TLS modes (the redirect-to-HTTPS listener).
pub const DEFAULT_REDIRECT_BIND: &str = "0.0.0.0:80";
/// Default HTTPS bind address in the TLS modes.
pub const DEFAULT_HTTPS_BIND: &str = "0.0.0.0:443";
/// Default site directory, relative to the working directory.
pub const DEFAULT_SITE: &str = "site";
/// Default ACME certificate-cache directory, relative to the working directory.
pub const DEFAULT_ACME_CACHE: &str = "acme-cache";

/// How the server terminates (or does not terminate) TLS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Serve plain HTTP on the HTTP bind address. Development mode; no TLS.
    PlainHttp,
    /// Serve HTTPS with a certificate chain and private key loaded from PEM files,
    /// plus an HTTP listener that redirects to HTTPS.
    ManualTls { cert: PathBuf, key: PathBuf },
    /// Serve HTTPS with certificates obtained and renewed automatically from Let's Encrypt
    /// (TLS-ALPN-01 challenges), plus an HTTP listener that redirects to HTTPS.
    Acme {
        domains: Vec<String>,
        email: String,
        cache_dir: PathBuf,
        staging: bool,
    },
}

/// Server configuration: where to listen, what to serve, and how to terminate TLS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub site_root: PathBuf,
    /// Plain-HTTP listener: serves the site in `PlainHttp` mode, redirects to HTTPS otherwise.
    pub http_bind: String,
    /// HTTPS listener address; unused in `PlainHttp` mode.
    pub https_bind: String,
    pub mode: Mode,
}

/// Usage text for `--help`.
pub const USAGE: &str = "eo9-www: the eo9.org web server

Usage:
  eo9-www [--site DIR] [--bind ADDR:PORT]                                   # plain HTTP (development)
  eo9-www --domain NAME [--domain NAME ...] --acme-email ADDR [options]     # HTTPS via Let's Encrypt
  eo9-www --tls-cert FILE --tls-key FILE [options]                          # HTTPS with provided cert

Options:
  --site DIR          directory to serve        (env EO9_WWW_SITE, default ./site)
  --bind ADDR:PORT    HTTP listener             (env EO9_WWW_BIND; default 127.0.0.1:8080,
                      or 0.0.0.0:80 in the HTTPS modes, where it only redirects to HTTPS)
  --https-bind ADDR:PORT
                      HTTPS listener            (env EO9_WWW_HTTPS_BIND, default 0.0.0.0:443)
  --domain NAME       domain to obtain a certificate for (repeatable; first one is canonical)
  --acme-email ADDR   contact email for the Let's Encrypt account
  --acme-cache DIR    certificate/account cache directory (default ./acme-cache)
  --acme-staging      use the Let's Encrypt staging environment (untrusted test certificates)
  --tls-cert FILE     PEM certificate chain (manual TLS mode)
  --tls-key FILE      PEM private key (manual TLS mode)
  --help              print this message

TLS is terminated by the server itself; there is no reverse proxy. See www/README.md for
standalone deployment (DNS, ports 80/443, certificate cache).";

/// Parse configuration from command-line arguments and the environment.
///
/// Precedence: flag > environment variable > default. `args` is the argument list *without*
/// the program name; `env` looks up environment variables (passed in as a function so
/// parsing stays pure and testable).
pub fn parse_config<I>(args: I, env: impl Fn(&str) -> Option<String>) -> Result<Config, String>
where
    I: IntoIterator<Item = String>,
{
    let mut site = None;
    let mut http_bind = None;
    let mut https_bind = None;
    let mut domains: Vec<String> = Vec::new();
    let mut acme_email = None;
    let mut acme_cache = None;
    let mut acme_staging = false;
    let mut tls_cert = None;
    let mut tls_key = None;

    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let mut value = |flag: &str| {
            args.next()
                .ok_or_else(|| format!("{flag} requires a value"))
        };
        match arg.as_str() {
            "--site" => site = Some(value("--site")?),
            "--bind" => http_bind = Some(value("--bind")?),
            "--https-bind" => https_bind = Some(value("--https-bind")?),
            "--domain" => domains.push(value("--domain")?),
            "--acme-email" => acme_email = Some(value("--acme-email")?),
            "--acme-cache" => acme_cache = Some(value("--acme-cache")?),
            "--acme-staging" => acme_staging = true,
            "--tls-cert" => tls_cert = Some(value("--tls-cert")?),
            "--tls-key" => tls_key = Some(value("--tls-key")?),
            other => return Err(format!("unrecognized argument `{other}` (see --help)")),
        }
    }

    let acme_requested =
        !domains.is_empty() || acme_email.is_some() || acme_cache.is_some() || acme_staging;
    let manual_requested = tls_cert.is_some() || tls_key.is_some();
    if acme_requested && manual_requested {
        return Err("choose either ACME (--domain …) or a provided certificate \
                    (--tls-cert/--tls-key), not both"
            .to_owned());
    }
    if manual_requested && (tls_cert.is_none() || tls_key.is_none()) {
        return Err("--tls-cert and --tls-key must be given together".to_owned());
    }
    if acme_requested && domains.is_empty() {
        return Err("--acme-email/--acme-cache/--acme-staging require --domain".to_owned());
    }

    let mode = if !domains.is_empty() {
        let email = acme_email.ok_or("ACME mode requires --acme-email")?;
        Mode::Acme {
            domains,
            email,
            cache_dir: PathBuf::from(acme_cache.unwrap_or_else(|| DEFAULT_ACME_CACHE.to_owned())),
            staging: acme_staging,
        }
    } else if let (Some(cert), Some(key)) = (tls_cert, tls_key) {
        Mode::ManualTls {
            cert: PathBuf::from(cert),
            key: PathBuf::from(key),
        }
    } else {
        Mode::PlainHttp
    };

    if mode == Mode::PlainHttp && https_bind.is_some() {
        return Err("--https-bind only applies with --domain or --tls-cert/--tls-key".to_owned());
    }

    let default_http_bind = if mode == Mode::PlainHttp {
        DEFAULT_HTTP_BIND
    } else {
        DEFAULT_REDIRECT_BIND
    };
    let http_bind = http_bind
        .or_else(|| env("EO9_WWW_BIND"))
        .unwrap_or_else(|| default_http_bind.to_owned());
    let https_bind = https_bind
        .or_else(|| env("EO9_WWW_HTTPS_BIND"))
        .unwrap_or_else(|| DEFAULT_HTTPS_BIND.to_owned());
    let site = site
        .or_else(|| env("EO9_WWW_SITE"))
        .unwrap_or_else(|| DEFAULT_SITE.to_owned());

    Ok(Config {
        site_root: PathBuf::from(site),
        http_bind,
        https_bind,
        mode,
    })
}

/// Why a request path could not be resolved to a file under the site root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveError {
    /// The URL was malformed (bad percent-encoding, embedded NUL): respond 400.
    BadRequest,
    /// The path does not name a servable file (missing, a traversal attempt, or outside
    /// the site root): respond 404.
    NotFound,
}

/// Resolve a request URL to a file inside `site_root`.
///
/// Guarantees: the returned path is a canonicalized regular file located under the
/// canonicalized site root. `..` components, percent-encoded traversal (`%2e%2e`),
/// backslashes, and symlinks escaping the root are all rejected. Directory paths
/// (including `/`) resolve to their `index.html`.
pub fn resolve(site_root: &Path, request_url: &str) -> Result<PathBuf, ResolveError> {
    // The request target is the path plus an optional query string; only the path matters.
    let raw_path = request_url.split(['?', '#']).next().unwrap_or("");
    let decoded = percent_decode(raw_path).ok_or(ResolveError::BadRequest)?;

    let mut candidate = site_root.to_path_buf();
    for segment in decoded.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." || segment.contains('\\') {
            return Err(ResolveError::NotFound);
        }
        candidate.push(segment);
    }
    if candidate.is_dir() {
        candidate.push("index.html");
    }

    // Canonicalize both sides and re-check containment so that symlinks inside the site
    // directory can never lead outside it. A path that fails to canonicalize does not exist.
    let root = fs::canonicalize(site_root).map_err(|_| ResolveError::NotFound)?;
    let file = fs::canonicalize(&candidate).map_err(|_| ResolveError::NotFound)?;
    if !file.starts_with(&root) || !file.is_file() {
        return Err(ResolveError::NotFound);
    }
    Ok(file)
}

/// Decode percent-escapes. Returns `None` for malformed escapes, non-UTF-8 results, or
/// embedded NUL bytes.
fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = hex_value(*bytes.get(i + 1)?)?;
            let lo = hex_value(*bytes.get(i + 2)?)?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let decoded = String::from_utf8(out).ok()?;
    if decoded.contains('\0') {
        None
    } else {
        Some(decoded)
    }
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// A pre-compressed representation the server can serve from a sibling file written by
/// `www/precompress` (`<file>.br` / `<file>.gz`), negotiated via `Accept-Encoding`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Brotli,
    Gzip,
}

impl Encoding {
    /// The `Content-Encoding` token sent on the wire.
    pub fn token(self) -> &'static str {
        match self {
            Encoding::Brotli => "br",
            Encoding::Gzip => "gzip",
        }
    }

    /// The sibling-file extension the precompressor writes.
    pub fn file_extension(self) -> &'static str {
        match self {
            Encoding::Brotli => "br",
            Encoding::Gzip => "gz",
        }
    }
}

/// Parse an `Accept-Encoding` header value into the encodings the client accepts, in the
/// server's preference order (brotli before gzip). A missing header accepts neither; a
/// wildcard accepts both; an explicit `q=0` refuses one. Only encodings we can actually
/// serve (from pre-compressed siblings) are reported.
pub fn accepted_encodings(accept_encoding: Option<&str>) -> Vec<Encoding> {
    let Some(value) = accept_encoding else {
        return Vec::new();
    };
    let mut brotli = None;
    let mut gzip = None;
    let mut wildcard = None;
    for entry in value.split(',') {
        let mut parts = entry.split(';');
        let token = parts.next().unwrap_or("").trim().to_ascii_lowercase();
        // q defaults to 1; a malformed q-value is treated as acceptable rather than refused.
        let quality = parts
            .filter_map(|param| {
                let (name, value) = param.split_once('=')?;
                name.trim().eq_ignore_ascii_case("q").then_some(value)
            })
            .next_back()
            .map(|q| q.trim().parse::<f32>().unwrap_or(1.0));
        let acceptable = quality.is_none_or(|q| q > 0.0);
        match token.as_str() {
            "br" => brotli = Some(acceptable),
            "gzip" | "x-gzip" => gzip = Some(acceptable),
            "*" => wildcard = Some(acceptable),
            _ => {}
        }
    }
    let mut accepted = Vec::new();
    if brotli.or(wildcard).unwrap_or(false) {
        accepted.push(Encoding::Brotli);
    }
    if gzip.or(wildcard).unwrap_or(false) {
        accepted.push(Encoding::Gzip);
    }
    accepted
}

/// The `Content-Type` value for a file, chosen by extension.
pub fn content_type(path: &Path) -> &'static str {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match extension.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "txt" | "md" => "text/plain; charset=utf-8",
        "svg" => "image/svg+xml",
        "json" => "application/json",
        "xml" => "application/xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "wasm" => "application/wasm",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// Is this a content-fingerprinted immutable asset (`name.<16-hex>.wasm` / `.cwasm`)?
///
/// The web-VM build (`cargo xtask fingerprint-web-vm`) renames the large immutable assets —
/// the wasm blob and the Pulley `.cwasm` store images — to carry a hash of their contents in
/// the filename, and points the page at them through `vm/assets.json`. The URL therefore *is*
/// the version: a different build yields a different URL, so these can be cached forever and
/// never revalidated, and the server never hashes their bodies on the request path. The check
/// is a cheap filename test (no I/O, no hashing): the stem's final dot-segment is exactly 16
/// lowercase hex digits and the extension is one we fingerprint.
pub fn is_fingerprinted(path: &Path) -> bool {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if !matches!(extension.as_str(), "wasm" | "cwasm") {
        return false;
    }
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    match stem.rsplit_once('.') {
        Some((base, hash)) => {
            !base.is_empty()
                && hash.len() == 16
                && hash
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }
        None => false,
    }
}

/// The `Cache-Control` value for a file.
///
/// The policy exists so that **a deploy never needs a CDN purge**:
///
/// - Content-fingerprinted assets (see [`is_fingerprinted`]) are immutable: their URL changes
///   whenever their bytes do, so they get a one-year lifetime plus `immutable` — Cloudflare and
///   browsers hold them indefinitely and never revalidate, and a new OS build simply produces a
///   new URL (nothing to purge). These are the only files that may be cached past a deploy.
/// - Every *mutable-in-place* text asset — HTML, scripts, styles, the `assets.json` manifest,
///   and any non-fingerprinted wasm (a dev build before fingerprinting) — is served `no-cache`:
///   caches may store it but must revalidate before reuse, so a deploy is visible on the next
///   request with no purge. The strong [`etag`] makes that revalidation a bodiless 304 whenever
///   the file is unchanged, so the steady-state cost is one conditional request, not a re-download.
///   (Previously scripts and styles carried `max-age=3600`, which is exactly what made a
///   Cloudflare purge necessary to pick up a new `vm.js`/`vm.css` within the hour.)
/// - Cosmetic media (images, fonts) get a short shared lifetime with `must-revalidate`: at worst
///   five minutes stale after a deploy, still purge-free, while absorbing repeat fetches.
pub fn cache_control(path: &Path) -> &'static str {
    if is_fingerprinted(path) {
        return "public, max-age=31536000, immutable";
    }
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match extension.as_str() {
        // Anything that changes in place under a stable URL: never serve stale, always
        // revalidate (cheap 304 via the strong ETag).
        "html" | "htm" | "css" | "js" | "mjs" | "json" | "txt" | "md" | "xml" | "wasm"
        | "cwasm" => "no-cache",
        // Logos, icons, fonts and other rarely-edited media: bounded staleness, no purge needed.
        _ => "public, max-age=300, must-revalidate",
    }
}

/// A strong ETag for one served representation: a 64-bit FNV-1a over the exact bytes being
/// sent (so the identity, brotli, and gzip representations of one file each get their own
/// validator), rendered as a quoted hex string. Collision risk is negligible for a site of
/// this size, and the value changes whenever the content does, which is all a validator
/// must guarantee.
pub fn etag(bytes: &[u8]) -> String {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    // Mix the length in so a truncation that happens to preserve the hash still changes it.
    hash ^= bytes.len() as u64;
    hash = hash.wrapping_mul(PRIME);
    format!("\"{hash:016x}\"")
}

/// Does an `If-None-Match` header value match this representation's ETag? Handles the
/// wildcard, comma-separated lists, and weak (`W/`) prefixes (weak comparison is fine for
/// a 304 decision).
pub fn if_none_match_matches(if_none_match: &str, etag: &str) -> bool {
    if if_none_match.trim() == "*" {
        return true;
    }
    if_none_match
        .split(',')
        .map(|candidate| candidate.trim().trim_start_matches("W/"))
        .any(|candidate| candidate == etag)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// The real site directory, relative to this crate's manifest.
    fn site_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("site")
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn parse(args: &[&str]) -> Result<Config, String> {
        parse_config(args.iter().map(|a| (*a).to_owned()), no_env)
    }

    #[test]
    fn parse_config_defaults_to_plain_http() {
        let config = parse(&[]).unwrap();
        assert_eq!(config.mode, Mode::PlainHttp);
        assert_eq!(config.http_bind, DEFAULT_HTTP_BIND);
        assert_eq!(config.https_bind, DEFAULT_HTTPS_BIND);
        assert_eq!(config.site_root, PathBuf::from(DEFAULT_SITE));
    }

    #[test]
    fn parse_config_env_then_flags_take_precedence() {
        let env = |name: &str| match name {
            "EO9_WWW_BIND" => Some("0.0.0.0:9000".to_owned()),
            "EO9_WWW_SITE" => Some("/srv/site".to_owned()),
            _ => None,
        };
        let env_only = parse_config(Vec::new(), env).unwrap();
        assert_eq!(env_only.http_bind, "0.0.0.0:9000");
        assert_eq!(env_only.site_root, PathBuf::from("/srv/site"));

        let flags = parse_config(
            ["--bind", "127.0.0.1:1234", "--site", "x"].map(String::from),
            env,
        )
        .unwrap();
        assert_eq!(flags.http_bind, "127.0.0.1:1234");
        assert_eq!(flags.site_root, PathBuf::from("x"));
    }

    #[test]
    fn parse_config_rejects_unknown_and_incomplete_flags() {
        assert!(parse(&["--verbose"]).is_err());
        assert!(parse(&["--bind"]).is_err());
        assert!(parse(&["--domain"]).is_err());
    }

    #[test]
    fn parse_config_acme_mode() {
        let config = parse(&[
            "--domain",
            "eo9.org",
            "--domain",
            "www.eo9.org",
            "--acme-email",
            "owner@example.com",
            "--acme-staging",
        ])
        .unwrap();
        assert_eq!(
            config.mode,
            Mode::Acme {
                domains: vec!["eo9.org".to_owned(), "www.eo9.org".to_owned()],
                email: "owner@example.com".to_owned(),
                cache_dir: PathBuf::from(DEFAULT_ACME_CACHE),
                staging: true,
            }
        );
        // In the TLS modes the HTTP listener defaults to port 80 (it only redirects).
        assert_eq!(config.http_bind, DEFAULT_REDIRECT_BIND);
        assert_eq!(config.https_bind, DEFAULT_HTTPS_BIND);
    }

    #[test]
    fn parse_config_manual_tls_mode() {
        let config = parse(&["--tls-cert", "c.pem", "--tls-key", "k.pem"]).unwrap();
        assert_eq!(
            config.mode,
            Mode::ManualTls {
                cert: PathBuf::from("c.pem"),
                key: PathBuf::from("k.pem"),
            }
        );
    }

    #[test]
    fn parse_config_rejects_inconsistent_tls_options() {
        // ACME email without a domain, ACME and manual mixed, half a manual pair,
        // --https-bind without any TLS mode: all rejected.
        assert!(parse(&["--acme-email", "a@b.c"]).is_err());
        assert!(
            parse(&[
                "--domain",
                "eo9.org",
                "--acme-email",
                "a@b.c",
                "--tls-cert",
                "c"
            ])
            .is_err()
        );
        assert!(parse(&["--tls-cert", "c.pem"]).is_err());
        assert!(parse(&["--domain", "eo9.org"]).is_err()); // missing --acme-email
        assert!(parse(&["--https-bind", "0.0.0.0:8443"]).is_err());
    }

    #[test]
    fn resolve_serves_index_for_root() {
        let file = resolve(&site_root(), "/").unwrap();
        assert!(file.ends_with("index.html"));
    }

    #[test]
    fn resolve_serves_plain_files_and_ignores_queries() {
        let file = resolve(&site_root(), "/logo.svg?cache-bust=1").unwrap();
        assert!(file.ends_with("logo.svg"));
        let file = resolve(&site_root(), "/style.css").unwrap();
        assert!(file.ends_with("style.css"));
    }

    #[test]
    fn resolve_rejects_missing_files() {
        assert_eq!(
            resolve(&site_root(), "/no-such-page"),
            Err(ResolveError::NotFound)
        );
    }

    #[test]
    fn resolve_rejects_traversal() {
        let root = site_root();
        // Plain, encoded, doubled, and backslash traversal attempts must never resolve,
        // even though ../Cargo.toml and ../src/lib.rs really exist.
        for url in [
            "/../Cargo.toml",
            "/../../Cargo.toml",
            "/%2e%2e/Cargo.toml",
            "/%2e%2e/%2e%2e/www/Cargo.toml",
            "/..%2fCargo.toml",
            "/static/../../src/lib.rs",
            "/..\\Cargo.toml",
        ] {
            assert_eq!(
                resolve(&root, url),
                Err(ResolveError::NotFound),
                "url: {url}"
            );
        }
    }

    #[test]
    fn resolve_rejects_malformed_escapes() {
        assert_eq!(resolve(&site_root(), "/%zz"), Err(ResolveError::BadRequest));
        assert_eq!(resolve(&site_root(), "/%2"), Err(ResolveError::BadRequest));
        assert_eq!(resolve(&site_root(), "/%00"), Err(ResolveError::BadRequest));
    }

    #[test]
    fn content_types_and_cache_headers() {
        assert_eq!(
            content_type(Path::new("index.html")),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            content_type(Path::new("style.css")),
            "text/css; charset=utf-8"
        );
        assert_eq!(content_type(Path::new("logo.svg")), "image/svg+xml");
        assert_eq!(content_type(Path::new("LOGO.SVG")), "image/svg+xml");
        assert_eq!(
            content_type(Path::new("unknown.bin")),
            "application/octet-stream"
        );
        assert_eq!(
            content_type(Path::new("no-extension")),
            "application/octet-stream"
        );

        // Mutable-in-place text assets always revalidate, so a deploy needs no CDN purge.
        assert_eq!(cache_control(Path::new("index.html")), "no-cache");
        assert_eq!(cache_control(Path::new("vm/vm.js")), "no-cache");
        assert_eq!(cache_control(Path::new("vm/vm.css")), "no-cache");
        assert_eq!(cache_control(Path::new("vm/assets.json")), "no-cache");
        assert_eq!(cache_control(Path::new("vm/web-eo9.wasm")), "no-cache");
        assert_eq!(cache_control(Path::new("vm/store/hello.cwasm")), "no-cache");
        // Cosmetic media may be briefly stale, never purge-requiring.
        assert_eq!(
            cache_control(Path::new("logo.svg")),
            "public, max-age=300, must-revalidate"
        );
        // Fingerprinted assets are immutable and cached for a year; the manifest that points
        // at them is never cached, so a new build is picked up immediately.
        assert_eq!(
            cache_control(Path::new("vm/web-eo9.3872dc3f251945ac.wasm")),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(
            cache_control(Path::new("vm/store/hello.5afedde1cf4b36c8.cwasm")),
            "public, max-age=31536000, immutable"
        );
    }

    #[test]
    fn fingerprinted_assets_are_detected_by_name() {
        // Real fingerprinted names (16 lowercase hex + a fingerprinted extension).
        assert!(is_fingerprinted(Path::new(
            "vm/web-eo9.3872dc3f251945ac.wasm"
        )));
        assert!(is_fingerprinted(Path::new(
            "vm/store/hello.5afedde1cf4b36c8.cwasm"
        )));
        // Not fingerprinted: canonical names, wrong length/case/charset, wrong extension,
        // and a bare hash with no base name.
        assert!(!is_fingerprinted(Path::new("vm/web-eo9.wasm")));
        assert!(!is_fingerprinted(Path::new("vm/store/hello.cwasm")));
        assert!(!is_fingerprinted(Path::new(
            "vm/web-eo9.3872DC3F251945AC.wasm"
        )));
        assert!(!is_fingerprinted(Path::new("vm/web-eo9.3872dc3f.wasm")));
        assert!(!is_fingerprinted(Path::new(
            "vm/web-eo9.notarealhash16x.wasm"
        )));
        assert!(!is_fingerprinted(Path::new("app.3872dc3f251945ac.js")));
        assert!(!is_fingerprinted(Path::new("3872dc3f251945ac.wasm")));
    }

    #[test]
    fn etags_are_strong_quoted_and_content_dependent() {
        let a = etag(b"hello");
        let b = etag(b"hello!");
        assert_ne!(a, b);
        assert_eq!(a, etag(b"hello"));
        assert!(a.starts_with('"') && a.ends_with('"') && a.len() == 18);

        assert!(if_none_match_matches(&a, &a));
        assert!(if_none_match_matches("*", &a));
        assert!(if_none_match_matches(&format!("W/{a}"), &a));
        assert!(if_none_match_matches(&format!("{b}, {a}"), &a));
        assert!(!if_none_match_matches(&b, &a));
        assert!(!if_none_match_matches("\"deadbeef\"", &a));
    }

    #[test]
    fn accept_encoding_negotiation() {
        // No header, empty header, or everything refused: serve the original.
        assert!(accepted_encodings(None).is_empty());
        assert!(accepted_encodings(Some("")).is_empty());
        assert!(accepted_encodings(Some("identity")).is_empty());
        assert!(accepted_encodings(Some("br;q=0, gzip;q=0")).is_empty());

        // Typical browser value: both, brotli preferred.
        assert_eq!(
            accepted_encodings(Some("gzip, deflate, br, zstd")),
            vec![Encoding::Brotli, Encoding::Gzip]
        );
        assert_eq!(accepted_encodings(Some("gzip")), vec![Encoding::Gzip]);
        assert_eq!(accepted_encodings(Some("x-gzip")), vec![Encoding::Gzip]);
        assert_eq!(accepted_encodings(Some("BR")), vec![Encoding::Brotli]);

        // Wildcard accepts both unless an explicit entry refuses one.
        assert_eq!(
            accepted_encodings(Some("*")),
            vec![Encoding::Brotli, Encoding::Gzip]
        );
        assert_eq!(accepted_encodings(Some("*, br;q=0")), vec![Encoding::Gzip]);

        // q-values: refused vs preferred (we only honor refusal, order is ours).
        assert_eq!(
            accepted_encodings(Some("br;q=0.5, gzip;q=1.0")),
            vec![Encoding::Brotli, Encoding::Gzip]
        );
        assert_eq!(
            accepted_encodings(Some("gzip;q=0, br")),
            vec![Encoding::Brotli]
        );
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("/a%20b").as_deref(), Some("/a b"));
        assert_eq!(percent_decode("/plain").as_deref(), Some("/plain"));
        assert_eq!(percent_decode("/%G1"), None);
        assert_eq!(percent_decode("/%0"), None);
        assert_eq!(percent_decode("/%00"), None);
    }
}
