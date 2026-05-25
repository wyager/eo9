//! Integration tests: bind the real server to an ephemeral port, serve the real `site/`
//! directory, and talk plain HTTP/1.1 to it over TCP (no client library needed).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::thread;

use eo9_www::{Config, SiteServer};

/// Start one shared server for the whole test binary and return its address.
fn server_addr() -> SocketAddr {
    static ADDR: OnceLock<SocketAddr> = OnceLock::new();
    *ADDR.get_or_init(|| {
        let config = Config {
            bind: "127.0.0.1:0".to_owned(),
            site_root: site_root(),
        };
        let server = SiteServer::bind(&config).expect("bind test server");
        let addr = server.local_addr().expect("test server has an IP address");
        thread::spawn(move || server.run());
        addr
    })
}

fn site_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("site")
}

/// A minimally parsed HTTP response.
struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Send one raw request line (plus Host/Connection headers) and parse the response.
fn request(method: &str, target: &str) -> HttpResponse {
    let addr = server_addr();
    let mut stream = TcpStream::connect(addr).expect("connect to test server");
    let message =
        format!("{method} {target} HTTP/1.1\r\nHost: eo9.org\r\nConnection: close\r\n\r\n");
    stream.write_all(message.as_bytes()).expect("send request");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read response");

    let split_at = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("response has a header/body separator");
    let head = String::from_utf8(raw[..split_at].to_vec()).expect("headers are UTF-8");
    let body = raw[split_at + 4..].to_vec();

    let mut lines = head.lines();
    let status_line = lines.next().expect("response has a status line");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .expect("status line has a code")
        .parse()
        .expect("status code is numeric");
    let headers = lines
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            Some((key.trim().to_owned(), value.trim().to_owned()))
        })
        .collect();
    HttpResponse {
        status,
        headers,
        body,
    }
}

#[test]
fn serves_index_at_root_with_html_content_type() {
    let response = request("GET", "/");
    assert_eq!(response.status, 200);
    assert_eq!(
        response.header("Content-Type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(
        response.header("Cache-Control"),
        Some("public, max-age=300")
    );
    let body = response.body_text();
    assert!(body.contains("<!doctype html>"));
    assert!(body.contains("capability-secure operating system"));
    assert!(body.contains("https://github.com/wyager/eo9"));
}

#[test]
fn serves_index_html_directly_too() {
    let response = request("GET", "/index.html");
    assert_eq!(response.status, 200);
    assert_eq!(
        response.header("Content-Type"),
        Some("text/html; charset=utf-8")
    );
}

#[test]
fn serves_css_and_svg_with_correct_content_types_and_caching() {
    let css = request("GET", "/style.css");
    assert_eq!(css.status, 200);
    assert_eq!(css.header("Content-Type"), Some("text/css; charset=utf-8"));
    assert_eq!(css.header("Cache-Control"), Some("public, max-age=86400"));

    let svg = request("GET", "/logo.svg");
    assert_eq!(svg.status, 200);
    assert_eq!(svg.header("Content-Type"), Some("image/svg+xml"));
    assert_eq!(svg.header("Cache-Control"), Some("public, max-age=86400"));
    assert!(svg.body_text().contains("<svg"));
}

#[test]
fn missing_files_get_a_clean_404() {
    let response = request("GET", "/no-such-page.html");
    assert_eq!(response.status, 404);
    assert_eq!(
        response.header("Content-Type"),
        Some("text/html; charset=utf-8")
    );
    assert!(response.body_text().contains("404"));
}

#[test]
fn path_traversal_is_rejected() {
    // Cargo.toml and src/lib.rs really exist one level above the site root; none of these
    // spellings may reach them.
    for target in [
        "/../Cargo.toml",
        "/../../www/Cargo.toml",
        "/%2e%2e/Cargo.toml",
        "/..%2fCargo.toml",
        "/static/../../src/lib.rs",
    ] {
        let response = request("GET", target);
        assert_eq!(response.status, 404, "target: {target}");
        assert!(
            !response.body_text().contains("[package]"),
            "target: {target}"
        );
    }
}

#[test]
fn malformed_percent_encoding_is_a_400() {
    let response = request("GET", "/%zz");
    assert_eq!(response.status, 400);
}

#[test]
fn head_requests_return_headers_without_a_body() {
    let response = request("HEAD", "/");
    assert_eq!(response.status, 200);
    assert_eq!(
        response.header("Content-Type"),
        Some("text/html; charset=utf-8")
    );
    assert!(response.body.is_empty());
}

#[test]
fn other_methods_are_rejected_with_405() {
    let response = request("POST", "/");
    assert_eq!(response.status, 405);
    assert_eq!(response.header("Allow"), Some("GET, HEAD"));
}

#[test]
fn query_strings_are_ignored_for_resolution() {
    let response = request("GET", "/style.css?v=1");
    assert_eq!(response.status, 200);
    assert_eq!(
        response.header("Content-Type"),
        Some("text/css; charset=utf-8")
    );
}
