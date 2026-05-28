//! The listeners and request handlers: plain-HTTP site serving, HTTPS site serving
//! (manual or ACME certificates), and the HTTP→HTTPS redirect listener.
//!
//! Every listener applies the same connection hygiene (see [`Limits`]): a cap on concurrent
//! connections, a TLS-handshake deadline, a header-read deadline, and an overall per-connection
//! lifetime, so stalled or hostile clients cannot pin tasks or sockets indefinitely.

use std::convert::Infallible;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use futures_util::StreamExt;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, Uri, header};
use hyper_util::rt::{TokioIo, TokioTimer};
use rustls_acme::is_tls_alpn_challenge;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::time::timeout;
use tokio_rustls::LazyConfigAcceptor;
use tokio_rustls::server::TlsStream;

use hyper::HeaderMap;

use crate::tls::TlsSettings;
use crate::{
    Config, Encoding, Mode, ResolveError, accepted_encodings, cache_control, content_type, resolve,
    tls,
};

type Body = Full<Bytes>;

/// Whether a response is being served over TLS. `Strict-Transport-Security` is only ever
/// sent on TLS responses (per the HSTS spec, plain-HTTP responses must not set it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Plain,
    Tls,
}

/// The Content-Security-Policy for every page: first-party only, with the one extra
/// permission the demo pages need — `'wasm-unsafe-eval'` so the browser may compile
/// WebAssembly fetched from this origin (`/try`'s transpiled components, `/vm`'s blob).
/// There is no inline script or style anywhere on the site.
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; \
     style-src 'self'; img-src 'self'; font-src 'self'; connect-src 'self'; object-src 'none'; \
     base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// Two years, the conventional HSTS lifetime once a site is committed to HTTPS.
const STRICT_TRANSPORT_SECURITY: &str = "max-age=63072000; includeSubDomains";

/// Add the security headers every response carries (and HSTS on TLS responses). Applied to
/// site responses, error responses, and redirects alike.
fn with_standard_headers(mut response: Response<Body>, transport: Transport) -> Response<Body> {
    let headers = response.headers_mut();
    headers.insert(
        header::HeaderName::from_static("x-content-type-options"),
        header::HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::HeaderName::from_static("referrer-policy"),
        header::HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::HeaderName::from_static("content-security-policy"),
        header::HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        header::HeaderName::from_static("cross-origin-opener-policy"),
        header::HeaderValue::from_static("same-origin"),
    );
    if transport == Transport::Tls {
        headers.insert(
            header::HeaderName::from_static("strict-transport-security"),
            header::HeaderValue::from_static(STRICT_TRANSPORT_SECURITY),
        );
    }
    response
}

/// Connection limits applied to every listener. Defaults are generous for a small static
/// site; tests use smaller values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Maximum concurrent connections per listener; further connections wait in the kernel
    /// accept backlog until a slot frees up.
    pub max_connections: usize,
    /// Time allowed for a TLS handshake to complete before the connection is dropped.
    pub tls_handshake_timeout: Duration,
    /// Time allowed for a client to send its request headers (also bounds how long an idle
    /// keep-alive connection waits for its next request).
    pub header_read_timeout: Duration,
    /// Overall lifetime of one connection; when it expires the connection is dropped even
    /// mid-request. Keep-alive is fine within this bound.
    pub max_connection_lifetime: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_connections: 256,
            tls_handshake_timeout: Duration::from_secs(10),
            header_read_timeout: Duration::from_secs(10),
            max_connection_lifetime: Duration::from_secs(60),
        }
    }
}

