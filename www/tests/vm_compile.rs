//! Integration tests for the `POST /vm/compile` endpoint (plan/18 D20): a real listener on an
//! ephemeral port, raw HTTP/1.1 with a body. The endpoint fuses a composition expressed over
//! store-program names + algebra ops and compiles it to a pulley32 image. These tests need the
//! guest components built (`cargo xtask build-guest`); they skip cleanly if they are absent.

#[allow(dead_code)] // common/ is shared across test binaries; this one uses a subset.
mod common;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::sync::OnceLock;

use common::{HttpResponse, parse_response, start_server};
use eo9_www::server::{Limits, serve_site_http};

/// One shared site server backed by a temp `site/vm/raw` populated with the two demo
/// components. `None` if the guest components are not built — tests then skip.
fn vm_server() -> Option<SocketAddr> {
    static SERVER: OnceLock<Option<SocketAddr>> = OnceLock::new();
    *SERVER.get_or_init(|| {
        let root = std::env::temp_dir().join("eo9-vm-compile-it");
        let raw = root.join("vm").join("raw");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&raw).expect("create temp site/vm/raw");
        let components = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("guest")
            .join("target")
            .join("components");
        let pairs = [
            ("entropy.seeded", "eo9-stub-entropy-seeded.wasm"),
            ("rng", "eo9-coreutil-rng.wasm"),
        ];
        for (name, file) in pairs {
            let src = components.join(file);
            if !src.exists() {
                eprintln!("skipping /vm/compile tests: {} not built", src.display());
                return None;
            }
            std::fs::copy(&src, raw.join(format!("{name}.wasm"))).expect("copy demo component");
        }
        Some(start_server(move |listener| {
            serve_site_http(listener, root, Limits::default())
        }))
    })
}

/// POST a body to `target` over a one-shot HTTP/1.1 connection and parse the response.
fn post(addr: SocketAddr, target: &str, body: &str) -> HttpResponse {
    let mut stream = TcpStream::connect(addr).expect("connect to test server");
    let message = format!(
        "POST {target} HTTP/1.1\r\nHost: eo9.org\r\nConnection: close\r\n\
         Content-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(message.as_bytes()).expect("send request");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read response");
    parse_response(&raw)
}

#[test]
fn compiles_a_store_name_composition_to_an_image() {
    let Some(addr) = vm_server() else { return };
    let response = post(addr, "/vm/compile", "entropy.seeded $ rng --count 3");
    assert_eq!(response.status, 200, "body: {}", response.body_text());
    assert_eq!(response.header("content-type"), Some("application/octet-stream"));
    assert!(
        response.body.len() > 4096,
        "implausibly small image: {} bytes",
        response.body.len()
    );
    // The compile response carries the standard security headers like every other response.
    assert!(response.header("content-security-policy").is_some());
    assert_eq!(response.header("x-content-type-options"), Some("nosniff"));
}

#[test]
fn a_plain_store_name_compiles_too() {
    let Some(addr) = vm_server() else { return };
    let response = post(addr, "/vm/compile", "rng");
    assert_eq!(response.status, 200, "body: {}", response.body_text());
    assert!(response.body.len() > 4096);
}

#[test]
fn an_unknown_program_is_rejected_not_compiled() {
    let Some(addr) = vm_server() else { return };
    let response = post(addr, "/vm/compile", "secret $ rng");
    assert_eq!(response.status, 400, "body: {}", response.body_text());
    assert!(response.body_text().contains("secret"));
}

#[test]
fn unsupported_operators_are_rejected() {
    let Some(addr) = vm_server() else { return };
    let response = post(addr, "/vm/compile", "entropy.seeded & rng");
    assert_eq!(response.status, 400, "body: {}", response.body_text());
}

#[test]
fn an_empty_composition_is_a_400() {
    let Some(addr) = vm_server() else { return };
    let response = post(addr, "/vm/compile", "   ");
    assert_eq!(response.status, 400);
}

#[test]
fn an_oversized_body_is_rejected() {
    let Some(addr) = vm_server() else { return };
    let huge = "rng ".repeat(2000); // well past MAX_COMPILE_BODY (2048 bytes)
    let response = post(addr, "/vm/compile", &huge);
    assert_eq!(response.status, 413);
}
