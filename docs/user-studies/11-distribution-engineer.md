# User study 11 — devtools / distribution engineer

## Session metadata

- **Date:** 2026-05-31
- **Branch / worktree:** `docs/study-11` (worktree of master at `5985249`)
- **Participant persona:** a developer-experience / release engineer, ~10 years of
  experience packaging and shipping developer tools (Homebrew formulae, cargo/npm/pip
  publishing, CI release pipelines, install docs). Evaluates one question: *"if this team
  asked me to ship Eo9 to real users tomorrow, what would I have to fix first?"* No
  WebAssembly background, no prior exposure to Eo9.
- **Methodology:** the participant is a role-played persona run as a separate session with
  no access to the repository or any tools — it sees only what the facilitator pastes.
  Every command shown was actually executed by the facilitator in the study environment;
  outputs are verbatim, trimmed only for length. Failures are shown as they happened.
- **Environment:** a fresh worktree of master on an Apple Silicon macOS host (no
  pre-existing `target/` directories — every build below is what a fresh checkout pays),
  warm cargo registry cache, rustup with the pinned nightly + stable installed,
  `wasm-tools` 1.250.0 and QEMU 11.0.0 on PATH. The facilitator's `eo9` binary was
  installed with `cargo install --path crates/eo9` into a **worktree-local `--root`**
  (and `CARGO_TARGET_DIR` inside the repo) so the host machine's real `~/.cargo/bin/eo9`
  and `~/.eo9` store were never touched; all runs use a fresh `EO9_STORE`. This is the
  identical code path to the README's install line — only the destination differs.
- **Focus:** how Eo9 *ships* — the Makefile/doctor UX, the README as install
  documentation, `cargo xtask package` and the 8-crate publish chain, the `cargo install
  eo9` experience (bundled-seed first run), platform assumptions, and the missing-tool UX.

## Phase 1 — the front door: `make help`, `doctor`, `make setup`

### `make help` (and bare `make`)

Both print the same six-line index (the default goal is `help`):

```
$ make
Eo9 — common entry points:
  make setup      install/verify prerequisites (Rust targets, wasm-tools; checks QEMU)
  make shell      build the components and drop into the eosh shell on your host
  make www        serve the website + the in-browser shell at http://127.0.0.1:8080/
  make www-build  rebuild the /vm in-browser shell assets from source, then serve
  make qemu       boot the bare-metal kernel in QEMU to an eosh prompt (aarch64)
  make ci         run the full local gate (host + guest + kernel workspaces)
```

Accurate, short, and the default target is the help — a fresh `make` cannot do anything
destructive. No findings.

### `cargo xtask doctor` — all green, but look at what it took

First invocation in the fresh worktree (warm cargo registry cache):

```
$ time cargo xtask doctor
   Compiling proc-macro2 v1.0.106
   Compiling quote v1.0.45
   [… ~200 more crates, including wasmtime v45.0.0, cranelift-codegen v0.132.0,
      wit-component, wasm-compose, zstd-sys (a C build) …]
   Compiling xtask v0.1.0 (…/xtask)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 23.05s
     Running `target/debug/xtask doctor`
xtask doctor — checking the host tools and toolchains this repository needs

  ok       rustup
  ok       pinned toolchain nightly-2026-05-25
  ok       wasm32-unknown-unknown target
  ok       aarch64-unknown-none target
  ok       wasm-tools 1.250.0
  ok       QEMU emulator version 11.0.0
  ok       node v25.2.1

xtask: doctor: everything required is installed
cargo xtask doctor  0.26s user 0.86s system 4% cpu 24.436 total
```

The output itself is excellent: one line per tool, required vs `optional` vs `note`
clearly distinguished, an install hint for anything missing, and a one-line verdict.
Subsequent runs are instant (0.1–0.3 s).

**The wart is structural:** `doctor` is an xtask subcommand, and xtask depends on
`eo9-component` → `wasmtime`/`cranelift`. So the *prerequisite checker* itself requires:
the pinned nightly toolchain (rustup auto-installs it, several hundred MB), then
compiling ~200 crates including all of wasmtime — **before it can tell you whether your
machine is set up**. With a warm registry cache that is 23 s; on a genuinely fresh
machine it is a multi-minute, multi-GB download-and-compile *to run a tool whose job is
to check whether you can build*. The Makefile has the same shape: `make setup` ends with
`cargo xtask doctor`.

### `make setup` — idempotency

Run twice back-to-back; both runs identical and ~2 s:

