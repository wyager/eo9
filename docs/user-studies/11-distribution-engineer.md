# User study 11 — devtools / distribution engineer

## Session metadata

- **Date:** 2026-05-31
- **Branch / worktree:** `docs/study-11` (worktree of master at `5985249`)
- **Participant persona:** a developer-experience / release engineer, ~10 years of
  experience packaging and shipping developer tools (Homebrew formulae, cargo/npm/pip
  publishing, Debian packaging, CI release pipelines, install docs, version policy).
  Evaluates one question: *"if this team asked me to make this thing installable and
  shippable for real users, what is good, what is broken, and what would I have to fix
  first?"* No WebAssembly background, no prior exposure to Eo9.
- **Methodology:** the participant is a role-played persona run as a separate session with
  no access to the repository or any tools — it sees only what the facilitator pastes.
  Every command shown was actually executed by the facilitator in the study environment;
  outputs are verbatim, trimmed only for length. Failures are shown as they happened.
- **Environment:** a fresh worktree of master on an Apple Silicon macOS host (no
  pre-existing `target/` directories — every build below is what a fresh checkout pays),
  warm cargo registry cache, rustup with the pinned nightly + stable installed,
  `wasm-tools` 1.250.0 and QEMU 11.0.0 on PATH. The facilitator's `eo9` binaries were
  installed with `cargo install --path crates/eo9` into **worktree-local `--root`s**
  (and `CARGO_TARGET_DIR` inside the repo) so the host machine's real `~/.cargo/bin/eo9`
  and `~/.eo9` store were never touched; all runs use a fresh `EO9_STORE`. This is the
  identical code path to the README's install line — only the destination differs.
- **Focus:** how Eo9 *ships* — the Makefile/doctor UX, the README as install
  documentation, `cargo xtask package` and the 8-crate publish chain, the `cargo install
  eo9` experience (bundled-seed first run, upgrade/downgrade), platform assumptions, and
  the missing-tool UX.

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

Idempotent, fast, safe to re-run. Two warts:

1. **Two overlapping summaries.** `make setup` prints its own four-line "Prerequisite
   summary" and then runs `doctor`, which prints a seven-line summary of the same things
   plus more (nightly, kernel target, node). Same information, two formats, back to back.
2. **`make setup` cannot fail on a missing tool.** The Makefile invokes doctor as
   `-@cargo xtask doctor` — the leading `-` tells make to *ignore* its exit status. So on
   a machine where doctor reports `MISSING wasm-tools` and exits 1, `make setup` still
   exits 0, and `make setup && make ci` style scripting loses its guard. (Verified in
   Phase 3: doctor exits 1 when wasm-tools is missing; make's `-` prefix converts that to
   "Error 1 (ignored)" and an overall exit of 0.)

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
- `make ci` — not run as a unit in this study (its components — build-guest, build, test —
  were exercised individually).

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

## Phase 3 — packaging, clean install, upgrades, stable toolchain, missing tools, platforms

### `cargo xtask package` — the publish pre-flight failed on master

```
$ cargo xtask package
xtask: [guest] cargo build --workspace --release --target wasm32-unknown-unknown
    Finished `release` profile [optimized] target(s) in 0.10s
xtask: error: the eo9-components bundle is stale: eo9-stub-fs-eofs (contents differ); run
`cargo xtask refresh-components` and commit the result
$ echo $?
1
```

**The publish chain was red on the master this study forked from.** Root cause (from git
history): the `area/03-bind-entrypoint` merge (`eebc10e`, ~3 days before this study)
changed `wit/rt/rt.wit`, which changes the built bytes of the `fs.eofs` guest component;
`cargo xtask refresh-components` was not re-run after the merge (the web-VM assets *were*
rebuilt — `8ed125c` — but the components bundle was missed). STATUS.md still said the
publish chain was "dry-runs green". The drift guard exists and works — it caught exactly
this — but it is not part of `cargo xtask ci`, so nothing forced anyone to notice.

The fix is the one command the error names. Applied and committed on this study branch
(`components: refresh the bundled fs.eofs for the bind entrypoint`):

