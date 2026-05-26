# www — the eo9.org website

This directory holds the public website for Eo9 (served at **eo9.org**) and the Rust server
that serves it directly to the internet — the server terminates TLS itself and obtains its
own certificates from Let's Encrypt; there is no reverse proxy. It is deliberately **not** a
member of the repository's root Cargo workspace (the same arrangement as `guest/` and
`kernel/`): it has its own `Cargo.toml` and lockfile, so nothing here ever enters the OS
dependency tree. Build and test it with plain `cargo` from this directory.

## Layout

- `site/` — the static site content: hand-written `index.html`, one small `style.css`, and
  `logo.svg` (which doubles as the favicon). No JavaScript, no analytics, no external assets.
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