/// Run the configured server until failure. This is the whole program after argument parsing.
pub async fn run(config: Config) -> Result<(), String> {
    if !config.site_root.is_dir() {
        return Err(format!(
            "site directory `{}` does not exist or is not a directory",
            config.site_root.display()
        ));
    }
    let Config {
        site_root,
        http_bind,
        https_bind,
        mode,
    } = config;
    let limits = Limits::default();

    match mode {
        Mode::PlainHttp => {
            let listener = bind(&http_bind).await?;
            println!(
                "eo9-www: serving {} on http://{}/",
                site_root.display(),
                local_addr(&listener, &http_bind)
            );
            serve_site_http(listener, site_root, limits)
                .await
                .map_err(|e| format!("http server failed: {e}"))
        }
        Mode::ManualTls { cert, key } => {
            let settings = tls::manual_tls(&cert, &key)?;
            run_tls(&site_root, &http_bind, &https_bind, settings, None, limits).await
        }
        Mode::Acme {
            domains,
            email,
            cache_dir,
            staging,
        } => {
            let (settings, mut state) = tls::acme_tls(&domains, &email, &cache_dir, staging)?;
            println!(
                "eo9-www: acme: certificates for {} cached in {} ({})",
                domains.join(", "),
                cache_dir.display(),
                if staging {
                    "Let's Encrypt staging"
                } else {
                    "Let's Encrypt production"
                }
            );
            // Drive certificate acquisition and renewal in the background for the lifetime
            // of the server; the resolver inside `settings` picks up each new certificate.
            tokio::spawn(async move {
                loop {
                    match state.next().await {
                        Some(Ok(event)) => println!("eo9-www: acme: {event:?}"),
                        Some(Err(error)) => eprintln!("eo9-www: acme error: {error}"),
                        None => break,
                    }
                }
            });
            run_tls(
                &site_root,
                &http_bind,
                &https_bind,
                settings,
                domains.into_iter().next(),
                limits,
            )
            .await
        }
    }
}

/// Run the HTTPS site listener plus the HTTP redirect listener.
async fn run_tls(
    site_root: &Path,
    http_bind: &str,
    https_bind: &str,
    settings: TlsSettings,
    canonical_host: Option<String>,
    limits: Limits,
) -> Result<(), String> {
    let https_listener = bind(https_bind).await?;
    let http_listener = bind(http_bind).await?;
    println!(
        "eo9-www: serving {} on https://{}/ (http://{}/ redirects)",
        site_root.display(),
        local_addr(&https_listener, https_bind),
        local_addr(&http_listener, http_bind)
    );
    tokio::try_join!(
        serve_site_https(https_listener, settings, site_root.to_path_buf(), limits),
        serve_redirect(http_listener, canonical_host, limits),
    )
    .map(|_| ())
    .map_err(|e| format!("server failed: {e}"))
}

async fn bind(addr: &str) -> Result<TcpListener, String> {
    TcpListener::bind(addr)
        .await
        .map_err(|e| format!("failed to bind {addr}: {e}"))
}

/// The address to print at startup: the real local address (useful with port 0), falling
/// back to the configured string.
fn local_addr(listener: &TcpListener, configured: &str) -> String {
    listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| configured.to_owned())
}

/// Serve the site over plain HTTP (development mode).
pub async fn serve_site_http(
    listener: TcpListener,
    site_root: PathBuf,
    limits: Limits,
) -> io::Result<()> {
    let site_root = Arc::new(site_root);
    let permits = Arc::new(Semaphore::new(limits.max_connections));
    loop {
        let permit = acquire(&permits).await;
        let stream = accept(&listener).await;
        let site_root = site_root.clone();
        tokio::spawn(async move {
            let _permit = permit; // released when the connection is done
            serve_site_connection(stream, site_root, limits, Transport::Plain).await;
        });
    }
}

/// Serve the site over HTTPS. In ACME mode, TLS-ALPN-01 challenge handshakes from the
/// certificate authority are answered here too (that is the whole challenge: completing a
/// handshake with a special certificate — no content is served on such connections).
pub async fn serve_site_https(
    listener: TcpListener,
    settings: TlsSettings,
    site_root: PathBuf,
    limits: Limits,
) -> io::Result<()> {
    let settings = Arc::new(settings);
    let site_root = Arc::new(site_root);
    let permits = Arc::new(Semaphore::new(limits.max_connections));
    loop {
        let permit = acquire(&permits).await;
        let stream = accept(&listener).await;
        let settings = settings.clone();
        let site_root = site_root.clone();
        tokio::spawn(async move {
            let _permit = permit; // released when the connection is done
            if let Err(error) = serve_tls_connection(stream, &settings, site_root, limits).await {
                eprintln!("eo9-www: tls connection error: {error}");
            }
        });
    }
}

