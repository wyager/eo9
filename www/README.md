# www — the eo9.org website

This directory holds the public website for Eo9 (served at **eo9.org**) and the Rust server
that serves it directly to the internet — the server terminates TLS itself and obtains its
own certificates from Let's Encrypt; there is no reverse proxy. It is deliberately **not** a
member of the repository's root Cargo workspace (the same arrangement as `guest/` and
`kernel/`): it has its own `Cargo.toml` and lockfile, so nothing here ever enters the OS
dependency tree. Build and test it with plain `cargo` from this directory.

## Layout

- `site/` — the static site content: hand-written `index.html`, one small `style.css`, and
  `logo.svg` (which doubles as the favicon). No analytics, no external assets, and no JavaScript
  except on the `/try/` page (see "The /try page" below), which is the one page that needs it.
- `site/try/` — the in-browser demo: a hand-written page (`index.html`, `try.css`, `try.js`,
  `host.js`) plus the generated `components/` bundle it loads (committed; regenerate with
  `cargo xtask build-web-demo`).
- `try-build/` — the build-time helper (its own Cargo workspace, like `www/` itself) that
  produces `site/try/components/`. It is not needed to build, test, or deploy the server.
- `src/` — `eo9-www`: a library (`lib.rs` config + path resolution, `server.rs` listeners and
  request handling, `tls.rs` certificates) plus a thin binary.
- `tests/` — integration tests that run the real listeners on ephemeral ports, over plain
  HTTP and over TLS with a self-signed certificate.

## Server choice

The server is built on **hyper** (HTTP/1.1), **tokio**, **rustls**, and **rustls-acme**:

- `rustls` with the `ring` provider does all TLS — no OpenSSL linkage, no cmake, nothing to
  install on the deployment host.
- `rustls-acme` implements the ACME client and certificate renewal against Let's Encrypt
  using TLS-ALPN-01 challenges, and plugs into rustls as a certificate resolver, so renewal
  needs no restarts and no extra listener.
- `hyper` + `tokio` is the smallest well-maintained HTTP layer that composes cleanly with
  `tokio-rustls` and `rustls-acme` (both are async). The earlier synchronous `tiny_http`
  version could not drive modern rustls or ACME cleanly, so it was replaced when TLS moved
  in-process. No web framework (axum/tower) is used — routing is just "resolve a path under
  `site/`" — and there is no HTTP/2: a site this small gains nothing from it.

Everything above the HTTP layer (path resolution, content types, caching, redirects, error
responses) is our own code. All dependency versions are pinned in `Cargo.toml` and locked in
`Cargo.lock`; the tree stays confined to this directory.

What the server does:

- serves `site/` with correct `Content-Type` headers (and UTF-8 charsets for text);
- `Cache-Control: public, max-age=300` for HTML, `max-age=86400` for other assets;
- `/` and directory paths resolve to `index.html`;
- GET and HEAD only (anything else is `405`); missing files are a clean `404`;
- path-traversal safe: `..` segments, encoded traversal (`%2e%2e`), backslashes, and
  symlinks that escape the site root are all rejected — nothing outside `site/` is ever
  served;
- malformed percent-encoding gets a `400`; malformed HTTP is rejected by the HTTP layer
  without taking the server down;
- in the HTTPS modes, the plain-HTTP listener 301-redirects everything to HTTPS (preserving
  host, path, and query);
- every listener caps concurrent connections and applies handshake/header/lifetime deadlines
  (see "Built-in limits" under deployment).

## Modes

| Mode | How it is selected | What it does |
|------|--------------------|--------------|
| Plain HTTP | default (no TLS flags) | serves the site on `--bind` (default `127.0.0.1:8080`) — local development |
| ACME | `--domain … --acme-email …` | HTTPS on `--https-bind` (default `0.0.0.0:443`) with certificates obtained and renewed automatically from Let's Encrypt; HTTP on `--bind` (default `0.0.0.0:80`) redirects to HTTPS |
| Manual TLS | `--tls-cert … --tls-key …` | HTTPS with a certificate chain + key you provide (PEM); HTTP redirects as above |

Configuration (flag beats environment variable beats default):

