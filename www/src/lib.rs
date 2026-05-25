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

/// The `Cache-Control` value for a file. HTML revalidates quickly so content updates show up;
/// everything else (stylesheet, logo, images) may be cached for a day.
pub fn cache_control(path: &Path) -> &'static str {
    if content_type(path).starts_with("text/html") {
        "public, max-age=300"
    } else {
        "public, max-age=86400"
    }
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

        assert_eq!(
            cache_control(Path::new("index.html")),
            "public, max-age=300"
        );
        assert_eq!(
            cache_control(Path::new("logo.svg")),
            "public, max-age=86400"
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