```
$ make setup
rustup target add wasm32-unknown-unknown
info: component 'rust-std' for target 'wasm32-unknown-unknown' is up to date

Prerequisite summary:
  ok       rustup (the pinned nightly + per-workspace targets install on first build)
  ok       wasm32-unknown-unknown target
  ok       wasm-tools
  ok       qemu-system-aarch64
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.13s
     Running `target/debug/xtask doctor`
xtask doctor — checking the host tools and toolchains this repository needs
  ok       rustup
  ok       pinned toolchain nightly-2026-05-25
  ok       wasm32-unknown-unknown target
  ok       aarch64-unknown-none target
  ok       wasm-tools 1.250.0
  ok       QEMU emulator version 11.0.0
  ok       node v25.2.1
xtask: doctor: everything required is installed
$ make setup        # second run: byte-identical, 1.97 s
```

Idempotent, fast, safe to re-run. Two nits:

1. **Two overlapping summaries.** `make setup` prints its own four-line "Prerequisite
   summary" and then runs `doctor`, which prints a seven-line summary of the same things
   plus more (nightly, kernel target, node). Same information, two formats, one after the
   other.
2. **`make setup` cannot fail on a missing tool.** The Makefile invokes doctor as
   `-@cargo xtask doctor` — the leading `-` tells make to *ignore* its exit status. So on
   a machine where doctor reports `MISSING wasm-tools`, `make setup && make ci` sails
   straight past the check. (Verified in Phase 3 below: with `wasm-tools` removed from
   PATH and no cargo on PATH to reinstall it, `make setup` still exits 0.)

### `cargo xtask help` — a duplicate entry

