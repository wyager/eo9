//! Tests for the connection-hygiene limits: stalled handshakes/headers are dropped after
//! the configured timeouts, and the per-listener connection cap is respected. The servers
//! here use small limit values so the tests stay fast.
//!
//! Not covered (kept proportionate): the overall connection-lifetime bound and the exact
//! permit count at scale — both are the same `timeout`/`Semaphore` mechanism exercised
//! below, just with larger numbers.

mod common;

use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use common::{parse_response, site_root, start_server};
use eo9_www::server::{Limits, serve_site_http, serve_site_https};
use eo9_www::tls::manual_tls;

/// Read until EOF (connection closed by the server) or until the socket read timeout fires.
/// Returns the bytes read and whether the server closed the connection.
fn read_until_close(stream: &mut TcpStream, patience: Duration) -> (Vec<u8>, bool) {
    stream
        .set_read_timeout(Some(patience))
        .expect("set read timeout");
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => return (bytes, true),
            Ok(n) => bytes.extend_from_slice(&buffer[..n]),
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return (bytes, false);
            }
            // An abrupt close (reset) still means the server dropped the connection.
            Err(_) => return (bytes, true),
        }
    }
}

#[test]
fn stalled_tls_handshake_is_dropped_after_timeout() {
    // A fresh HTTPS server with a short handshake deadline and a self-signed certificate.
    let dir = std::env::temp_dir().join(format!("eo9-www-limits-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp certificate directory");
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])
        .expect("generate self-signed certificate");
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    std::fs::write(&cert_path, certified.cert.pem()).expect("write certificate");
    std::fs::write(&key_path, certified.signing_key.serialize_pem()).expect("write key");
    let settings = manual_tls(&cert_path, &key_path).expect("load certificate and key");

    let limits = Limits {
        tls_handshake_timeout: Duration::from_millis(300),
        ..Limits::default()
    };
    let addr =
        start_server(move |listener| serve_site_https(listener, settings, site_root(), limits));

    // Connect with plain TCP and never start the TLS handshake: the server must hang up.
    let mut stream = TcpStream::connect(addr).expect("connect to test server");
    let (bytes, closed) = read_until_close(&mut stream, Duration::from_secs(5));
    assert!(closed, "server kept a stalled TLS handshake open");
    assert!(bytes.is_empty(), "server sent unexpected bytes: {bytes:?}");
}

#[test]
fn stalled_request_headers_are_dropped_after_timeout() {
    let limits = Limits {
        header_read_timeout: Duration::from_millis(300),
        ..Limits::default()
    };
    let addr = start_server(move |listener| serve_site_http(listener, site_root(), limits));

    // Send half a request line and then stall: the server must hang up.
    let mut stream = TcpStream::connect(addr).expect("connect to test server");
    stream.write_all(b"GET / HT").expect("send partial request");
    let (_bytes, closed) = read_until_close(&mut stream, Duration::from_secs(5));
    assert!(closed, "server kept a stalled request open");
}

#[test]
fn connection_cap_queues_further_connections_until_a_slot_frees() {
    let limits = Limits {
        max_connections: 1,
        ..Limits::default()
    };
    let addr = start_server(move |listener| serve_site_http(listener, site_root(), limits));

    // First connection: complete a request but keep the connection alive, holding the only
    // permit.
    let mut first = TcpStream::connect(addr).expect("connect first client");
    first
        .write_all(b"GET /style.css HTTP/1.1\r\nHost: eo9.org\r\n\r\n")
        .expect("send first request");
    first
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    let mut buffer = [0u8; 4096];
    let n = first.read(&mut buffer).expect("read first response");
    assert!(
        String::from_utf8_lossy(&buffer[..n]).starts_with("HTTP/1.1 200"),
        "first connection should be served"
    );

    // Second connection: sends a complete request, but must not be served while the first
    // connection holds the only permit.
    let mut second = TcpStream::connect(addr).expect("connect second client");
    second
        .write_all(b"GET / HTTP/1.1\r\nHost: eo9.org\r\nConnection: close\r\n\r\n")
        .expect("send second request");
    let (bytes, closed) = read_until_close(&mut second, Duration::from_millis(700));
    assert!(
        bytes.is_empty() && !closed,
        "second connection was served over the limit"
    );

    // Closing the first connection frees the permit and the second gets served.
    drop(first);
    let (bytes, _closed) = read_until_close(&mut second, Duration::from_secs(5));
    let response = parse_response(&bytes);
    assert_eq!(response.status, 200);
    assert_eq!(
        response.header("Content-Type"),
        Some("text/html; charset=utf-8")
    );
    assert!(
        response
            .body_text()
            .contains("capability-secure operating system")
    );
}