```
$ cargo xtask refresh-components
xtask: refreshed crates/eo9-components/data: 50 components, 3404 KiB
$ git diff --stat
 crates/eo9-components/data/eo9-stub-fs-eofs.wasm | Bin 163000 -> 163065 bytes
```

### `cargo xtask package` — the green run

```
$ cargo xtask package            # 15.8 s after the fix
xtask: eo9-components bundle matches the built components (50 components)
xtask: [repo] cargo publish --dry-run --registry crates-io -p eo9-component
   Packaging eo9-component v0.1.0 (…)
    Packaged 19 files, 220.6KiB (56.9KiB compressed)
   Verifying eo9-component v0.1.0 (…)
   Uploading eo9-component v0.1.0 (…)
warning: aborting upload due to dry run
   [… same Packaging → Verifying → dry-run Uploading sequence for the other leaves:
      eo9-store          12 files,  88.5KiB ( 23.1KiB compressed)
      eo9-providers-unix 14 files, 111.5KiB ( 29.1KiB compressed)
      eo9-components     56 files,   3.3MiB (  1.1MiB compressed)   ← the wasm bundle
      eofs-core          19 files, 131.3KiB ( 33.9KiB compressed) …]
xtask: dry-run-verified leaf crates (target/package):
xtask:   eo9-component-0.1.0.crate  0 KiB
xtask:   eo9-store-0.1.0.crate  0 KiB
xtask:   eo9-providers-unix-0.1.0.crate  0 KiB
xtask:   eo9-components-0.1.0.crate  0 KiB
xtask:   eofs-core-0.1.0.crate  0 KiB
xtask: [repo] cargo package --list -p eo9-runtime
   [… file lists for eo9-runtime, eo9-embed, eo9 — the eo9 binary crate is source-only:
      build.rs, 13 src files, tests/cli.rs, no .wasm files …]
xtask: pre-flight complete. To publish, run (in this order, waiting for each crate
xtask: to be live on crates.io before the next):
xtask:   cargo publish --registry crates-io -p eo9-component
xtask:   cargo publish --registry crates-io -p eo9-store
xtask:   cargo publish --registry crates-io -p eo9-providers-unix
xtask:   cargo publish --registry crates-io -p eo9-components
xtask:   cargo publish --registry crates-io -p eofs-core
xtask:   cargo publish --registry crates-io -p eo9-runtime
xtask:   cargo publish --registry crates-io -p eo9-embed
xtask:   cargo publish --registry crates-io -p eo9
xtask: note: only the leaf crates are dry-run-verified here — cargo cannot verify
xtask: the dependent crates until their dependencies are live on crates.io, so
xtask: `cargo publish` performs that verification at publish time.
$ echo $?
0
```

Two observations on the pre-flight's own output:

