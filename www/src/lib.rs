//! Static-file server for the eo9.org website.
//!
//! The library contains everything the binary does apart from reading `std::env`, so the
//! request-handling logic (path resolution, content types, cache headers) is unit-testable
//! and the full server is integration-testable on an ephemeral port (see `tests/server.rs`).

use std::fs;
use std::io::Error as IoError;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use tiny_http::{Header, Method, Request, Response, Server};

/// Default bind address when neither the flag nor the environment variable is set.
pub const DEFAULT_BIND: &str = "127.0.0.1:8080";
/// Default site directory, relative to the working directory.
pub const DEFAULT_SITE: &str = "site";

/// Server configuration: where to listen and which directory to serve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub bind: String,
    pub site_root: PathBuf,
}

/// Parse configuration from command-line arguments and environment values.
///
/// Precedence: flag > environment variable > default.
/// `args` is the argument list *without* the program name; `env_bind` / `env_site` are the
/// values of `EO9_WWW_BIND` / `EO9_WWW_SITE` (passed in so parsing stays a pure function).
pub fn parse_config<I>(
    args: I,
    env_bind: Option<String>,
    env_site: Option<String>,
) -> Result<Config, String>
where
    I: IntoIterator<Item = String>,
{
    let mut bind = None;
    let mut site = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bind" => {
                bind = Some(args.next().ok_or("--bind requires an ADDR:PORT value")?);
            }
            "--site" => {
                site = Some(args.next().ok_or("--site requires a DIRECTORY value")?);
            }
            other => return Err(format!("unrecognized argument `{other}` (see --help)")),
        }
    }
    let bind = bind.or(env_bind).unwrap_or_else(|| DEFAULT_BIND.to_owned());
    let site = site.or(env_site).unwrap_or_else(|| DEFAULT_SITE.to_owned());
    Ok(Config {
        bind,
        site_root: PathBuf::from(site),
    })
}

/// Usage text for `--help`.
pub const USAGE: &str = "eo9-www: static-file server for the eo9.org website

Usage: eo9-www [--bind ADDR:PORT] [--site DIRECTORY]

Options:
  --bind ADDR:PORT   address to listen on      (env EO9_WWW_BIND, default 127.0.0.1:8080)
  --site DIRECTORY   directory to serve        (env EO9_WWW_SITE, default ./site)
  --help             print this message

The server speaks plain HTTP; run it behind a reverse proxy for TLS (see www/README.md).";

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

/// A bound server plus the directory it serves.
pub struct SiteServer {
    server: Server,
    site_root: PathBuf,
}

impl SiteServer {
    /// Bind to `config.bind` and verify the site directory exists.
    pub fn bind(config: &Config) -> Result<SiteServer, String> {
        if !config.site_root.is_dir() {
            return Err(format!(
                "site directory `{}` does not exist or is not a directory",
                config.site_root.display()
            ));
        }
        let server = Server::http(config.bind.as_str())
            .map_err(|e| format!("failed to bind {}: {e}", config.bind))?;
        Ok(SiteServer {
            server,
            site_root: config.site_root.clone(),
        })
    }

    /// The address the server is actually listening on (useful when bound to port 0).
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.server.server_addr().to_ip()
    }

    /// Serve requests forever. Malformed requests are rejected by the HTTP layer and
    /// I/O errors on individual connections are logged and skipped; neither stops the loop.
    pub fn run(&self) {
        loop {
            match self.server.recv() {
                Ok(request) => {
                    if let Err(error) = handle(request, &self.site_root) {
                        eprintln!("eo9-www: error writing response: {error}");
                    }
                }
                Err(error) => eprintln!("eo9-www: error receiving request: {error}"),
            }
        }
    }
}

/// Handle one request. GET and HEAD are served (tiny_http omits the body for HEAD itself);
/// every other method gets 405. Resolution failures get 400 or 404 with a small HTML body.
fn handle(request: Request, site_root: &Path) -> Result<(), IoError> {
    let method = request.method().clone();
    if method != Method::Get && method != Method::Head {
        let response =
            error_response(405, "method not allowed").with_header(header("Allow", "GET, HEAD"));
        return request.respond(response);
    }

    match resolve(site_root, request.url()) {
        Ok(file) => match fs::read(&file) {
            Ok(body) => {
                let response = Response::from_data(body)
                    .with_header(header("Content-Type", content_type(&file)))
                    .with_header(header("Cache-Control", cache_control(&file)));
                request.respond(response)
            }
            // The file vanished between resolution and reading; treat it as not found.
            Err(_) => request.respond(error_response(404, "not found")),
        },
        Err(ResolveError::BadRequest) => request.respond(error_response(400, "bad request")),
        Err(ResolveError::NotFound) => request.respond(error_response(404, "not found")),
    }
}

fn error_response(status: u16, message: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let body = format!(
        "<!doctype html>\n<html><head><title>{status} {message}</title></head>\
         <body><h1>{status} {message}</h1><p><a href=\"/\">eo9.org</a></p></body></html>\n"
    );
    Response::from_data(body.into_bytes())
        .with_status_code(status)
        .with_header(header("Content-Type", "text/html; charset=utf-8"))
        .with_header(header("Cache-Control", "no-store"))
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes())
        .expect("static header names and values are always valid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// The real site directory, relative to this crate's manifest.
    fn site_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("site")
    }

    #[test]
    fn parse_config_defaults() {
        let config = parse_config(Vec::new(), None, None).unwrap();
        assert_eq!(config.bind, DEFAULT_BIND);
        assert_eq!(config.site_root, PathBuf::from(DEFAULT_SITE));
    }

    #[test]
    fn parse_config_env_then_flags_take_precedence() {
        let env_only = parse_config(
            Vec::new(),
            Some("0.0.0.0:9000".to_owned()),
            Some("/srv/site".to_owned()),
        )
        .unwrap();
        assert_eq!(env_only.bind, "0.0.0.0:9000");
        assert_eq!(env_only.site_root, PathBuf::from("/srv/site"));

        let flags = parse_config(
            vec![
                "--bind".to_owned(),
                "127.0.0.1:1234".to_owned(),
                "--site".to_owned(),
                "x".to_owned(),
            ],
            Some("0.0.0.0:9000".to_owned()),
            Some("/srv/site".to_owned()),
        )
        .unwrap();
        assert_eq!(flags.bind, "127.0.0.1:1234");
        assert_eq!(flags.site_root, PathBuf::from("x"));
    }

    #[test]
    fn parse_config_rejects_unknown_and_incomplete_flags() {
        assert!(parse_config(vec!["--verbose".to_owned()], None, None).is_err());
        assert!(parse_config(vec!["--bind".to_owned()], None, None).is_err());
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