/// Redirect every plain-HTTP request to HTTPS (used in the TLS modes). The target host is
/// taken from the request's `Host` header, falling back to `canonical_host`.
pub async fn serve_redirect(
    listener: TcpListener,
    canonical_host: Option<String>,
    limits: Limits,
) -> io::Result<()> {
    let canonical_host = Arc::new(canonical_host);
    let permits = Arc::new(Semaphore::new(limits.max_connections));
    loop {
        let permit = acquire(&permits).await;
        let stream = accept(&listener).await;
        let canonical_host = canonical_host.clone();
        tokio::spawn(async move {
            let _permit = permit; // released when the connection is done
            let service = service_fn(move |request: Request<Incoming>| {
                let canonical_host = canonical_host.clone();
                async move {
                    let host = request
                        .headers()
                        .get(header::HOST)
                        .and_then(|value| value.to_str().ok());
                    Ok::<_, Infallible>(redirect_response(
                        host,
                        request.uri(),
                        canonical_host.as_deref(),
                    ))
                }
            });
            drive_connection(stream, service, limits).await;
        });
    }
}

/// Wait for a free connection slot. The semaphore is never closed, so this cannot fail.
async fn acquire(permits: &Arc<Semaphore>) -> tokio::sync::OwnedSemaphorePermit {
    permits
        .clone()
        .acquire_owned()
        .await
        .expect("connection semaphore is never closed")
}

