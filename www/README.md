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
  except on the `/vm/` try-it page (see "The /vm try-it page" below), which is the one page that
  needs it.
- `site/vm/` — the try-it page: a hand-written page (`index.html`, `vm.css`, `vm.js`) plus the
  committed, fingerprinted wasm blob and program store it loads (regenerate with
  `cargo xtask build-web-vm`; `cargo xtask check-web-vm` verifies the committed set).
- `web-eo9/` — the wasm32 blob's own Cargo workspace (the real Eo9 runtime + algebra + compiler
  compiled to WebAssembly). It is not needed to build, test, or deploy the server.
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

## The /vm try-it page

`/vm/` boots the real Eo9 shell in the visitor's browser: the page loads a wasm32 blob containing
the actual Eo9 stack — the wasmtime-based runtime (Pulley interpreter backend), the component
algebra, the Cranelift→Pulley compiler, and the unmodified `eosh` — plus a fingerprinted store of
pre-compiled programs. The page's hand-written JavaScript provides only the roots a browser must
provide (terminal text, clocks, entropy, JSPI suspension); everything else, including capability
enforcement and composition, runs inside the blob. Design notes and decisions live in
`plan/18-web.md`; the earlier jco-transpile demo (`/try/`) was removed when this page took over
(plan/15 Decision 25).

Operational notes:

- Everything runs client-side; the server just serves static files. Nothing the visitor types
  leaves their machine.
- The blob and program store under `site/vm/` are committed and content-fingerprinted, so
  deployment stays "copy `site/`" and needs neither Rust nor node. Regenerate with
  `cargo xtask build-web-vm` (from the repository root) and verify with `cargo xtask
  check-web-vm`; commit the result.
- There is no third-party JavaScript: the terminal and host glue are hand-written ES modules.
- The shell's read-line needs JSPI (`WebAssembly.Suspending`); the page feature-detects it and
  says so when it is missing (current Chrome/Edge have it).

## Deploying eo9.org (standalone)

> The live eo9.org deployment now runs **behind Cloudflare** with a manual Cloudflare Origin
> CA certificate (manual TLS mode), not the standalone Let's Encrypt setup described here —
> see [Updating the live deployment](#updating-the-live-deployment) for the current layout.
> This section documents the self-contained, direct-to-internet ACME deployment the server
> also supports (and the way eo9.org ran before the Cloudflare migration).

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

### Updating the live deployment

The production host runs the **clone-in-place** variant of the steps above, but it now sits
**behind Cloudflare**: the Cloudflare proxy terminates TLS for visitors and caches assets, and
the origin presents a long-lived **Cloudflare Origin CA certificate** (manual TLS mode) rather
than running ACME. The repository is cloned to `/opt/eo9`, built there with the repo-pinned
toolchain, and the systemd unit `eo9-www` runs the binary straight out of that build tree as
the unprivileged `eo9www` user.

> **SSH to the origin by IP, not by name.** `eo9.org` resolves to Cloudflare's proxy, which
> only forwards HTTP/HTTPS — `ssh root@eo9.org` will time out. Reach the box at its origin IP:
> `ssh root@64.177.116.122`.

The layout:

| Path | What |
|------|------|
| `/opt/eo9` | the git clone (tracks `origin/master`) |
| `/opt/eo9/www/target/release/eo9-www` | the built binary (the unit's `ExecStart`) |
| `/opt/eo9/www/site` | the served site (`--site`) |
| `/srv/eo9-www/tls/{cert.pem,key.pem}` | the Cloudflare Origin CA cert + key (manual TLS); `key.pem` is `0600 eo9www` |
| `/etc/systemd/system/eo9-www.service` | the unit (`systemctl enable`d; starts on boot). `eo9-www.service.acme.bak` is the previous ACME unit, kept for rollback |

**Site content only** (anything under `www/site/`) — no rebuild and no restart. The server
reads files from disk per request, so a pull is enough:

```sh
cd /opt/eo9 && git pull
```

Cloudflare caches static assets at its edge, so after changing a file you may need to **purge
the Cloudflare cache** (dashboard → Caching → Configuration → Purge) for visitors to see it
before the edge TTL lapses. (HTML is not edge-cached; static assets are.)

**Server code or dependencies** (`www/src/`, `www/Cargo.toml`, `www/Cargo.lock`) — rebuild,
then restart. Build as root (the SSH user); cargo selects the repo-pinned nightly
automatically. On the 1-vCPU host the build takes a few minutes and leans on swap, but the
old binary keeps serving the whole time — only the final `restart` swaps it in:

```sh
cd /opt/eo9 && git pull
cd /opt/eo9/www && cargo build --release   # if `cargo` isn't found: . "$HOME/.cargo/env"
systemctl restart eo9-www
```

A restart just re-reads the certificate from disk and rebinds — a sub-second blip, no ACME and
no rate limits. The Origin CA certificate is valid for ~15 years, so there is nothing to renew
on this host; to rotate it, replace `/srv/eo9-www/tls/{cert.pem,key.pem}` (keep `key.pem` at
`0600 eo9www`) and `systemctl restart eo9-www`.

**Confirm it worked:**

```sh
systemctl status eo9-www --no-pager
journalctl -u eo9-www -n 20 --no-pager           # expect the "serving … on https://…" line
curl -sI https://eo9.org/ | head -1              # through Cloudflare; expect HTTP/2 200
echo | openssl s_client -connect 64.177.116.122:443 -servername eo9.org 2>/dev/null \
  | openssl x509 -noout -issuer                  # origin should present the CloudFlare Origin CA cert
```

If `git pull` ever reports local changes (e.g. an out-of-band edit to `Cargo.lock`), reconcile
the clone to the remote with `git fetch origin master && git reset --hard origin/master` — the
build tree should carry nothing that diverges from `origin/master`, and the certificate lives
outside it (`/srv/eo9-www/tls`), so this is safe.

**Cloudflare caching.** Cloudflare edge-caches `.js`/`.css`/images by file extension
automatically, but **not `.wasm`** — including the eo9 VM at `/vm/web-eo9.wasm`. To cache the
wasm blobs, add a Cache Rule (dashboard → Caching → Cache Rules) matching e.g.
`ends_with(http.request.uri.path, ".wasm")` with *Cache eligibility: Eligible for cache*; the
origin already sends `Cache-Control: public, max-age=86400`, which Cloudflare then honors.
Keep the SSL/TLS mode at **Full (strict)** so the Origin CA cert is validated. Confirm with
`curl -sI https://eo9.org/vm/web-eo9.wasm | grep -i cf-cache-status` (expect `HIT` after the
first request).

> **Heads-up on `cargo update` (ACME mode only).** `www/Cargo.lock` pins `http = 1.4.0` on
> purpose: `http` 1.4.1 makes `async-web-client` (pulled in by `rustls-acme`) panic during a
> live ACME order. The live host no longer uses ACME (it serves a Cloudflare Origin CA cert),
> but ACME mode still exists in the binary, so don't let a `cargo update` bump `http` back
> without re-validating ACME against Let's Encrypt staging (`--acme-staging`).

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
- The `/vm` try-it page keeps that property: it is static files too (hand-written JS plus a
  committed, fingerprinted wasm blob and program store), produced by `cargo xtask build-web-vm`.
  The in-page terminal is hand-rolled rather than a vendored xterm.js, so the site ships no
  third-party JavaScript at all. Design decisions and feasibility notes live in
  `plan/15-website.md` and `plan/18-web.md`.
