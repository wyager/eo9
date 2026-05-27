# Eo9

**A capability-secure operating system where capabilities *are* components and granting authority *is* an algebra.** Built on the WebAssembly Component Model, in Rust.

---

- **A program's imports are its permissions.** There is no ambient authority — no implicit filesystem, clock, RNG, or network. A program can do exactly what you *composed into* it, and nothing else.
- **Granting and revoking authority is an algebra, decided before the program runs.** `provider $ program` grants a capability; `only <interfaces> $ program` restricts to a set; missing capabilities are *sealed* so nothing outside can re-grant them. The decision is visible in the artifact, not buried in runtime config.
- **Deny by default, all the way down.** Withholding a capability isn't a flag — it's simply not composing it in. Ask for less than a program needs and it is refused *before* it starts, not midway through.
- **Everything is a component** — every program, provider, shell, and driver is a wasm component. The OS is the algebra over them plus a compiler; the only privileged residue is the compiler itself.
- **Deterministic by construction.** Compose `time.frozen`, `entropy.seeded`, `fs.memfs` and a run is byte-identical and sealed against the ambient world. The deterministic-test story goes all the way down.
- **The same components run in userspace and on bare metal.** `eo9` gives you a capability-secure "VM" on your host OS; the *same* components boot on bare aarch64 (QEMU), where the kernel is no_std and **carries its own Cranelift compiler** — it composes and compiles programs on the machine itself.

> Design doc: [`SPEC.md`](SPEC.md). Current status: [`STATUS.md`](STATUS.md). Known gaps: [`GAPS.md`](GAPS.md).

---

## Userspace mode

```sh
cargo install --path crates/eo9      # puts `eo9` on your PATH
eo9                                  # boot the eosh shell; the first run seeds the
                                     # store with the bundled programs, then drops
                                     # you at an `eosh>` prompt
```

Run a program directly (typed, WAVE-encoded args; three-way outcome printed):

```sh
eo9 hello --name world --excited true
#> [..] Hello, world!
#> success(greeted)

eo9 cruncher --seed 9 --rounds 200000
#> success(digest(14341732361190694547))
```

**Capability lockdown — deny by default.** `readwrite` needs a filesystem; it gets none unless you grant one, and a grant is rooted and inescapable:

```sh
eo9 readwrite --path note.txt --contents hi
#> eo9: readwrite requires the eo9:fs filesystem capability, which eo9 does not grant
#>      by default: pass `--fs-root <dir>` to give the program access to a host directory

eo9 --fs-root ./sandbox readwrite --path note.txt --contents hi
#> success(round-tripped(2))          # confined to ./sandbox; guest paths cannot escape it
```

**Capability lockdown — the algebra, in the shell** (`eo9 -c "<line>"` runs one command and exits):

```sh
# Grant exactly the two capabilities hello needs — text + time — and seal everything else:
eo9 -c "only eo9:text,eo9:time $ hello --name boxed --excited true"
#> ok: greeted

# Drop one it needs, and it is refused before it ever runs:
eo9 -c "only eo9:text $ hello --name boxed --excited true"
#> error: only — required imports outside the allow-list: eo9:time/time@0.1.0

# Compose a sealed, deterministic RNG onto a program — same answer every time:
eo9 -c "entropy.seeded $ cruncher --seed 9 --rounds 200000"
#> ok: digest(14341732361190694547)
```

Inspect before you run — `describe` shows a program's imports/args; `env` shows what the
session grants and marks each of a program's imports satisfied / optional-absent / refused:

```sh
eo9 -c "describe readwrite"
eo9 -c "env readwrite"
```

## Bare-metal mode (aarch64 / QEMU)

Prerequisites: a nightly Rust toolchain and `qemu-system-aarch64`.

```sh
cargo xtask build-kernel aarch64
cargo xtask qemu aarch64             # boots Eo9 on the QEMU `virt` machine straight
                                     # into an interactive eosh prompt over serial
```

At the on-metal `eosh>` prompt, the **same** commands work — and composition is fused by the
real algebra and compiled to native code **on the machine** by the kernel's own Cranelift:

```
eosh> hello --name metal --excited true
[..] Hello, metal!
ok: greeted

eosh> entropy.seeded $ cruncher --seed 9 --rounds 200000
ok: digest(14341732361190694547)     # composed + compiled on-target; no prebuilt artifact

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
