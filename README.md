# Eo9

**A capability-secure operating system built on the WebAssembly Component Model, in Rust.**

---

- **A program's imports are its permissions.** There is no ambient authority — no implicit filesystem, clock, RNG, or network. A program can do exactly what you *composed into* it, and nothing else.
- **Granting and revoking authority is an algebra, decided before the program runs.** `provider $ program` grants a capability; `only <interfaces> $ program` restricts to a set; missing capabilities are *sealed* so nothing outside can re-grant them. The decision is visible in the artifact, not buried in runtime config.
- **Deny by default, all the way down.** Withholding a capability isn't a flag — it's simply not composing it in. Ask for less than a program needs and it is refused *before* it starts, not midway through.
- **Everything is a component** — every program, provider, shell, and driver is a wasm component. The OS is the algebra over them plus a compiler; the only privileged residue is the compiler itself.
- **Deterministic by construction.** Compose a frozen clock and a seeded RNG onto a program and the run is reproducible by definition — the program cannot observe anything you didn't hand it.
- **The same components run in userspace and on bare metal.** `eo9` gives you a capability-secure "VM" on your host OS; the *same* components boot on bare aarch64 (QEMU), where the kernel is no_std and **carries its own Cranelift compiler** — it composes and compiles programs on the machine itself.

> Design doc: [`SPEC.md`](SPEC.md). Current status: [`STATUS.md`](STATUS.md). Known gaps: [`GAPS.md`](GAPS.md).

---

## Quick start

| Command | What it does |
|---|---|
| `make setup` | One-time: install the prerequisites (Rust target, `wasm-tools`; checks for QEMU) |
| `make shell` | Build the components and drop into the `eosh>` shell on your host |
| `make www` | Serve the website + the in-browser demos at <http://127.0.0.1:8080/> |
| `make qemu` | Boot the bare-metal kernel in QEMU to an `eosh>` prompt |
| `make ci` | Run the full local gate (host + guest + kernel workspaces) |

**Prerequisites** (`make setup` does all of it): a Rust toolchain via [rustup](https://rustup.rs)
(the pinned nightly and per-workspace targets install themselves on first build), the
`wasm32-unknown-unknown` target, and the `wasm-tools` CLI (`cargo install --locked wasm-tools`).
`qemu-system-aarch64` is only needed for the bare-metal demo.

## Userspace mode

```sh
cargo xtask build-guest                       # build the wasm components (the binary embeds them)
cargo install --path crates/eo9 --force       # then install `eo9` onto your PATH
eo9                                           # first run seeds the store with the bundled
                                              # programs and drops you at an `eosh>` prompt
```

Run a program directly — bare names resolve from the store, args are typed, the outcome is
reported on **stderr** so pipes stay clean:

```sh
eo9 hello --name world --excited true
#> [..] Hello, world!                         # program output (stdout)
#> success(greeted)                           # outcome (stderr); exit code 0/1/2/3

eo9 cruncher --seed 9 --rounds 200000
#> success(digest(14341732361190694547))

eo9 echo --text "hello pipes" | tr a-z A-Z
#> HELLO PIPES                                # only the program's bytes go through the pipe
```

**Deny by default.** Filesystem access exists only if you grant it, and a grant is a rooted
jail the program cannot escape:

```sh
eo9 cat --path notes.txt
#> eo9: error: cat […] requires the eo9:fs filesystem capability, which eo9 does not grant
#>      by default: pass `--fs-root <dir>` …            (refused before it runs; exit 3)

eo9 --fs-root ./sandbox cat --path notes.txt
#> capability systems are neat
#> success(printed(28))

eo9 --fs-root ./sandbox readwrite --path note.txt --contents hi
#> success(round-tripped(2))
```

**The algebra, in the shell** (`eo9 -c "<line>"` runs one command and exits):

```sh
# Grant exactly the two capabilities hello needs — text + time — and seal everything else:
eo9 -c 'only eo9:text/text,eo9:time/time $ hello --name boxed --excited true'
#> [..] Hello, boxed!
#> ok: greeted

# Drop one it needs, and it is refused before it ever runs:
eo9 -c 'only eo9:text/text $ hello --name boxed --excited true'
#> error: `only` refused: the program still requires eo9:time/time@0.1.0, which the
#>        allow-list does not include (allow it, compose a provider for it, or drop the requirement)

# Substitute reality: a seeded RNG makes `rng` deterministic, a frozen clock pins the time —
# the program is unchanged, only its world is:
eo9 -c 'entropy.seeded --seed 43 $ rng --count 3'
#> 13432527470776545160
#> 11303639812522640203
#> 7982107704362031207                        # same three numbers on every run

eo9 -c 'time.frozen --now-seconds 1700000000 --monotonic-ns 0 $ hello --name frozen --excited true'
#> [1700000000.000000000] Hello, frozen!
```

Inspect before you run — `describe` shows a program's imports and args, `env <program>` shows
how this session would treat each import (satisfied / no-authority / refused):

```sh
eo9 -c "describe readwrite"
eo9 -c "env readwrite"
```

In the interactive shell, programs see their world at `/` (your `--fs-root`, writable) with the
bundled programs read-only at `/bin` — and `eosh` is just another program: type `eosh` at the
prompt and the nested shell can run, compose, and recurse with the same authority, or less:

```
eosh> eosh
eosh> hello --name nested --excited true
[..] Hello, nested!
ok: greeted
```

Runaway compute is a flag away from being bounded: `eo9 --max-fuel 100000 cruncher --rounds 200000000`
ends in `abnormal(killed)` instead of a hot loop.

## Bare-metal mode (aarch64 / QEMU)

Prerequisites: a nightly Rust toolchain and `qemu-system-aarch64`.

```sh
cargo xtask build-kernel aarch64
cargo xtask qemu aarch64             # boots Eo9 on the QEMU `virt` machine straight
                                     # into an interactive eosh prompt over serial
```

At the on-metal `eosh>` prompt the **same** commands work — and a composition is fused by the
real algebra and compiled to native code **on the machine** by the kernel's own Cranelift:

```
eosh> hello --name metal --excited true
[..] Hello, metal!
ok: greeted

eosh> time.frozen --now-seconds 1700000000 --monotonic-ns 0 $ hello --name metal --excited true
[1700000000.000000000] Hello, metal!     # fused + compiled on-target; no prebuilt artifact
ok: greeted

eosh> exit
```

Children spawned from the metal shell receive only text/time/entropy — never the filesystem
or the right to spawn. Capability containment is the same on metal as in userspace.

Headless (self-terminating) variants for scripting:

```sh
cargo xtask qemu aarch64 program=cruncher seed=9 rounds=200000
cargo xtask qemu aarch64 demo        # the boot demo: async guests + on-target codegen
```

---

Run the full local gate (host + guest + kernel workspaces) with `cargo xtask ci`.
Licensed under [MIT](LICENSE).