1. **Every `.crate` size reports "0 KiB."** The summary looks for the files at
   `target/package/<name>-0.1.0.crate`, but current cargo writes dry-run artifacts to
   `target/package/tmp-crate/` (and `tmp-registry/`); the lookup fails and
   `.unwrap_or(0)` silently prints 0. The real files exist (verified):
   eo9-component 58 KB, eo9-store 24 KB, eo9-providers-unix 30 KB, eo9-components
   1,133,603 bytes (1.08 MiB, comfortably under crates.io's 10 MiB cap), eofs-core 35 KB.
2. The `eo9` crate itself **cannot be packaged at all today** — `cargo package --no-verify
   -p eo9` fails with "no matching package named `eo9-component` found" because its
   path+version dependencies are not live on any registry yet. The pre-flight's closing
   note is honest about this; the consequence is that the first true end-to-end test of
   the artifact users will install is the production publish itself.

### The clean-install simulation (what `cargo install eo9` from a registry gets)

`guest/target/components/` was moved aside entirely and the binary rebuilt — exactly the
registry build (the eo9 build.rs finds no components directory, embeds nothing, and the
binary falls back to the `eo9-components` bundle crate):

```
$ mv guest/target/components guest/target/components.aside
$ cargo install --path crates/eo9 --force [--root <clean prefix>]
    Finished `release` profile [optimized] target(s) in 6.82s
$ eo9 hello                                  # brand-new empty store
eo9: first run: seeded 50 bundled programs into the module store at …/store-clean
[1780288391.301471000] Hello, world.
success(greeted)
$ eo9 -c 'entropy.seeded --seed 43 $ rng --count 3'
13432527470776545160
11303639812522640203
7982107704362031207
ok: generated(3)
$ eo9 store ls | head -4
names (50):
  cat a95fe983…
  cp f5eb2886…
  cruncher 7e9202e2…
$ mv guest/target/components.aside guest/target/components      # restored
```

A registry user gets a fully working system — eosh, 50 programs (coreutils, examples,
providers), composition, the compile cache — with **no wasm toolchain, no nightly, no
wasm-tools**. The bundled-seed fallback genuinely works.

What that user does **not** get (verified):

- **A version flag** (see D1).
- **Any way to write a program.** The guest SDK (`eo9-guest`) is `publish = false` and not
  in the publish chain; the WIT definitions live only in the repo. A registry user can run
  and compose the 50 bundled programs but cannot author a component without cloning the
  repository and installing its nightly + wasm32 + wasm-tools toolchain. This scope
  decision is stated nowhere.
- Man pages, shell completions, an `eo9 new` scaffold (tracked in GAPS), or any
  README/docs on the crates.io page (see D11).

### The upgrade / downgrade test

Two binaries whose bundled sets differ by exactly one component (the pre-fix and post-fix
fs.eofs), sharing one store:

```
$ OLD hello                 # fresh store
eo9: first run: seeded 50 bundled programs into the module store at …/store-upgrade
$ OLD hello                 # again: silent, no re-seed
$ NEW hello                 # the upgrade
eo9: store: refreshed 1 bundled program(s) for this version of eo9
[…] Hello, world.
$ OLD hello                 # the downgrade
eo9: store: refreshed 1 bundled program(s) for this version of eo9
[…] Hello, world.
```

Upgrades work: exactly the changed component is refreshed, the refresh announces itself,
user-rebound names are never touched, objects are never deleted. But there is no version
*ordering* — only "different from mine" — so a **downgrade silently reverts** the
component, and two coexisting binaries sharing a store ping-pong the binding on every
alternation.

### `cargo +stable check -p eo9` — the no-nightly claim

```
$ cargo +stable check -p eo9         # stable rustc 1.94.1
    [… full dependency check …]
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 22.77s
$ echo $?
0
```

The host chain checks clean on stable. Notes: the claim ("The host chain checks clean on
stable Rust, so plain `cargo install eo9` needs no nightly") lives only in
plan/01-workspace.md D12 — the README never states it, and instead tells every reader to
install the pinned nightly. plan/01 D12 also still describes the publish chain as 7 crates
(eofs-core missing) with stale bundle-size numbers. No crate declares `rust-version`
(MSRV), so a too-old stable fails with rustc errors rather than a clear version message.

### The missing-tool UX

With `wasm-tools` removed from PATH. (Method note: these were run by invoking the xtask
binary directly, because going through the `cargo` rustup shim re-prepends `~/.cargo/bin`
to the child PATH and the "missing" tool — which physically exists there on this machine —
gets found again. On a genuinely fresh machine the two are identical.)

```
$ xtask doctor
  ok       rustup
  ok       pinned toolchain nightly-2026-05-25
  ok       wasm32-unknown-unknown target
  ok       aarch64-unknown-none target
  MISSING  wasm-tools — run `cargo install --locked wasm-tools` (or `make setup`)
  ok       QEMU emulator version 11.0.0
  ok       node v25.2.1

xtask: error: doctor: missing required tools: wasm-tools — run `make setup` and re-check
$ echo $?
1

$ xtask build-guest
xtask: [guest] cargo build --workspace --release --target wasm32-unknown-unknown
    Finished `release` profile [optimized] target(s) in 0.08s
xtask: [guest] wasm-tools component new …/eo9_example_hello.wasm -o …
xtask: error: `wasm-tools` not found — run `make setup` (or `cargo xtask doctor`) to
install the host tools this command needs
$ echo $?
1
```

Both are exactly what they should be: correct exit codes, the error names the tool and the
exact fix, no raw `os error 2`. This is the friendly-error work from plan/01 D11 holding up.

`make setup` on the same broken machine was **not** run live, because its recipe would have
performed a real, unpinned `cargo install --locked wasm-tools` into the host toolchain
(potentially replacing the machine's pinned 1.250.0 with the newest release — see D15).
Instead the make semantics were demonstrated in isolation: a recipe line prefixed with `-`
prints `Error 1 (ignored)` and the make run exits 0. Combined with doctor's verified exit 1,
this confirms D3: `make setup` exits 0 on a machine missing required tools.

### Platform assumptions (read from the code, not guessed)

- `crates/eo9/src/interactive.rs` uses raw libc termios (`tcgetattr`/`tcsetattr`)
  unconditionally; `crates/eo9/src/mkfs.rs` and the fs/disk providers use
  `std::os::unix::fs::*` unconditionally → **Windows does not compile.**
- The fs provider has dedicated zero-copy snapshot paths for macOS (`fclonefileat`) and
  Linux (`FICLONE` ioctl) with a clean `cfg(not(any(macos, linux)))` fallback → macOS and
  Linux are first-class; other unixes should compile.
- There is **no `compile_error!` gate** for unsupported platforms, **no platform statement
  in the README** (zero occurrences of "macOS", "Linux", "Windows", "platform", or
  "supported"), and no `rust-version`/platform metadata in any crate. A Windows user pays
  several minutes of wasmtime compilation before hitting a wall of `std::os::unix` errors.
- **There is no CI configuration in the repository at all** — no GitHub Actions, nothing.
  The merge gate is `cargo xtask ci`, run locally by developers. Every verification of
  this project so far has happened on Apple Silicon macOS machines.

### crates.io metadata audit (all 8 publishable crates)

Every publishable crate has `description`, `repository`, and `license` — and nothing else.
No `readme`, no `keywords`, no `categories`, no `homepage`, no `documentation`, no
`rust-version`, on any of them; no crate ships a README file. The crates.io page for `eo9`
will be a one-line description and a GitHub link, for a project that has an excellent
README and a website.

## The participant's reactions — round 1 (after Phases 1–2)

Condensed; quotes are theirs.

**What they called genuinely good:**

- "**The README doesn't lie.** … In ten years of packaging other people's tools I can
  count on one hand the projects where every README example survives contact with a fresh
  checkout. This is the single most valuable property a project can have going into a
  release, because it means we can turn the README into a golden test suite."
- stdout/stderr discipline "verified, not asserted"; "most CLIs get this wrong for years."
- Meaningful exit codes and refusal messages that "name the exact flag that fixes it."
- The identical usermode/bare-metal digest: "not just a cool demo — it's a free
  cross-platform conformance test for the release pipeline. I'd weaponize that."
- `make` defaulting to help; idempotent setup; the repo-local store for `make shell`
  ("somebody thought about state isolation").

**What worried them, in their order:**

1. "**The prerequisite checker has prerequisites.** … A doctor's whole job is to work on a
   broken machine. … As written, doctor can never tell you the most common problem —
   'your Rust toolchain isn't set up' — because it needs one to exist."
2. "**`make setup` cannot fail.** … the one command whose entire purpose is 'tell me if my
   machine is ready' lies to scripts and CI. This is a one-character fix and I'd block a
   release on it."
3. The unpinned `cargo install --locked wasm-tools` vs the pinned 1.250.x family: "a time
   bomb … warning + unpinned docs is the worst combination."
4. The bundled-components question (are the 50 blobs in the published artifact, or does a
   registry build need the wasm toolchain?) — "this is the question for shippability."
5. The nightly pin vs `cargo install` from a registry — "if the CLI needs nightly,
   `cargo install eo9` from crates.io fails for every stable user, full stop."
6. The self-contradictions (env misprediction, `success(…)` vs `ok: …`, duplicate help
   entry): "individually cosmetic, collectively it tells me nobody is dogfooding the
   introspection surface."

**Their verdict on the layer split:** "the runtime UX is above average … The *installation*
layer is below average."

They then asked for seven specific things — the publish artifact contents and sizes, the
no-build-guest install, `--version`/`--help`, the manifest + a stable build, the store
upgrade test, the missing-tool failure modes live, and the platform/CI matrix — all of
which were run and shown in Phase 3 above (Phase 3 was largely shaped by their asks).

## The participant's verdict — round 2 (after Phase 3)

Their opening line: "This team built a product that's better than its packaging, and the
packaging gap is almost entirely 'guards exist but gates don't.' Close that, and this is a
release I'd put my name on."

### The ship/no-ship call

"**No-ship this week. Yes-ship in roughly two weeks if the blockers get done.** … Publishing
to crates.io is the one semi-irreversible act this project will take: names are owned
forever, 0.1.0 can never be re-uploaded, and the first impression is permanent. You don't
take that step with zero CI three days after master was silently publish-broken."

**Their blockers** (would refuse to publish until these exist):

1. **Hosted CI** on at least linux-x86_64 + macOS, running `cargo xtask ci` *plus the
   components drift check* plus `cargo +stable check -p eo9`. "A guard that isn't a gate is
   a suggestion."
2. **Evidence this works on Linux at all.** "Nothing in two sessions proves anyone ever
   compiled or ran this on Linux. `cargo install eo9` will be run on Linux more than macOS
   within a week of publishing."
3. **`eo9 --version`.** "A registry binary that cannot identify itself is unsupportable …
   Worse, `--version` currently falls through to store-name resolution, which means a store
   program could shadow it. … This is an afternoon."
4. **crates.io metadata + a platform statement** (readme, keywords, rust-version, and a
   `compile_error!` gate for non-unix). "Cumulatively a day of work; permanent first
   impression."
5. **A crate-name review.** "Names are the only truly irreversible decision here.
   `eofs-core` isn't even in the `eo9-` namespace."
6. **A publish rehearsal against a local registry** (e.g. kellnr), then `cargo install eo9
   --registry <local>` on a clean machine, because "the current plan's first true
   end-to-end test of the product is the production publish."

**Their should-fix list:** un-break `make setup` (remove the `-`); pin wasm-tools in every
install instruction or make doctor hard-fail on mismatch; fix the 0 KiB pre-flight bug
("release tooling that prints nonsense numbers trains operators to ignore the summary");
record the seeding version and warn on downgrade; **document the authoring limitation**
("unstated, it's a bait-and-switch and it will dominate your issue tracker"); git tag + a
10-line changelog at publish time.

**Fine to defer:** Homebrew/prebuilt binaries, shell completions, man pages, XDG
compliance, publishing the guest SDK, rewriting doctor as a script, the cosmetic
inconsistencies.

### Their top 3 pain points for an operator

1. "**The human is the CI.** No hosted CI, an 8-step manual publish with wait-for-live
   polling between steps, and the one drift guard that matters not wired into the merge
   gate. … The fs.eofs incident is the proof, not a hypothetical."
2. "**No version identity anywhere.** Binary can't say what it is; 8 crates move in
   lockstep at 0.1.0 with no bump policy; the store refresh has no ordering. … 'what is the
   user actually running' becomes unanswerable, and that cost lands entirely on the
   maintainer."
3. "**Undefined platform support.** Works-on-this-Mac, Linux plausible but unproven,
   Windows a slow-motion failure."

### Their top 3 things other projects should copy

1. **README-as-contract** — "every example reproduces byte-for-byte … formalize it — run
   the README table in CI as golden tests."
2. **The bundled-components architecture** — "source-only binary crate + data-only blob
   crate as a hard dep + build.rs that prefers fresh repo-built components and falls back
   to the bundle … the best 'ship a runtime plus its payload through a source registry'
   design I've seen," including the seeder-aware store refresh.
3. **Errors that contain their own fix, with exit codes that mean something** — and "the
   fact that a release pre-flight *exists* and caught a real shipping bug. The team's
   instinct to build guards is right; they just need to be promoted from tools into gates."

### Their release checklist (condensed)

- **Phase 0 (half a day):** crate-name review; write the platform statement and the
  authoring-scope statement.
- **Phase 1 (2–3 days):** GitHub Actions (ubuntu + macos: xtask ci, drift check, stable
  check, package --list); verify on Linux end to end; un-break `make setup`; pin
  wasm-tools; fix the pre-flight size lookup.
- **Phase 2 (2–3 days):** `--version`/`-V` reserved ahead of store resolution;
  `compile_error!` gate + README platform note; full crates.io metadata + per-crate
  READMEs; store seeding records the version and warns on downgrade.
- **Phase 3 (1 day):** scripted publish rehearsal against a local registry (turn the
  printed runbook into `xtask publish` with wait-for-live polling and a recovery plan for
  "crate N fails after N−1 are live"); `cargo install` from the local registry on clean
  macOS and Linux; run the README table against the installed binary.
- **Phase 4:** tag, changelog, publish, immediately `cargo install eo9` from crates.io on
  both platforms and run the README table one final time; GitHub Release; README gets an
  "install from crates.io" section "that doesn't mention nightly or wasm-tools at all."

### What they don't believe yet / want re-tested

1. **Linux, period.** "Strongest disbelief."
2. **"Stable suffices."** "That was `cargo +stable check`, in-repo. Check doesn't link,
   doesn't run tests, and in-repo isn't the registry build."
3. **Bundle-check reproducibility across machines.** "The moment the check runs on Linux CI
   against a Mac-committed bundle, any path- or host-dependent bytes in the wasm output
   produce a permanent false positive that will tempt someone to remove the check. Test
   exactly this when you stand up CI."
4. **The `--help` accuracy claim** — "we've already found two places where the tool's
   self-description disagrees with its behavior."
5. **eo9-components growth headroom** — 1.08 MiB today vs the 10 MiB cap; "put a size
   assertion in the pre-flight while someone still remembers why."

## Findings

### Verified during the session

- **D1 — `eo9` has no version flag.** `--version` → `unknown command`, exit 3; `-V` → same;
  `eo9 version` is interpreted as a *store program name* (`name version does not resolve in
  profile "default"`). The binary embeds the wasmtime version and target triple at build
  time and surfaces neither. Participant blocker #3; also a shadowing hazard (a flag that
  falls through to name resolution).
- **D2 — the prerequisite checker needs the prerequisites.** `cargo xtask doctor` requires
  the nightly toolchain plus a ~200-crate compile (wasmtime, cranelift, zstd-sys) before it
  can probe for tools. 23 s warm; minutes + GBs on a fresh machine.
- **D3 — `make setup` ignores doctor's exit code** (`-@cargo xtask doctor`), so a machine
  missing required tools still gets `make setup` → exit 0.
- **D4 — duplicate prerequisite summaries**: `make setup` prints its own summary and then
  doctor's, different formats, different coverage.
- **D5 — `cargo xtask help` lists `check-web-vm` twice** with different descriptions.
- **D6 — kernel-build warnings** scroll by during a README-following `cargo xtask qemu`
  (already tracked in GAPS as a known nit).
- **D7 — `env <program>` mispredicts a refusal**: `eo9 -c "env readwrite"` (a README
  example) says `eo9:rt/diagnostics … would be refused at spawn`, but the spawn succeeds.
- **D8 — outcome rendering still differs by surface**: `success(greeted)` from direct run
  vs `ok: greeted` in the shell (carried over from study 01).
- **D9 — the publish pre-flight was red on master.** `cargo xtask package` failed: the
  committed eo9-components bundle was stale for `fs.eofs` (the bind-entrypoint WIT change,
  merged 3 days prior, was never followed by `refresh-components`). STATUS.md still said
  "dry-runs green". Fixed on this branch (`a7eb541`). The structural gap remains: the
  drift check is not part of `cargo xtask ci`, so nothing prevents a recurrence.
- **D10 — the pre-flight's size summary prints "0 KiB" for every crate.** It looks in
  `target/package/`, but current cargo writes dry-run crates to `target/package/tmp-crate/`;
  the metadata lookup failure is swallowed by `.unwrap_or(0)`.
- **D11 — crates.io metadata is minimal on all 8 crates**: no readme, keywords, categories,
  homepage, documentation, or rust-version anywhere; no per-crate README files.
- **D12 — no platform statement, no platform gate.** Unix-only by construction (termios,
  `std::os::unix`); macOS/Linux first-class; Windows = a wall of compile errors after
  minutes of building wasmtime; the README contains no platform words at all.
- **D13 — a registry user cannot author programs, and nothing says so.** `eo9-guest` is
  publish=false, the WIT definitions are repo-only; `cargo install eo9` is a run-and-compose
  appliance. A legitimate 0.1 scope decision that is stated nowhere.
- **D14 — no MSRV (`rust-version`) declared** in any publishable crate.
- **D15 — `make setup` auto-installs wasm-tools unpinned and unprompted.**
  `cargo install --locked wasm-tools` (Makefile, README, doctor hint) installs the latest
  release while the repo pins the 1.250.x family; doctor only warns on mismatch. On a
  machine where wasm-tools exists but is off PATH, setup would also silently *replace* the
  user's installed version.
- **D16 — store downgrade silently reverts bundled programs.** The seed refresh has no
  version ordering, only "different from mine"; alternating binaries ping-pong the binding.
- **D17 — there is no hosted CI** (no .github/workflows, nothing) and therefore no platform
  matrix; everything is verified on developers' Macs.
- **D18 — the publish process is fully manual**: 8 ordered `cargo publish` commands with
  wait-for-live polling between them; no tags, no changelog, no scripted recovery if crate
  N fails after N−1 are live, no binary releases.
- **D19 — `cargo xtask package` cannot verify the `eo9` crate itself** (cargo limitation:
  dependencies must be live first). The first end-to-end test of the user-facing artifact
  is the production publish — unless a local-registry rehearsal is added.
- **D20 — `eo9 store --help` / `eo9 run --help` error** instead of printing help (already
  tracked in GAPS; re-verified).
- **D21 — plan/01 D12 is stale**: lists 7 publish crates (eofs-core missing) and the old
  bundle sizes; STATUS.md's "dry-runs green" was false at study time (true again after
  `a7eb541`).
- **D22 — stable-Rust support is real but unadvertised**: `cargo +stable check -p eo9`
  passes (rustc 1.94.1), yet the claim lives only in plan/01; the README routes everyone
  through the nightly pin.

### What landed well

- Every README example runs as written — **the top finding of study 01 ("docs that
  overclaim") is fixed.** Deny-by-default refusals, stderr outcomes, package-level `only`,
  determinism examples: all real.
- The bundled-components architecture: registry users get a fully working system with no
  wasm toolchain; repo developers always run fresh components; the store seeder tracks
  ownership, refreshes exactly what changed, announces itself, and never deletes user data.
- `make` defaults to help; `make setup` is idempotent and ~2 s when healthy; `make shell`
  uses a repo-local store.
- Doctor's *output* (when it runs) and the missing-tool errors are exemplary — they name
  the tool and the exact fix command, with correct exit codes.
- The publish pre-flight exists, is honest about what it cannot verify, prints an exact
  runbook — and its drift guard caught a real shipping bug during this study.
- `cargo +stable check -p eo9` passes; the host chain genuinely needs no nightly.
- The usermode/bare-metal digest identity, reproduced from a cold build in under a minute.

## Triage table

| # | Finding | Disposition | Notes |
|---|---|---|---|
| D9 | Publish pre-flight red on master (stale fs.eofs bundle) | **fixed in this study** (`a7eb541`) | one-file refresh; merge this branch or re-run `cargo xtask refresh-components` on master |
| D9b | Drift check not part of any gate → recurrence guaranteed | **needs-owner-decision** | add `check-components-bundle` (cheap: build-guest + byte-compare) to `cargo xtask ci`, or to a pre-publish checklist only |
| D1 | No `eo9 --version` / `-V`; falls through to store-name resolution | **fix-now** (dispatch) | parse `--version` before name resolution; print version + embedded wasmtime/target. Participant blocker |
| D3 | `make setup` ignores doctor failure (`-@`) | **fix-now** (dispatch) | one-character fix (drop the `-`); decide whether setup should fail or just propagate |
| D5 | `cargo xtask help` lists check-web-vm twice | **fix-now** (dispatch) | delete the stale block in print_help() |
| D10 | Pre-flight prints 0 KiB for every .crate | **fix-now** (dispatch) | look in `tmp-crate/`; treat a missing file as an error, not 0 |
| D15 | wasm-tools install unpinned vs the 1.250 pin; auto-install unprompted | **fix-now** (dispatch) | add `--version` constraint to the install command in Makefile/README/doctor hint, or make doctor hard-fail on family mismatch |
| D11/D14 | No crates.io metadata (readme/keywords/categories/homepage/docs/rust-version) | **fix-now** (dispatch) | metadata-only change across 8 Cargo.tomls + a readme per crate (eo9 reuses the repo README) |
| D12 | No platform statement / no non-unix compile gate | **fix-now** (dispatch) | README "Supported platforms" section + `compile_error!` in eo9/eo9-providers-unix |
| D13 | Registry users can't author programs; undocumented | **tracked** + owner | document the scope in README/crate description now; publishing the guest SDK is a separate owner decision |
| D21 | plan/01 D12 stale (7 crates, old sizes); STATUS "dry-runs green" was false | **fix-now** (docs) | planner-owned docs; correct alongside the GAPS update for this study |
| D2 | Doctor requires compiling wasmtime first | **needs-owner-decision** | options: (a) accept and document, (b) make doctor a dependency-light xtask feature/bin, (c) shell-script doctor. Participant: defer is acceptable |
| D4 | Duplicate setup/doctor summaries | **tracked** | fold the Makefile summary into doctor (or vice versa) when D2 is decided |
| D7 | `env readwrite` predicts a refusal that doesn't happen | **tracked** | runtime/inspection bug, outside distribution scope; affects a README example |
| D8 | `success(…)` vs `ok: …` rendering split | **tracked** (pre-existing, study 01) | still present |
| D16 | Downgrade silently reverts bundled programs | **tracked** | record seeder version; warn on downgrade. Participant: defer with a known-issues note |
| D17 | No hosted CI / platform matrix; Linux unproven | **needs-owner-decision** | participant blocker #1/#2; requires a GitHub Actions (or equivalent) decision from the owner |
| D18 | Manual 8-step publish; no tags/changelog/recovery script | **needs-owner-decision** | `xtask publish` automation is a meaningful work item; owner sets release policy |
| D19 | The eo9 crate can't be end-to-end tested before real publish | **needs-owner-decision** | local-registry rehearsal (participant blocker #6) vs accepting the risk |
| D23 | eo9-components growth headroom unmonitored (1.08 MiB / 10 MiB cap) | **tracked** | add a size assertion to the pre-flight |
| D24 | Crate names not reviewed for permanence (`eofs-core` outside the eo9- namespace) | **needs-owner-decision** | GAPS already parks "crates.io name"; participant calls it the only irreversible decision |
| D6 | Kernel build warnings during README flow | already tracked in GAPS | no action here |
| D20 | `store --help`/`run --help` error | already tracked in GAPS | re-verified, still present |
| D25 | `~/.eo9` not XDG-compliant | **tracked** (defer) | participant: fine to defer |

## Facilitator observations

- The packaging layer is where this project's own discipline (guards, pre-flights,
  drift checks) exists but is not yet self-enforcing. Both real bugs found this session
  (D9, D10) were in the *release tooling itself*, and one of them was the release tooling
  failing to notice that its own earlier output ("dry-runs green") had gone stale.
- The study's demo plan was deliberately not destructive to the host machine: the
  README's `cargo install --path crates/eo9 --force` was redirected to worktree-local
  roots, and the `make setup`-with-broken-PATH test was replaced by an equivalent
  non-destructive demonstration after realizing the recipe would have performed a real,
  unpinned `cargo install` of wasm-tools into the host toolchain. That realization is
  itself finding D15.
- The rustup shim's PATH re-injection (missing-tool simulations silently "heal" when run
  through `cargo`) is worth remembering for future studies: missing-tool UX must be tested
  by invoking the xtask binary directly, or on a machine where the tool genuinely does not
  exist.
- Three of the participant's asks could not be satisfied on this machine and remain open:
  a Linux run of anything, a true registry-install of the eo9 .crate (impossible until the
  dependency crates are live or a local registry is stood up), and a from-scratch cold-cache
  timing of the first build. These line up with their "what I don't believe yet" list.
- Builds in the fresh worktree, for the record: xtask debug 23 s; build-guest 11.4 s;
  eo9 release (cargo install) 1 m 21 s; kernel aarch64 release 36.8 s; full
  `cargo xtask package` 15.8 s after the bundle fix. All with a warm cargo registry cache.
