//! Helpers shared by the integration tests: starting a listener on an ephemeral port in a
//! background runtime thread, and a minimal HTTP/1.1 response parser.

use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;

/// The real site directory, relative to this crate's manifest.
pub fn site_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("site")
}

/// Bind an ephemeral 127.0.0.1 port, hand the listener to `serve`, and drive it forever on
/// a dedicated runtime thread. Returns the bound address.
pub fn start_server<F, Fut>(serve: F) -> SocketAddr
where
    F: FnOnce(tokio::net::TcpListener) -> Fut + Send + 'static,
    Fut: Future<Output = std::io::Result<()>>,
{
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build test runtime");
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind ephemeral test port");
            let addr = listener.local_addr().expect("test listener address");
            sender.send(addr).expect("report test server address");
            serve(listener).await.expect("test server failed");
        });
    });
    receiver.recv().expect("receive test server address")
}

/// A minimally parsed HTTP response.
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Parse a raw HTTP/1.x response (status line, headers, body).
pub fn parse_response(raw: &[u8]) -> HttpResponse {
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
