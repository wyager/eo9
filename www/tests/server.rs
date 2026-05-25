//! Integration tests for the plain-HTTP site listener and the HTTP→HTTPS redirect listener:
//! real sockets on ephemeral ports, raw HTTP/1.1 over TCP (no client library needed).

mod common;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::OnceLock;

use common::{HttpResponse, parse_response, site_root, start_server};
use eo9_www::server::{Limits, serve_redirect, serve_site_http};

/// One shared plain-HTTP site server for the whole test binary.
fn site_server_addr() -> SocketAddr {
    static ADDR: OnceLock<SocketAddr> = OnceLock::new();
    *ADDR.get_or_init(|| {
        start_server(|listener| serve_site_http(listener, site_root(), Limits::default()))
    })
}

/// One shared redirect server (as used in the TLS modes), with `eo9.org` as canonical host.
fn redirect_server_addr() -> SocketAddr {
    static ADDR: OnceLock<SocketAddr> = OnceLock::new();
    *ADDR.get_or_init(|| {
        start_server(|listener| {
            serve_redirect(listener, Some("eo9.org".to_owned()), Limits::default())
        })
    })
}

/// Send one raw HTTP/1.1 request (plus Host/Connection headers) and parse the response.
fn request(addr: SocketAddr, method: &str, target: &str) -> HttpResponse {
    let mut stream = TcpStream::connect(addr).expect("connect to test server");
    let message =
        format!("{method} {target} HTTP/1.1\r\nHost: eo9.org\r\nConnection: close\r\n\r\n");
    stream.write_all(message.as_bytes()).expect("send request");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read response");
    parse_response(&raw)
}

#[test]
fn serves_index_at_root_with_html_content_type() {
    let response = request(site_server_addr(), "GET", "/");
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
    let response = request(site_server_addr(), "GET", "/index.html");
    assert_eq!(response.status, 200);
    assert_eq!(
        response.header("Content-Type"),
        Some("text/html; charset=utf-8")
    );
}

#[test]
fn serves_css_and_svg_with_correct_content_types_and_caching() {
    let css = request(site_server_addr(), "GET", "/style.css");
    assert_eq!(css.status, 200);
    assert_eq!(css.header("Content-Type"), Some("text/css; charset=utf-8"));
    assert_eq!(css.header("Cache-Control"), Some("public, max-age=86400"));

    let svg = request(site_server_addr(), "GET", "/logo.svg");
    assert_eq!(svg.status, 200);
    assert_eq!(svg.header("Content-Type"), Some("image/svg+xml"));
    assert_eq!(svg.header("Cache-Control"), Some("public, max-age=86400"));
    assert!(svg.body_text().contains("<svg"));
}

#[test]
fn missing_files_get_a_clean_404() {
    let response = request(site_server_addr(), "GET", "/no-such-page.html");
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
        let response = request(site_server_addr(), "GET", target);
        assert_eq!(response.status, 404, "target: {target}");
        assert!(
            !response.body_text().contains("[package]"),
            "target: {target}"
        );
    }
}

#[test]
fn malformed_percent_encoding_is_a_400() {
    let response = request(site_server_addr(), "GET", "/%zz");
    assert_eq!(response.status, 400);
}

#[test]
fn head_requests_return_headers_without_a_body() {
    let response = request(site_server_addr(), "HEAD", "/");
    assert_eq!(response.status, 200);
    assert_eq!(
        response.header("Content-Type"),
        Some("text/html; charset=utf-8")
    );
    assert!(response.body.is_empty());
}

#[test]
fn other_methods_are_rejected_with_405() {
    let response = request(site_server_addr(), "POST", "/");
    assert_eq!(response.status, 405);
    assert_eq!(response.header("Allow"), Some("GET, HEAD"));
}

#[test]
fn query_strings_are_ignored_for_resolution() {
    let response = request(site_server_addr(), "GET", "/style.css?v=1");
    assert_eq!(response.status, 200);
    assert_eq!(
        response.header("Content-Type"),
        Some("text/css; charset=utf-8")
    );
}

#[test]
fn http_redirects_to_https_preserving_host_path_and_query() {
    let response = request(redirect_server_addr(), "GET", "/style.css?v=1");
    assert_eq!(response.status, 301);
    assert_eq!(
        response.header("Location"),
        Some("https://eo9.org/style.css?v=1")
    );

    let root = request(redirect_server_addr(), "GET", "/");
    assert_eq!(root.status, 301);
    assert_eq!(root.header("Location"), Some("https://eo9.org/"));
}

#[test]
fn redirect_falls_back_to_canonical_host_without_a_host_header() {
    // HTTP/1.0 requests may omit the Host header; the redirect then uses the first
    // configured domain.
    let mut stream = TcpStream::connect(redirect_server_addr()).expect("connect to test server");
    stream
        .write_all(b"GET /spec HTTP/1.0\r\n\r\n")
        .expect("send request");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read response");
    let response = parse_response(&raw);
    assert_eq!(response.status, 301);
    assert_eq!(response.header("Location"), Some("https://eo9.org/spec"));
}
