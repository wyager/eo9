# www — the eo9.org website

This directory holds the public website for Eo9 (served at **eo9.org**) and the small Rust
server that serves it. It is deliberately **not** a member of the repository's root Cargo
workspace (the same arrangement as `guest/` and `kernel/`): it has its own `Cargo.toml` and
lockfile, so nothing here ever enters the OS dependency tree. Build and test it with plain
`cargo` from this directory.

## Layout

- `site/` — the static site content: hand-written `index.html`, one small `style.css`, and
  `logo.svg` (which doubles as the favicon). No JavaScript, no analytics, no external assets.
- `src/` — `eo9-www`, a small static-file server (library + thin binary).
- `tests/` — integration tests that run the real server on an ephemeral port.

## Server choice

The server is built on [`tiny_http`](https://crates.io/crates/tiny_http) (0.12.0): a small,
synchronous HTTP/1.1 server with three tiny transitive dependencies (`ascii`, `httpdate`,
`chunked_transfer`) and no async runtime. A framework like axum would pull in tokio, hyper,
and tower to serve a handful of static files — the opposite of this repository's
minimal-dependency rule — while hand-rolling HTTP request parsing on raw sockets is the kind
of avoidable sharp edge `tiny_http` exists to cover. Everything above the HTTP layer
(path resolution, content types, caching, error responses) is our own code in `src/lib.rs`.

What the server does:

- serves `site/` with correct `Content-Type` headers (and UTF-8 charsets for text);
- `Cache-Control: public, max-age=300` for HTML, `max-age=86400` for other assets;
- `/` and directory paths resolve to `index.html`;
- GET and HEAD only (anything else is `405`); missing files are a clean `404`;
- path-traversal safe: `..` segments, encoded traversal (`%2e%2e`), backslashes, and
  symlinks that escape the site root are all rejected — nothing outside `site/` is ever
  served;
- malformed percent-encoding gets a `400`; malformed HTTP is rejected by the HTTP layer
  without taking the server down.

## Build, run, test

```sh
cd www
cargo build
cargo test
cargo run                      # serves ./site on http://127.0.0.1:8080/
cargo run -- --bind 0.0.0.0:9090 --site site
```

Configuration (flag beats environment variable beats default):

| Flag      | Environment    | Default          | Meaning               |
|-----------|----------------|------------------|-----------------------|
| `--bind`  | `EO9_WWW_BIND` | `127.0.0.1:8080` | address to listen on  |
| `--site`  | `EO9_WWW_SITE` | `./site`         | directory to serve    |

`cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo build`, and `cargo test` are the
CI bar for this directory (the root `xtask ci` does not cover it, by design).

## Deploying behind a reverse proxy

`eo9-www` speaks plain HTTP and does **no TLS termination**. Run it bound to localhost and
put a reverse proxy (nginx, Caddy, …) in front of it to terminate HTTPS for eo9.org. For
example, with nginx:

```nginx
server {
    server_name eo9.org;
    # ... TLS configuration (certificates, redirect from port 80) ...
    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header Host $host;
    }
}
```

Deployment is: build the binary (`cargo build --release`), copy `target/release/eo9-www`
and the `site/` directory to the host, and run
`eo9-www --bind 127.0.0.1:8080 --site /path/to/site` under your process supervisor of
choice. The content is plain files, so updating the site is just replacing `site/` —
no rebuild of the server needed.

## The logo

`site/logo.svg` is the Eo9 mark: a filled circle (a program) held inside a rounded-square
frame (its capability boundary). It is a hand-written SVG — two shapes and a few lines of
embedded CSS so the frame follows the viewer's light/dark preference — with no text and no
font dependencies, legible from 16px up. To change it, edit the SVG directly; it is also
referenced as the favicon from `index.html`, so one file covers both uses.

## Decisions

- `tiny_http` over axum/hyper or raw `std::net`: smallest dependency footprint that still
  handles HTTP parsing correctly (see "Server choice" above).
- TLS is out of scope here; eo9.org HTTPS is terminated by a reverse proxy.
- Traversal handling returns `404` (not `403`) so probing reveals nothing about what exists
  outside the site root.
- The site is static files only; if the site ever needs more pages, add more `.html` files
  to `site/` — the server needs no changes.