The xtask command index lists **`check-web-vm` twice with two different descriptions**
(once as "rebuild and byte-compare against the committed files", once as "verify
vm/assets.json matches the committed fingerprinted assets"). Both blocks are in
`xtask/src/main.rs::print_help()`. Cosmetic, but it is the first thing a packaging
engineer reads.

## Phase 2 — the README, front to back, as a newcomer

The facilitator followed `README.md` top to bottom and ran every command exactly as
written (one substitution: the install `--root`/target dir redirected into the worktree,
see metadata). Repository state: fresh worktree, `make setup` already run.

### Quick-start table

- `make setup` — works (above).
- `make shell` — works: `cargo xtask build-guest` (11.4 s cold for all 50 components,
  every one validated by `wasm-tools validate`), then a dev-profile `eo9` build (11.9 s
  incremental), then a real `eosh>` prompt against a repo-local store
  (`target/eo9-store`), seeded with 50 programs on first run. The eosh `help` builtin's
  output is genuinely good (operators with examples, an "explore the sandbox" section).
- `make www` — not run (web demo is study 04's territory).
- `make qemu` — verified via the README's headless variant (below).
- `make ci` — not run in this study (it is the merge gate; its components — build-guest,
  build, test — were all exercised individually).

### Userspace mode — install and first run

```
$ cargo xtask build-guest          # 11.4 s, 50 components built + validated
$ cargo install --path crates/eo9 --force [--root <worktree-local>]
   [… release build of the host workspace …]
    Finished `release` profile [optimized] target(s) in 1m 21s
  Installing …/bin/eo9
   Installed package `eo9 v0.1.0` (executable `eo9`)
$ eo9 hello --name world --excited true        # fresh EO9_STORE
eo9: first run: seeded 50 bundled programs into the module store at …/store
[1780287863.596420000] Hello, world!
success(greeted)
$ echo $?
0
```

The README's claim structure holds exactly: first run seeds the store (50 programs, one
clear stderr line saying where), program output goes to stdout, the outcome line goes to
stderr. Verified by redirection:

```
$ eo9 hello --name world --excited true 2>/dev/null     # stdout only
[1780287892.285737000] Hello, world!
$ eo9 hello --name world --excited true >/dev/null      # stderr only
success(greeted)
```

### Every README example, result vs. claim

| README example | Claimed | Observed | Match |
|---|---|---|---|
| `eo9 hello --name world --excited true` | `Hello, world!` + `success(greeted)`, stderr outcome, exit 0 | exactly that | yes |
| `eo9 cruncher --seed 9 --rounds 200000` | `success(digest(14341732361190694547))` | identical digest | yes |
| `eo9 echo --text "hello pipes" \| tr a-z A-Z` | only program bytes through the pipe | `HELLO PIPES`; outcome stayed on stderr | yes |
| `eo9 cat notes.txt` (no grant) | refused before run, exit 3, friendly message | exactly that, names the fix (`--fs-root`) | yes |
| `eo9 --fs-root ./sandbox cat notes.txt` | file contents + `success(printed(28))` | exactly that | yes |
| `eo9 --fs-root ./sandbox readwrite …` | `success(round-tripped(2))` | exactly that | yes |
| `only eo9:text/text,eo9:time/time $ hello …` | runs, `ok: greeted` | runs, exit 0 | yes |
| `only eo9:text/text $ hello …` | friendly refusal naming the missing interface | exactly that, exit 3 | yes |
| `entropy.seeded --seed 43 $ rng --count 3` | three specific numbers, same every run | the *same three numbers as printed in the README*, identical across runs | yes |
| `time.frozen --now-seconds 1700000000 … $ hello …` | `[1700000000.000000000] Hello, frozen!` | exactly that | yes |
| `describe readwrite` / `env readwrite` | inspection output | works (see finding D7) | yes* |
| nested `eosh` | nested shell with same/less authority | works (piped stdin) | yes |
| `--max-fuel 100000 cruncher --rounds 200000000` | `abnormal(killed)` instead of a hot loop | `abnormal(killed)`, exit 2 | yes |
| eosh help's `only eo9:text,eo9:time $ hello` (package-level shorthand) | implied to work | works, exit 0 | yes |

This is a dramatically different result from user study 01 (2026-05-27), where the README
could not be followed verbatim. **Today every README example runs as written.** The
deny-by-default refusal, the stderr outcome discipline, the friendly `only` refusal text,
and the package-level shorthand have all landed since.

*The one inspection wart (finding D7): `eo9 -c "env readwrite"` — a README example —
reports `required eo9:rt/diagnostics@0.1.0 — missing — would be refused at spawn`, yet
running `readwrite` through the same shell session works fine (`ok: round-tripped(2)`).
The prediction and the behavior disagree; one of them is wrong.

### Bare-metal mode

The README's headless variant, run exactly as written (cold kernel build):

```
$ cargo xtask qemu aarch64 program=cruncher seed=9 rounds=200000
   [… kernel release build, 36.8 s, with dead-code warnings from vendored
      wasmtime-cranelift and eo9-kernel …]
xtask: built kernel image …/aarch64-unknown-none/release/eo9-kernel
xtask: booting … under qemu-system-aarch64 (serial on stdio …)

Eo9 kernel — aarch64 (QEMU virt)
  exception level: EL1
  …
store: 22 components baked in (1956 KiB components, 15442 KiB artifacts): eosh, hello, …
runner: selected `cruncher` from the kernel command line
runner: cruncher (276648 byte artifact) with kernel text/time/entropy providers
runner: cruncher outcome = success(digest(14341732361190694547))
runner: instantiate + main took 41439 us
[   62931 us] kernel run complete; requesting PSCI SYSTEM_OFF
                                             # exit 0; total wall 46.7 s incl. build
```

The README's headline claim — *"the same components run in userspace and on bare metal"*
— is verifiable in one command: the on-metal cruncher digest is bit-identical to the
usermode one. The only blemish is ~10 `dead_code`/`never read` warnings scrolling by
during the kernel build (already tracked in GAPS.md as a known nit).

## Phase 3 — packaging, clean install, stable toolchain, missing tools

*(extended after the draft commit — see below)*

## Findings (running list; triage table at the end)

- **D1 — `eo9` has no version flag.** `eo9 --version` → `error: unknown command`, exit 3.
  `eo9 -V` → same. `eo9 version` is interpreted as a *program name* and fails with
  `name version does not resolve in profile "default"`. A binary distributed via
  `cargo install eo9` cannot tell you what version it is — bug reports, support, and
  "is my install stale?" all have nothing to go on. The binary even embeds
  `EO9_WASMTIME_VERSION` and a target triple at build time (build.rs) — none of it is
  surfaced. `--help` exists and is good; `--version` does not.
- **D2 — the prerequisite checker needs the prerequisites.** `cargo xtask doctor` (and
  therefore `make setup`'s final check) requires the nightly toolchain and a ~200-crate
  compile (wasmtime, cranelift, a C build) before it can run. ~23 s warm; minutes + GBs
  on a fresh machine. Doctor cannot diagnose the most common broken state — "I can't
  build" — because it *is* a build.
- **D3 — `make setup` ignores doctor's exit code** (`-@cargo xtask doctor`), so a missing
  required tool does not fail setup. Scripted flows (`make setup && make ci` in CI docs)
  lose their guard.
- **D4 — duplicate summaries**: `make setup` prints two overlapping, differently formatted
  prerequisite summaries (its own + doctor's).
- **D5 — `cargo xtask help` lists `check-web-vm` twice** with two different descriptions.
- **D6 — kernel build warnings** during a README-following `cargo xtask qemu aarch64`
  (already tracked in GAPS as a known nit; noted here because a newcomer following the
  README sees them).
- **D7 — `env <program>` mispredicts a refusal**: `eo9 -c "env readwrite"` (a README
  example) says `eo9:rt/diagnostics … would be refused at spawn`, but the spawn succeeds.
- **D8 — outcome rendering still differs by surface**: `success(greeted)` from direct run
  vs `ok: greeted` from the shell (carried over from study 01; check synthesis tracking).

*(continued in Phase 3 / participant sections)*