/// Accept the next connection; transient accept failures are logged and retried after a
/// short pause so a burst of errors (e.g. file-descriptor exhaustion) cannot kill the loop.
async fn accept(listener: &TcpListener) -> TcpStream {
    loop {
        match listener.accept().await {
            Ok((stream, _peer)) => return stream,
            Err(error) => {
                eprintln!("eo9-www: accept failed: {error}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// Complete the TLS handshake (within the handshake deadline) and serve the site over the
/// resulting stream.
async fn serve_tls_connection(
    stream: TcpStream,
    settings: &TlsSettings,
    site_root: Arc<PathBuf>,
    limits: Limits,
) -> io::Result<()> {
    let accepted = timeout(limits.tls_handshake_timeout, tls_accept(stream, settings))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "tls handshake timed out"))??;
    match accepted {
        // An ACME validation connection: already answered and closed in `tls_accept`.
        None => Ok(()),
        Some(tls) => {
            serve_site_connection(tls, site_root, limits, Transport::Tls).await;
            Ok(())
        }
    }
}

/// Run the TLS handshake. Returns `None` for ACME TLS-ALPN-01 validation connections (the
/// handshake with the challenge certificate *is* the whole exchange — nothing is served).
async fn tls_accept(
    stream: TcpStream,
    settings: &TlsSettings,
) -> io::Result<Option<TlsStream<TcpStream>>> {
    let handshake = LazyConfigAcceptor::new(rustls::server::Acceptor::default(), stream).await?;
    match &settings.challenge_config {
        Some(challenge_config) if is_tls_alpn_challenge(&handshake.client_hello()) => {
            let mut tls = handshake.into_stream(challenge_config.clone()).await?;
            tls.shutdown().await?;
            Ok(None)
        }
        _ => Ok(Some(
            handshake
                .into_stream(settings.server_config.clone())
                .await?,
        )),
    }
}

/// Serve HTTP/1.1 on one (plain or TLS) connection, answering every request from the site.
async fn serve_site_connection<IO>(
    stream: IO,
    site_root: Arc<PathBuf>,
    limits: Limits,
    transport: Transport,
) where
    IO: AsyncRead + AsyncWrite + Unpin,
{
    let service = service_fn(move |request: Request<Incoming>| {
        let site_root = site_root.clone();
        async move {
            // The one dynamic endpoint: POST /vm/compile fuses a store-name composition with
            // the real algebra and compiles it to a pulley32 image (plan/18 D20). Everything
            // else is the static-file path.
            let response =
                if request.method() == Method::POST && request.uri().path() == "/vm/compile" {
                    compile_response(request.into_body(), &site_root, transport).await
                } else {
                    site_response(
                        request.method(),
                        request.uri(),
                        request.headers(),
                        &site_root,
                        transport,
                    )
                    .await
                };
            Ok::<_, Infallible>(response)
        }
    });
    drive_connection(stream, service, limits).await;
}

/// Drive one HTTP/1.1 connection with the configured deadlines: hyper's header-read timeout
/// covers each request's headers (and idle keep-alive waits), and the whole connection is
/// dropped once its lifetime budget is spent.
async fn drive_connection<IO, S>(stream: IO, service: S, limits: Limits)
where
    IO: AsyncRead + AsyncWrite + Unpin,
    S: hyper::service::Service<Request<Incoming>, Response = Response<Body>, Error = Infallible>,
{
    let connection = http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(limits.header_read_timeout)
        .serve_connection(TokioIo::new(stream), service);
    match timeout(limits.max_connection_lifetime, connection).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => eprintln!("eo9-www: http connection error: {error}"),
        // Lifetime budget spent: dropping the connection future closes the socket.
        Err(_elapsed) => {}
    }
}

/// Build the response for one request against the site directory. GET and HEAD are served
/// (hyper omits the body for HEAD itself); every other method gets 405. Resolution failures
/// get 400 or 404 with a small HTML body. When the client accepts it and a fresh
/// pre-compressed sibling exists (see `www/precompress`), that representation is served with
/// the matching `Content-Encoding`; the `Content-Type` and cache policy are always those of
/// the original file. Every response carries the standard security headers.
pub async fn site_response(
    method: &Method,
    uri: &Uri,
    request_headers: &HeaderMap,
    site_root: &Path,
    transport: Transport,
) -> Response<Body> {
    with_standard_headers(
        site_file_response(method, uri, request_headers, site_root).await,
        transport,
    )
}

/// The content half of [`site_response`]: everything except the standard headers.
async fn site_file_response(
    method: &Method,
    uri: &Uri,
    request_headers: &HeaderMap,
    site_root: &Path,
) -> Response<Body> {
    if method != Method::GET && method != Method::HEAD {
        let mut response = error_response(StatusCode::METHOD_NOT_ALLOWED);
        response
            .headers_mut()
            .insert(header::ALLOW, header::HeaderValue::from_static("GET, HEAD"));
        return response;
    }
    let file = match resolve(site_root, uri.path()) {
        Ok(file) => file,
        Err(ResolveError::BadRequest) => return error_response(StatusCode::BAD_REQUEST),
        Err(ResolveError::NotFound) => return error_response(StatusCode::NOT_FOUND),
    };

    let accept_encoding = request_headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|value| value.to_str().ok());
    let variant = select_precompressed(&file, accept_encoding).await;
    let read_path = variant
        .as_ref()
        .map_or_else(|| file.clone(), |(path, _)| path.clone());

    let contents = match tokio::fs::read(&read_path).await {
        Ok(contents) => contents,
        // The file vanished between resolution and reading; treat it as not found.
        Err(_) => return error_response(StatusCode::NOT_FOUND),
    };

    // Content-fingerprinted assets (the big wasm blob and the `.cwasm` store images) are
    // immutable: the URL is the version, so they are served `immutable` with no validator and
    // — crucially — their bodies are never hashed on the request path (a million requests for
    // the ~1 MiB blob cost zero hashing). Everything else carries a strong ETag computed over
    // the exact representation being served, so a post-lifetime revalidation is a bodyless 304.
    let fingerprinted = crate::is_fingerprinted(&file);
    let etag = (!fingerprinted).then(|| crate::etag(&contents));
    let revalidated = etag.as_deref().is_some_and(|etag| {
        request_headers
            .get(header::IF_NONE_MATCH)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| crate::if_none_match_matches(value, etag))
    });

    let mut builder = Response::builder()
        .status(if revalidated {
            StatusCode::NOT_MODIFIED
        } else {
            StatusCode::OK
        })
        .header(header::CONTENT_TYPE, content_type(&file))
        .header(header::CACHE_CONTROL, cache_control(&file))
        // The served representation depends on Accept-Encoding whenever a sibling exists,
        // and may start to at any deploy: always tell caches to key on it.
        .header(header::VARY, "Accept-Encoding");
    if let Some(etag) = etag {
        builder = builder.header(header::ETAG, etag);
    }
    if let Some((_, encoding)) = variant {
        builder = builder.header(header::CONTENT_ENCODING, encoding.token());
    }
    let body = if revalidated { Vec::new() } else { contents };
    builder
        .body(Full::new(Bytes::from(body)))
        .expect("statically valid response")
}

