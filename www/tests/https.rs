//! Integration tests for the HTTPS site listener in manual-TLS mode: a self-signed
//! certificate is generated with rcgen, loaded through the same code path as a real
//! deployment (`tls::manual_tls`), and exercised with a rustls client over TCP.
//!
//! ACME mode itself is deliberately not tested end to end here: it requires a public DNS
//! name and inbound port 443 from Let's Encrypt. Everything below the certificate order —
//! the TLS accept path, the request handling, and the redirect listener — is shared with
//! manual mode and covered by these tests and `tests/server.rs`; ACME flag parsing is
//! covered by the unit tests in `src/lib.rs`.

mod common;

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, OnceLock};

use common::{HttpResponse, parse_response, site_root, start_server};
use eo9_www::server::{Limits, serve_site_https};
use eo9_www::tls::manual_tls;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

struct TlsTestServer {
    addr: SocketAddr,
    client_config: Arc<ClientConfig>,
}

/// One shared HTTPS server (self-signed certificate for `localhost`) plus a client
/// configuration that trusts exactly that certificate.
fn tls_server() -> &'static TlsTestServer {
    static SERVER: OnceLock<TlsTestServer> = OnceLock::new();
    SERVER.get_or_init(|| {
        // Write a fresh self-signed certificate and key where manual mode expects them.
        let dir = std::env::temp_dir().join(format!("eo9-www-tls-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp certificate directory");
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])
            .expect("generate self-signed certificate");
        let cert_path = dir.join("cert.pem");
        let key_path = dir.join("key.pem");
        std::fs::write(&cert_path, certified.cert.pem()).expect("write certificate");
        std::fs::write(&key_path, certified.signing_key.serialize_pem()).expect("write key");

        // Start the HTTPS listener through the same code path as a real deployment.
        let settings = manual_tls(&cert_path, &key_path).expect("load certificate and key");
        let addr = start_server(move |listener| {
            serve_site_https(listener, settings, site_root(), Limits::default())
        });

        // A client that trusts exactly the certificate generated above.
        let mut roots = RootCertStore::empty();
        roots
            .add(certified.cert.der().clone())
            .expect("trust test certificate");
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let client_config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("client protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        TlsTestServer {
            addr,
            client_config: Arc::new(client_config),
        }
    })
}

/// Send one raw HTTP/1.1 request over TLS and parse the response.
fn https_request(method: &str, target: &str) -> HttpResponse {
    let server = tls_server();
    let tcp = TcpStream::connect(server.addr).expect("connect to test server");
    let name = ServerName::try_from("localhost".to_owned()).expect("valid server name");
    let connection =
        ClientConnection::new(server.client_config.clone(), name).expect("create tls client");
    let mut stream = StreamOwned::new(connection, tcp);

    let message =
        format!("{method} {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(message.as_bytes()).expect("send request");

    let mut raw = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&buffer[..n]),
            // A peer that closes without a TLS close_notify surfaces as UnexpectedEof;
            // the response bytes read so far are still complete.
            Err(error) if error.kind() == ErrorKind::UnexpectedEof && !raw.is_empty() => break,
            Err(error) => panic!("read response: {error}"),
        }
    }
    parse_response(&raw)
}

#[test]
fn https_serves_index_with_html_content_type() {
    let response = https_request("GET", "/");
    assert_eq!(response.status, 200);
    assert_eq!(
        response.header("Content-Type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(
        response.header("Cache-Control"),
        Some("public, max-age=300")
    );
    assert!(
        response
            .body_text()
            .contains("capability-secure operating system")
    );
}

#[test]
fn https_responses_carry_hsts_and_the_security_headers() {
    // HSTS is only ever sent over TLS; the rest of the security set matches plain HTTP.
    let response = https_request("GET", "/");
    assert_eq!(
        response.header("Strict-Transport-Security"),
        Some("max-age=63072000; includeSubDomains")
    );
    assert_eq!(response.header("X-Content-Type-Options"), Some("nosniff"));
    assert_eq!(response.header("Referrer-Policy"), Some("no-referrer"));
    assert!(
        response
            .header("Content-Security-Policy")
            .is_some_and(|csp| csp.contains("default-src 'self'"))
    );
}

#[test]
fn https_serves_assets_with_correct_content_types() {
    let svg = https_request("GET", "/logo.svg");
    assert_eq!(svg.status, 200);
    assert_eq!(svg.header("Content-Type"), Some("image/svg+xml"));
    assert_eq!(svg.header("Cache-Control"), Some("public, max-age=3600"));

    let css = https_request("GET", "/style.css");
    assert_eq!(css.status, 200);
    assert_eq!(css.header("Content-Type"), Some("text/css; charset=utf-8"));
}

#[test]
fn https_missing_files_get_a_clean_404() {
    let response = https_request("GET", "/no-such-page.html");
    assert_eq!(response.status, 404);
    assert!(response.body_text().contains("404"));
}

#[test]
fn https_path_traversal_is_rejected() {
    for target in [
        "/../Cargo.toml",
        "/%2e%2e/Cargo.toml",
        "/static/../../src/lib.rs",
    ] {
        let response = https_request("GET", target);
        assert_eq!(response.status, 404, "target: {target}");
        assert!(
            !response.body_text().contains("[package]"),
            "target: {target}"
        );
    }
}

#[test]
fn https_head_requests_return_headers_without_a_body() {
    let response = https_request("HEAD", "/");
    assert_eq!(response.status, 200);
    assert_eq!(
        response.header("Content-Type"),
        Some("text/html; charset=utf-8")
    );
    assert!(response.body.is_empty());
}