| Flag | Environment | Default | Meaning |
|------|-------------|---------|---------|
| `--site DIR` | `EO9_WWW_SITE` | `./site` | directory to serve |
| `--bind ADDR:PORT` | `EO9_WWW_BIND` | `127.0.0.1:8080` / `0.0.0.0:80` | HTTP listener (serves in plain mode, redirects in the HTTPS modes) |
| `--https-bind ADDR:PORT` | `EO9_WWW_HTTPS_BIND` | `0.0.0.0:443` | HTTPS listener |
| `--domain NAME` | — | — | domain to certify; repeatable, first one is canonical |
| `--acme-email ADDR` | — | — | Let's Encrypt account contact (required with `--domain`) |
| `--acme-cache DIR` | — | `./acme-cache` | where certificates and the account key are cached |
| `--acme-staging` | — | off | use the Let's Encrypt staging environment |
| `--tls-cert FILE`, `--tls-key FILE` | — | — | PEM certificate chain and private key (manual mode) |

## Build, run, test

```sh
cd www
cargo build
cargo test
cargo run                      # plain HTTP: serves ./site on http://127.0.0.1:8080/
```

To try HTTPS locally, make a self-signed certificate and use manual mode:

```sh
openssl req -x509 -newkey rsa:2048 -nodes -keyout key.pem -out cert.pem \
        -days 30 -subj "/CN=localhost" -addext "subjectAltName=DNS:localhost"
cargo run -- --tls-cert cert.pem --tls-key key.pem \
             --https-bind 127.0.0.1:8443 --bind 127.0.0.1:8080
curl -k https://localhost:8443/        # -k because the certificate is self-signed
```

`cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo build`, and `cargo test` are the
CI bar for this directory (the root `xtask ci` does not cover it, by design).

What the tests cover: content types, cache headers, 404/405/400, traversal rejection, and
HEAD handling over plain HTTP **and** over TLS (manual mode with a generated self-signed
certificate), plus the HTTP→HTTPS redirect and the config/flag validation for all three
modes. The ACME exchange itself cannot be tested end to end locally (Let's Encrypt has to
reach port 443 on a public DNS name), but everything below the certificate order — the TLS
accept path, challenge-vs-normal handshake split, request handling, redirect — is the same
code the tests exercise.

## The /try page

`/try/` runs the real Eo9 example components — the same `.wasm` artifacts `cargo xtask
build-guest` produces — in the visitor's browser. At build time `www/try-build` translates each
component from the Component Model binary format into an ES module plus core wasm files using
`js-component-bindgen` (the transpiler library inside jco, pinned in `try-build/Cargo.toml`) and
writes a `manifest.json` describing each program's imports, typed `main` signature, and outcome
variants. At run time the page's hand-written JavaScript provides a small terminal, a launcher,
and the **browser host**: implementations of the root capabilities the programs import
(`eo9:text` → the terminal, `eo9:time` → the browser clock, `eo9:fs` → an in-memory filesystem),
plus the loader rule — a program whose required import is not granted is refused before it is
instantiated. The page says explicitly what is real and what is not; in particular the prompt is
a launcher, **not** eosh.

Operational notes:

- Everything runs client-side; the server just serves the static files (`.js`, `.wasm`, `.json`
  content types are already in `content_type`). Nothing the visitor types leaves their machine.
- The generated bundle under `site/try/components/` is committed (~750 KiB for four programs), so
  deployment stays "copy `site/`" and needs neither Rust nor node. Regenerate it with
  `cargo xtask build-web-demo` (from the repository root) whenever the example components or the
  transpiler pin change, and commit the result.
- There is no third-party JavaScript: the terminal and host are hand-written ES modules
  (~700 lines total). The only build-time dependency is the `try-build` crate's Cargo tree.
- Programs with an `async func main` (currently `readwrite`) need JSPI
  (`WebAssembly.Suspending`); the page feature-detects it and says so when it is missing. The
  synchronous examples run in any modern browser.

## Deploying eo9.org (standalone)

1. **DNS.** Point an `A`/`AAAA` record for `eo9.org` (and any extra domains you pass with
   `--domain`) at the host. Let's Encrypt validates by connecting to port 443 on that name.