/// Pick the pre-compressed sibling to serve, if any: the client must accept the encoding,
/// the sibling must exist, and it must be at least as new as the original — an edited
/// original is never shadowed by a stale variant.
async fn select_precompressed(
    file: &Path,
    accept_encoding: Option<&str>,
) -> Option<(PathBuf, Encoding)> {
    let accepted = accepted_encodings(accept_encoding);
    if accepted.is_empty() {
        return None;
    }
    let original = tokio::fs::metadata(file).await.ok()?;
    for encoding in accepted {
        let mut name = file.as_os_str().to_owned();
        name.push(".");
        name.push(encoding.file_extension());
        let candidate = PathBuf::from(name);
        let Ok(meta) = tokio::fs::metadata(&candidate).await else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let fresh = match (meta.modified(), original.modified()) {
            (Ok(variant_mtime), Ok(original_mtime)) => variant_mtime >= original_mtime,
            // If either filesystem refuses to report mtimes, trust the build step.
            _ => true,
        };
        if fresh {
            return Some((candidate, encoding));
        }
    }
    None
}

/// Build the 301 redirect to HTTPS for one plain-HTTP request. `host_header` is the raw
/// `Host` header value; `canonical_host` is the fallback when it is missing. The redirect
/// listener is plain HTTP, so the standard headers are added without HSTS.
pub fn redirect_response(
    host_header: Option<&str>,
    uri: &Uri,
    canonical_host: Option<&str>,
) -> Response<Body> {
    let host = host_header
        .map(strip_port)
        .filter(|host| is_valid_host(host))
        .or(canonical_host);
    let Some(host) = host else {
        return with_standard_headers(error_response(StatusCode::BAD_REQUEST), Transport::Plain);
    };
    let path_and_query = uri.path_and_query().map_or("/", |pq| pq.as_str());
    let location = format!("https://{host}{path_and_query}");
    let response = Response::builder()
        .status(StatusCode::MOVED_PERMANENTLY)
        .header(header::LOCATION, location.as_str())
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Full::new(Bytes::from(format!(
            "<!doctype html>\n<html><body><a href=\"{location}\">{location}</a></body></html>\n"
        ))))
        .expect("statically valid response");
    with_standard_headers(response, Transport::Plain)
}

/// Drop a trailing `:port` from a Host header value, leaving IPv6 literals intact.
fn strip_port(host: &str) -> &str {
    if let Some(end) = host.find(']') {
        return &host[..=end]; // "[::1]:8080" -> "[::1]"
    }
    match host.rsplit_once(':') {
        Some((name, port)) if port.chars().all(|c| c.is_ascii_digit()) => name,
        _ => host,
    }
}

