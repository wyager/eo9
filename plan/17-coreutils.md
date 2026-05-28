# 17 — Coreutils (basic tool suite)

## Scope

A suite of small, capability-honest coreutils shipped as ordinary Eo9 guest programs under
`guest/coreutils/*`, built and seeded like the examples so a freshly installed `eo9` offers
them by bare name. Not full Unix compatibility — basic, correct forms that double as
capability/lockdown demos (each imports only what it needs).

Tools and their capabilities (imports):

| Tool  | Args                         | Imports                | Outcome |
|-------|------------------------------|------------------------|---------|
| cat   | `--path`                     | eo9:fs, eo9:text       | `printed(bytes)` |
| ls    | `--path`                     | eo9:fs, eo9:text       | `listed(count)` |
| find  | `--path --name` (`""`=all)   | eo9:fs, eo9:text       | `found(count)` |
| wc    | `--path`                     | eo9:fs, eo9:text       | prints `<lines> <words> <bytes>`, `counted` |
| head  | `--path --lines`             | eo9:fs, eo9:text       | `printed(lines)` |
| stat  | `--path`                     | eo9:fs, eo9:text       | prints `<kind> <size> bytes`, `described` |
| mkdir | `--path`                     | eo9:fs                 | `created` |
| rm    | `--path`                     | eo9:fs                 | `removed` |
| cp    | `--src --dst`                | eo9:fs                 | `copied(bytes)` |
| touch | `--path`                     | eo9:fs                 | `touched` |
| echo  | `--text`                     | eo9:text (no fs)       | `done` |
| rng   | `--count`                    | eo9:entropy, eo9:text  | `generated(count)` |

`rng` exists specifically to give a real entropy-consuming program: `entropy.seeded --seed N
$ rng --count K` is deterministic across runs (verified), unlike `cruncher` (which imports
nothing, so composing entropy onto it is a no-op).

## Wiring

- Each tool is its own crate `eo9-coreutil-<name>` under `guest/coreutils/<name>/`, mirroring
  the examples (own `wit/world.wit` with a `package eo9-coreutils:<name>`, `wit/deps/*`
  symlinks into the repo `wit/`, the `eo9_guest` SDK, named typed args, three-way
  `result<program-success, program-failure>` — failures are variants, never traps).
- `guest/Cargo.toml` members gained `"coreutils/*"`.
- `xtask` `GUEST_COMPONENTS` lists all twelve so `build-guest` componentizes them.
- `crates/eo9/src/seed.rs` `shell_name_for` maps the `eo9-coreutil-` prefix to the bare name
  (`eo9-coreutil-cat` → `cat`), so build.rs embeds them and first-run seeding binds them.
- Integration coverage in `crates/eo9/tests/cli.rs`: fs tools against an `--fs-root` sandbox
  (ls/cat/wc/cp), positional/variadic paths (`cat /a.txt /b.txt`, bare `ls`, `head --lines 1
  a b`, multi-path touch/rm/wc), `echo` with no fs, seeded-rng determinism, and an
  fs-capability refusal.

## Decisions

1. **All `main` args are required** — the runtime's WAVE arg binder (`wave::parse_args`)
   requires every parameter to be supplied (a missing param is an error, even for an
   `option<…>` type). So there are no optional flags: `find` takes a required `name` where
   `""` means "match all", and `head` takes a required `lines`. Optional/defaulted flags
   would need the binder to default an omitted `option<…>` param to `none` — a small runtime
   follow-up, recorded here. (The one carve-out is D6's variadic tail: a final `list<string>`
   parameter defaults to the empty list, which is how bare `ls` lists `/`.)
2. **`cat`/`wc`/`head`/`cp` size the read from `stat`** (one read of the file's reported
   size at offset 0), matching how `readwrite` does its single-read round-trip; no chunked
   read loop. Fine for the memfs and host-directory providers, which return the whole file in
   one read.
3. **`find` walks iteratively** (a `Vec<String>` worklist of directories) rather than via
   async recursion, which would require boxing the recursive future under no_std.
4. **`touch` opens with `CREATE | WRITE`** (no `TRUNCATE`) so it creates an absent file and
   leaves an existing one intact.
5. **Naming prefix `eo9-coreutil-`** keeps the tools categorized separately from
   `eo9-example-` while still mapping to clean bare shell names via `shell_name_for`.
6. **Path-taking tools take a variadic `paths: list<string>` tail** (owner request:
   `cat a.txt b.txt`). `cat`, `ls`, `wc`, `stat`, `rm`, and `touch` now declare
   `main(paths: list<string>)`; `head` declares `main(lines: u64, paths: list<string>)`.
   Positional command-line values fill parameters in declaration order, and a **final**
   `list<string>` parameter is the variadic tail collecting whatever positionals remain
   (binder details in plan/04 D-args and plan/10). Behavior over several paths follows the
   unix tools: `cat` concatenates in order; `ls` prints a `<path>:` header per group (and
   with no paths lists `/`); `wc` prints a per-file line suffixed with the path plus a
   `total` line; `head` prints `==> path <==` headers; `stat` prefixes each line with the
   path; `rm`/`touch` just iterate. Single-path output is byte-identical to the old
   single-`path` form. `cp` keeps `--src`/`--dst` (two distinguished roles, not a list);
   `find`/`mkdir` keep their single `path` (still usable positionally:
   `find /docs --name x`). The old `--path <p>` spelling is gone for the converted tools —
   the migration is `cat notes.txt` or `cat --paths notes.txt` (a single bare value for a
   `list<string>` flag is coerced to a one-element list). README's two `cat --path` examples
   need that one-word edit (planner follow-up; README not touched from this area), and the
   committed `/vm` store assets embed the old signatures until `build-web-vm` is re-run.

## Notes for the planner (out of this area's scope)

- `only` allow-list entries must be **full** interface names (`eo9:text/text`), not the short
  package form (`eo9:text`): `restrict` rejects `eo9:text` with
  `InvalidAllowList("… is not an interface name (expected namespace:package/interface)")`. The
  README's `only eo9:text,eo9:time $ …` examples need the `/interface` suffix
  (`only eo9:text/text,eo9:time/time $ …`). Verified: `only eo9:text/text $ rng` is correctly
  refused (rng needs `eo9:entropy/entropy`), and
  `only eo9:entropy/entropy,eo9:text/text $ entropy.seeded --seed 1 $ rng` runs.
- The `entropy.seeded` stub **traps if used without a seed** (`entropy.seeded $ rng` panics in
  `get-u64`); it must be configured (`entropy.seeded --seed N $ …`). A friendlier default or
  clean error would be a stub (area 09) improvement.