2. **Ports.** The server needs to bind 80 and 443. Either run it as root (not recommended),
   or grant the binary the capability / use a supervisor:
   - Linux: `sudo setcap 'cap_net_bind_service=+ep' /usr/local/bin/eo9-www`, or add
     `AmbientCapabilities=CAP_NET_BIND_SERVICE` to the systemd unit (below);
   - macOS (launchd) and systemd socket activation are alternatives, but the setcap /
     AmbientCapabilities route is the simplest.
3. **Install.** `cargo build --release`, copy `target/release/eo9-www` and the `site/`
   directory to the host (e.g. `/srv/eo9-www/site`). Updating the site content is just
   replacing `site/` — no rebuild of the server needed.
4. **Certificate cache.** Pick a persistent, private directory for `--acme-cache`
   (e.g. `/srv/eo9-www/acme-cache`). It holds the Let's Encrypt account key and the issued
   certificates; keeping it across restarts avoids re-ordering certificates and hitting rate
   limits. It is created on first use.
5. **Staging first.** Run once with `--acme-staging` to confirm DNS and port reachability
   against Let's Encrypt's staging environment (generous rate limits, untrusted test
   certificates — browsers will warn, which is expected). Then switch to production by
   dropping the flag and pointing `--acme-cache` at a fresh directory (staging and
   production certificates should not share a cache directory).
6. **Run.** The production invocation is:

```sh
eo9-www --domain eo9.org --acme-email you@example.com \
        --acme-cache /srv/eo9-www/acme-cache --site /srv/eo9-www/site
```

A minimal systemd unit:

```ini
[Unit]
Description=eo9.org web server
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/local/bin/eo9-www --domain eo9.org --acme-email you@example.com \
          --acme-cache /srv/eo9-www/acme-cache --site /srv/eo9-www/site
WorkingDirectory=/srv/eo9-www
User=eo9www
AmbientCapabilities=CAP_NET_BIND_SERVICE
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

Renewal is automatic: the server re-orders a certificate when the cached one approaches
expiry, with no restart and no downtime. If you ever need to supply a certificate from
elsewhere instead, use `--tls-cert`/`--tls-key` (manual mode) — the rest of the behavior is
identical.

**Built-in limits.** Each listener caps concurrent connections at 256 and applies deadlines:
10 s for a TLS handshake, 10 s to read a request's headers (which also bounds idle
keep-alive waits), and 60 s for one connection's total lifetime. Stalled or hostile
(slowloris-style) clients are therefore dropped instead of pinning sockets or tasks. The
values live in `Limits` in `src/server.rs` (deliberately not CLI flags — change them in code
if eo9.org ever needs different numbers).

## The logo

`site/logo.svg` is the Eo9 mark: a filled circle (a program) held inside a rounded-square
frame (its capability boundary). It is a hand-written SVG — two shapes and a few lines of
embedded CSS so the frame follows the viewer's light/dark preference — with no text and no
font dependencies, legible from 16px up. To change it, edit the SVG directly; it is also
referenced as the favicon from `index.html`, so one file covers both uses.

## Decisions

- The server faces the internet directly and terminates TLS itself; certificates come from
  Let's Encrypt via TLS-ALPN-01 (port 443 only — port 80 exists purely to redirect), with
  `--tls-cert`/`--tls-key` as the manual fallback and plain HTTP for development.
- Stack: hyper + tokio + rustls (ring) + rustls-acme (see "Server choice"). The original
  tiny_http version was replaced when TLS termination moved into the server.
- Traversal handling returns `404` (not `403`) so probing reveals nothing about what exists
  outside the site root.
- The site is static files only; if the site ever needs more pages, add more `.html` files
  to `site/` — the server needs no changes.
- The `/try` page keeps that property: it is static files too (hand-written JS plus a committed,
  generated component bundle), produced by `www/try-build` via `cargo xtask build-web-demo`. The
  in-page terminal is hand-rolled rather than a vendored xterm.js, so the site ships no
  third-party JavaScript at all. Design decisions and feasibility notes live in
  `plan/15-website.md`.
