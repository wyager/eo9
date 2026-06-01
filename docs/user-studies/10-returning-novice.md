# User study 10 — returning novice (saw the round-1 site, came back to see if they get it now)

## Session metadata

- **Date:** 2026-05-31
- **Branch / worktree:** `docs/study-10` (worktree of master at `5985249`)
- **Participant persona:** a curious developer — frontend/backend web work, comfortable with a
  terminal, not a systems programmer. They visited eo9.org once, months ago, when the site was
  rough; they bounced off it and remember only "some capability OS thing". They are returning
  to see whether it makes sense to them now. Zero other Eo9 context, no repository access.
- **Session focus:** the returning-visitor experience — does the site as it stands today land
  for someone who bounced off the earlier version? What clicks, what is still confusing, what
  is newly confusing? Compared throughout against the round-2 novice study
  (`docs/user-studies/06-novice.md`) to see which of that participant's complaints are fixed.
- **Methodology:** the participant was a role-played persona run as a separate session with no
  access to the repository or any tools — it saw only what the demoer pasted in, in stages
  (page text, then terminal transcripts), and replied conversationally. Every page shown was
  actually served and fetched; every command shown was actually executed in the study
  environment; outputs are verbatim, trimmed only for length.
- **Environment:** Apple Silicon macOS host. The website was built (`cargo build` in `www/`)
  and served with the real server (`eo9-www --site site --bind 127.0.0.1:8097`); both pages
  were fetched over HTTP as a browser would. The browser terminal experience was produced with
  the committed `/vm` blob driven through `node www/web-eo9/verify-eosh.mjs` (Node v25, JSPI)
  and a transcript harness that mirrors the page's import glue. Usermode demos used a fresh
  `eo9` build (`cargo build -p eo9` + `cargo xtask build-guest`) with `EO9_STORE` pointed at an
  empty throwaway directory, so the participant saw what a brand-new machine sees.
- **Shape:** demoer prepared all materials first (website capture, browser-terminal
  transcripts, usermode first-run transcripts), then ran the participant through five phases —
  front page → try-it page → browser terminal → usermode first run → structured wrap-up.

## The website walkthrough (what was served and captured)

The site server built in 8.2 s and serves the committed site:

```
$ cd www && cargo build
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 8.18s
$ ./target/debug/eo9-www --site site --bind 127.0.0.1:8097

$ curl -s -o /dev/null -w "%{http_code} %{size_download}\n" http://127.0.0.1:8097/
200 7901
$ curl -s -o /dev/null -w "%{http_code} %{size_download}\n" http://127.0.0.1:8097/vm/
200 5829
$ curl -s -o /dev/null -w "%{http_code} %{size_download}\n" -H "Accept-Encoding: br" \
    http://127.0.0.1:8097/vm/web-eo9.4e9e89baa375be13.wasm
200 1742167
```

The front page (`/`) is one screen of prose: hero ("A capability-secure operating system built
on the WebAssembly Component Model"), three links (Try it in your browser / Source on GitHub /
Read the spec), an early-development status note, then sections: What Eo9 is / Composition is
the whole interface (with a three-line shell example) / Virtualization and sandboxing by
substitution / Deterministic execution / Performance / eofs / Bare metal and hosted / Where it
stands.

The try-it page (`/vm/`) is the terminal (`fetching the Eo9 OS (about 1.8 MB compressed)…` —
the brotli-compressed blob measured above is 1.74 MB, so the claim is honest), an
"Explore the sandbox" section teaching the loop (`help` → `ls /bin` → `describe hello` /
`describe entropy.seeded` → `entropy.seeded --seed 7 $ rng --count 2`), a "Things to try" list
(bare `hello`, seeded rng, two `only` lockdowns, the frozen clock, coreutils against the
in-memory fs), and a "What this is actually doing" section that names the one honest
difference (Pulley interpreter instead of native codegen) and the requirements (JSPI-capable
browser).

*(Draft checkpoint — browser-terminal transcripts, usermode first-run, participant phases, and
findings follow.)*