/// A conservative validity check on a redirect target host, so a hostile `Host` header can
/// never smuggle anything surprising into the `Location` header.
fn is_valid_host(host: &str) -> bool {
    !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'))
}

/// Maximum size of a `/vm/compile` request body. The body is a short composition expression
/// over store-program names and algebra ops; anything larger is rejected before any work.
const MAX_COMPILE_BODY: usize = 2048;
/// A single server-side compile must finish within this budget or the request is abandoned.
const COMPILE_TIMEOUT: Duration = Duration::from_secs(20);
/// At most this many compiles run at once (the pulley compiler is CPU-heavy); further requests
/// wait up to [`COMPILE_ACQUIRE_WAIT`] for a slot before being turned away with 503.
const MAX_CONCURRENT_COMPILES: usize = 2;
/// How long a request waits for a compile slot before giving up with 503. Bounds the queue so
/// a burst cannot pile up unboundedly, while letting normal bursts through (a compile is fast).
const COMPILE_ACQUIRE_WAIT: Duration = Duration::from_secs(10);
/// Process-wide compile concurrency gate, created on first use.
static COMPILE_PERMITS: OnceLock<Semaphore> = OnceLock::new();

/// Handle `POST /vm/compile`: read a size-capped composition expression, fuse + compile it to
/// a pulley32 image under a concurrency limit and a time bound, and return the image (or a
/// typed 4xx/5xx). Carries the same standard security headers as every other response.
async fn compile_response(
    body: Incoming,
    site_root: &Path,
    transport: Transport,
) -> Response<Body> {
    with_standard_headers(compile_response_inner(body, site_root).await, transport)
}

async fn compile_response_inner(body: Incoming, site_root: &Path) -> Response<Body> {
    // 1. Size-capped body read — never buffer more than MAX_COMPILE_BODY from the client.
    let collected = match Limited::new(body, MAX_COMPILE_BODY).collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => {
            return compile_text_response(StatusCode::PAYLOAD_TOO_LARGE, "composition too large");
        }
    };
    let expr = match std::str::from_utf8(&collected) {
        Ok(s) => s.trim().to_string(),
        Err(_) => {
            return compile_text_response(StatusCode::BAD_REQUEST, "request body must be UTF-8");
        }
    };
    if expr.is_empty() {
        return compile_text_response(StatusCode::BAD_REQUEST, "empty composition");
    }

    // 2. Concurrency limit — wait briefly for a slot, then turn away. Held until the compile
    //    finishes, so at most MAX_CONCURRENT_COMPILES run at once.
    let sem = COMPILE_PERMITS.get_or_init(|| Semaphore::new(MAX_CONCURRENT_COMPILES));
    let permit = match timeout(COMPILE_ACQUIRE_WAIT, sem.acquire()).await {
        Ok(Ok(p)) => p,
        Ok(Err(_closed)) => {
            return compile_text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "compiler unavailable",
            );
        }
        Err(_elapsed) => {
            return compile_text_response(StatusCode::SERVICE_UNAVAILABLE, "compiler busy, retry");
        }
    };

    // 3. The allow-set is exactly the raw components shipped under site/vm/raw — never the
    //    client's word for what exists. An empty set means the site was built without them.
    let raw_dir = site_root.join("vm").join("raw");
    let allow = allowed_programs(&raw_dir).await;
    if allow.is_empty() {
        return compile_text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "no compile components installed on this server",
        );
    }

    // 4. Compile on a blocking thread (CPU-bound), abandon it if it overruns the time budget.
    let job = tokio::task::spawn_blocking(move || {
        crate::compile::compile_expression(&expr, &raw_dir, &allow)
    });
    let outcome = timeout(COMPILE_TIMEOUT, job).await;
    drop(permit);
    match outcome {
        Ok(Ok(Ok(image))) => compiled_image_response(image),
        Ok(Ok(Err(err))) => compile_error_response(err),
        Ok(Err(_join)) => {
            compile_text_response(StatusCode::INTERNAL_SERVER_ERROR, "compiler crashed")
        }
        Err(_elapsed) => compile_text_response(StatusCode::GATEWAY_TIMEOUT, "compile timed out"),
    }
}

/// The store-program names the site actually ships (the stems of `site/vm/raw/*.wasm`). This
/// is the allow-set: a referenced name not present here is rejected.
async fn allowed_programs(raw_dir: &Path) -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(mut rd) = tokio::fs::read_dir(raw_dir).await {
        while let Ok(Some(entry)) = rd.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("wasm")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            {
                names.push(stem.to_string());
            }
        }
    }
    names
}

/// A successful compile: the opaque pulley32 image, not cached (it is composition-specific and
/// cheap to regenerate; the browser caches nothing for these).
fn compiled_image_response(image: Vec<u8>) -> Response<Body> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Full::new(Bytes::from(image)))
        .expect("statically valid response")
}

/// A short plaintext compile status (errors and refusals), so the browser eosh can surface the
/// reason verbatim.
fn compile_text_response(status: StatusCode, msg: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Full::new(Bytes::from(format!("{msg}\n"))))
        .expect("statically valid response")
}

/// Map a [`crate::compile::CompileError`] to an HTTP status: bad input is the client's fault
/// (4xx), a missing server component or compiler failure is ours (5xx).
fn compile_error_response(err: crate::compile::CompileError) -> Response<Body> {
    use crate::compile::CompileError;
    let status = match &err {
        CompileError::BadExpression(_) | CompileError::UnknownProgram(_) => StatusCode::BAD_REQUEST,
        CompileError::CompileFailed(_) => StatusCode::UNPROCESSABLE_ENTITY,
        CompileError::MissingComponent(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    compile_text_response(status, &err.to_string())
}

fn error_response(status: StatusCode) -> Response<Body> {
    let reason = status.canonical_reason().unwrap_or("error");
    let body = format!(
        "<!doctype html>\n<html><head><title>{status}</title></head>\
         <body><h1>{} {reason}</h1><p><a href=\"/\">eo9.org</a></p></body></html>\n",
        status.as_u16()
    );
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Full::new(Bytes::from(body)))
        .expect("statically valid response")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_port_handles_names_and_literals() {
        assert_eq!(strip_port("eo9.org"), "eo9.org");
        assert_eq!(strip_port("eo9.org:8080"), "eo9.org");
        assert_eq!(strip_port("[::1]:8080"), "[::1]");
        assert_eq!(strip_port("[::1]"), "[::1]");
        assert_eq!(strip_port("127.0.0.1:80"), "127.0.0.1");
    }

    #[test]
    fn redirect_uses_host_header_then_fallback() {
        let uri: Uri = "/style.css?v=1".parse().unwrap();

        let location = |response: &Response<Body>| {
            response.headers()[header::LOCATION]
                .to_str()
                .unwrap()
                .to_owned()
        };

        let from_header = redirect_response(Some("eo9.org:8080"), &uri, Some("fallback.example"));
        assert_eq!(from_header.status(), StatusCode::MOVED_PERMANENTLY);
        assert_eq!(location(&from_header), "https://eo9.org/style.css?v=1");

        let from_fallback = redirect_response(None, &uri, Some("eo9.org"));
        assert_eq!(location(&from_fallback), "https://eo9.org/style.css?v=1");

        // A hostile Host header is ignored in favor of the canonical host.
        let hostile = redirect_response(Some("evil.example/path"), &uri, Some("eo9.org"));
        assert_eq!(location(&hostile), "https://eo9.org/style.css?v=1");

        // No usable host at all: refuse rather than guess.
        let none = redirect_response(None, &uri, None);
        assert_eq!(none.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn default_limits_are_sane() {
        let limits = Limits::default();
        assert!(limits.max_connections >= 64);
        assert!(limits.tls_handshake_timeout <= limits.max_connection_lifetime);
        assert!(limits.header_read_timeout <= limits.max_connection_lifetime);
    }
}
