# Eo9 Operating System Spec

Eo9 is an operating system built on modern language-theoretic principles for security, performance, and composability.

This is a living document: every concrete design choice below is provisional (see *Overall Guiding Principles*).

## Design

At its core, Eo9 is simply a set of APIs that programs can interact with.

Eo9 provides several things:
1. A standard library of APIs for common OS utilities (disk access, filesystems, memory allocation, etc.)
2. A compiler for WASM programs using the above APIs
3. Low-level implementations of standard APIs for various hardware devices (i.e. drivers)

## Security

Unlike every mainstream OS, Eo9 does not rely on hardware security features (like separate privilege domains).

Instead, Eo9 is secure-by-construction. Programs are distributed as WASM bytecode, not pre-compiled assembly,
so we can enforce security properties at the language level. 

The trusted computing base is correspondingly small and explicit: the compiler (the only thing that may generate
native code), the root scheduler, and the hardware-root capabilities held by the OS core. Everything else —
interpreters, user-level schedulers, providers, the shell — is unprivileged by construction (see *Execution APIs*).

Spectre-class side channels are mitigated capability-style: fine-grained time is itself a capability. Untrusted
programs are composed with noisy, adversarial, or stubbed timers (`time.fuzzy`, `time.frozen`, `time.none`) and are
not granted shared-memory threads (thread spawning is itself a capability — see *Execution APIs*) or other primitives from which a high-resolution clock could be rebuilt —
attenuating the attacker's clock is just provider substitution, the same mechanism as everything else.

## Virtualization

One of the strongest selling points of Eo9 is that it does not require any sort of special support to implement
secure and ultralightweight virtualization. Every Eo9 program (including the entire Eo9 userspace) can be
provided virtualized versions of standard OS utilities. This is zero-overhead, when possible; the virtualized
implementation will be compiled together with the invoked binary.

If you want to run an Eo9 program in a virtualized environment, you simply invoke the program with
standard devices replaced with swapped versions.

For example, if we wanted to run the program `browser` with virtualized networking and filesystem, we could simply run

```
> virtualfs --name fs0 --impl zfsfile=/tmp/fs.zfs $ virtualnet --name net0 --impl loopback-only $ browser
```

This would link the `browser` module with `virtualfs` and `virtualnet`-provided implementations of the relevant
APIs, instead of the standard OS-provided versions.

## Performance

Because each program is distributed as WASM bytecode, programs are compiled down to the native architecture
and (depending on compile settings), driver implementations are inlined into each program.

The consequences of this are:
1. There is no kernel-mode/user-mode boundary. There are no context switches, except for scheduler switches between programs.
2. There is no overhead from memory isolation. No MMU, no nested address translation.
3. There is no overhead from virtualization. Virtualized implementations that do nothing at all simply get compiled out.

One honest caveat on (2): without an MMU there are no guard-page tricks, so on MMU-less targets linear-memory safety
is paid for with explicit bounds checks — the overhead moves from address translation to whatever loads and stores
the optimizer cannot prove safe. On MMU-equipped hardware the compiler may still use guard pages, purely as an
optimization and never for privilege separation.

> TODO (scheduling): codegen inserts yield points (fuel checks — chosen over epoch deadlines for determinism), so native tasks are cooperatively
> schedulable *by construction* and a scheduler is just a program that holds task handles and decides which to
> resume (see *Execution APIs*). Still to specify: the root scheduler's handling of timer interrupts and the idle
> loop, the scheduling policy itself, and what a "scheduler switch" actually costs.

## Hardware Support

Eo9 supports any platform where we can compile WASM, including MMUless ARM and RISC-V.

## Eo9-as-program

Because Eo9 is fundamentally just a WASM compiler and some standard APIs, we can run Eo9 as a usermode program on
any OS (including Eo9 itself).


## The Details

### Eo9 API design

Eo9 OS APIs are designed around modern patterns that support a high degree of concurrency and asynchronicity.

Eo9 OS APIs are built around asynchronous operations — async-typed functions whose in-flight calls can be awaited individually or jointly. For example, the disk API looks like

```
fn read(dev : &DiskImpl, offset: u64, dst: Buffer) -> Async<(Buffer, Result<ReadResult, ReadError>)>
fn write(dev : &DiskImpl, offset: u64, src: Buffer) -> Async<(Buffer, Result<WriteResult, WriteError>)>
```

The `Buffer` is passed by ownership and returned to the caller on completion (on both success and error), so the backend holds it uniquely across the async boundary; see WASM runtime for how this lowers to WIT.

Implementations are designed to scale up to millions of concurrent read/write ops to handle the reality of modern high-IOPS SSDs/filesystems/RAID implementations.

### WASM runtime

Each Eo9 program is a WASM module (a Component Model component).

A WASM module's only channel to the outside world is its imports: core WASM has no syscalls and no ambient I/O, so a program can affect nothing it was not explicitly handed. The import set therefore *is* the capability set — which is what makes Eo9 secure-by-construction (see Security): everything a program can do is statically enumerable from its imports before it ever runs.

The WASM module imports the set of OS APIs it wants access to. Required OS APIs are imported directly; optional OS APIs are imported through the API's `-optional` flavor, so optionality is visible both in the import list and in the types (see *The capability algebra*). We use WIT (the Component Model's interface-definition language) for import/export specification; a WIT `world` is precisely the declaration of which APIs, by name and type, a program requires and provides.

Every Eo9 module is exactly one of two kinds. A **binary** exports a `main` entrypoint, which we invoke to run it; `main` returns a `result<program-success, program-failure>` whose success and failure types are defined by the program itself (typically variants), so a program reports its outcome in its own structured vocabulary rather than through a lossy numeric exit code. A **provider** instead exports OS-API interfaces (plus a `configure` entry) for composition into other modules, and is never run directly. A module is never both — see *Composition and the `$` operator*. Both entrypoints — `main` and `configure` — are declared `async`: in the Component Model's async ABI, whether an export may block is part of its type, and anything that performs I/O must therefore be async-lifted.

At load time, the OS scans the imports and ensures that, for each one, we know how to provide a resource of the specified name and type. Anything we cannot satisfy is rejected before execution.

Interfaces are defined by the Eo9 standard and versioned as semver-tagged WIT packages (e.g. `eo9:disk@1.0.0`), so a program pins the exact API contract it was built against. Link-time satisfaction is semver-compatible: a provider of `eo9:disk@1.2.0` satisfies an import of `eo9:disk@1.0.0` (same major, equal-or-newer minor/patch); different majors are simply different interfaces and never unify. For 0.x versions we follow the Cargo/Component-Model convention: `0.minor` is the compatibility track (`0.2.3` satisfies an import of `0.2.1` but not `0.3.0`), and `0.0.x` is exact. Shared types such as buffers live in their own package and are pulled in with `use`. For example,

```wit
// Shared I/O types, used across disk, net, text, …
package eo9:io@1.0.0 {
    interface buffers {
        /// A handle to a (possibly DMA-backed) block of memory. Transferred by
        /// `own`, so a backend takes exclusive possession for the life of an op.
        resource buffer {
            constructor(len: u64);
            len: func() -> u64;
            // ... plus read/write accessors for copying bytes between the buffer
            // and the program's own memory (full signatures live in wit/).
        }
    }
}

package eo9:disk@1.0.0 {
    // The root resource lives in a types-only interface so the `-optional` flavor and
    // stubs (e.g. `disk.none`) can `use` it without importing the authority-bearing
    // `disk` interface itself.
    interface types {
        resource disk-impl;
    }

    interface disk {
        use eo9:io/buffers@1.0.0.{buffer};
        use types.{disk-impl};

        /// The capability's root handle (see The capability algebra).
        default: func() -> disk-impl;

        record read-result  { bytes-read: u64 }
        record write-result { bytes-written: u64 }
        variant read-error  { not-found, io(string), out-of-range }
        variant write-error { io(string), out-of-range, read-only }

        /// own<buffer> in, own<buffer> back out (on both success and error).
        read:  async func(dev: borrow<disk-impl>, offset: u64, dst: own<buffer>)
            -> tuple<own<buffer>, result<read-result, read-error>>;
        write: async func(dev: borrow<disk-impl>, offset: u64, src: own<buffer>)
            -> tuple<own<buffer>, result<write-result, write-error>>;
    }
}

// A program targets a `world`: OS APIs arrive as compiled-in imports, while
// invocation config arrives as named, fully-typed arguments — no untyped argv.
package eo9:browser@0.1.0 {
    world browser {
        // implementations: resolved at link time, fused in
        import eo9:disk/disk@1.0.0;
        import eo9:net/net@1.0.0;

        // outcome types are defined by the program itself
        variant program-success {
            exited,
            restart-requested,
        }
        variant program-failure {
            bad-arguments(string),
            network-unreachable,
            internal-error(string),
        }

        // arguments: named and typed — one shell flag per parameter
        //   browser --url https://example.com --verbose true --max-connections 64
        export main: async func(
            url: string,
            verbose: bool,
            max-connections: u32
        ) -> result<program-success, program-failure>;
    }
}
```

**Arguments vs. imports.** A module's argument entry — `main` for a binary, `configure` for a provider — takes *named, fully-typed* arguments; there is no untyped `argv`. Each shell flag maps to one typed parameter (`--verbose true` ⇄ `verbose: bool`), so the runtime parses and type-checks an invocation against the signature. Because Component Model export parameters keep their names and types in the component's type, a launcher can extract that signature — via `wasm-tools component wit`, or the `wit-parser`/`wit-component` libraries — to validate arguments, generate a CLI parser, or auto-fill an invocation UI. This is the dual of imports: imports are *capabilities*, resolved at link time and fused in (see Performance); arguments are *invocation data* — a binary's `main` args bound at run time, a provider's `configure` args bound at compose time (see *Composition and the `$` operator*). Behavior always enters through imports — arguments carry data, never functions.

**Ownership and buffers.** WIT has no mutable/immutable data references — there is no `&`/`&mut`. Plain data (lists, records, …) is passed by value, and the only ownership concepts, `own<T>` and `borrow<T>`, apply solely to opaque `resource` handles.

For I/O buffers we use an **owned-buffer round-trip**: the caller transfers an `own<buffer>` to the backend and gets it back when the operation completes. (WIT itself spells this bare — `buffer` — since `own` is the default for resource params/results and a reserved word; the spec writes `own<buffer>` to emphasize the contract.) Because `own` is linear (consumed on transfer), the backend has manifestly unique ownership of the buffer for the whole duration of the async operation — no aliasing, and no reference whose lifetime must span an await point. A `borrow<T>`, by contrast, is valid only for the operation it was passed to and may not be retained beyond it; that suits the `fs-impl` handle (a reference to an OS-owned resource) but not a buffer the backend must take exclusive possession of and return. The buffer comes back on *both* the success and error paths — placed outside the `result` so a failed op never leaks it.

Modeling the buffer as a `resource` rather than a `list<u8>` also makes it DMA-friendly: it can be backed by host/driver-managed memory, so `own<buffer>` transfer maps directly onto "who may touch this I/O region right now," and the bytes never move.

**Contract vs. cost.** The Component Model nominally copies data across component boundaries to preserve isolation. Eo9 erases that cost: because driver implementations are compiled into the same module and linear memory as the program (see Performance), there is no runtime boundary between a program and its backends, so the optimizer can elide the canonical-ABI copies — an `own<buffer>` round-trip lowers to passing a pointer within shared linear memory. WIT describes the ownership *contract*; fusion makes it zero-cost.

> Note: The Component Model's async support was still stabilizing as of this writing; since async I/O is central to Eo9, the concrete encoding may need to track the upstream spec. Operations are declared `async func` — async-ness is part of a function's component-level type, only async-typed exports may block, and that is also what lets providers be implemented as wasm components. `future<T>`/`stream<T>` remain available as value types where a first-class handle is genuinely needed; `stream<T>` is sequential and so is not used for the offset-addressed, random-access disk/net APIs. We deliberately build on the Component Model's async machinery (async functions, tasks, waitable-sets) rather than inventing a parallel Eo9 mechanism — see *Execution APIs*.

### Composition and the `$` operator

Virtualization layers are **providers**, not executors. A provider — `virtualnet`, `virtualfs`, a sandbox — is just a component that *exports* one or more OS-API interfaces (importing only what it itself needs). It exports no `main` and is never run directly; it is a configurable bag of implementations. This is what makes virtualization zero-overhead: a provider's exports are composed into a program's imports and inlined, so a no-op layer compiles out (see Performance). It also keeps wrappers low-privilege — a provider needs no authority to run other programs.

**Binary or provider, never both.** Every module is exactly one kind. A *binary* exports `main(args)` and is run; a *provider* exports interfaces plus `configure(args)` and is composed. The two argument surfaces stage differently: a binary's `main` args are bound at run time and may differ each run, while a provider's `configure` args are bound at compose time. A provider takes config the same type-directed way a binary takes flags — `virtualfs --dir /tmp/sandbox` binds `configure(dir: string)` — and because that config is usually a compile-time constant, the compose-and-compile step specializes the provider with it and inlines, so even a configured layer stays zero-overhead. An invalid value fails at `configure`, before the consumer ever runs. Binding those flags is itself an operation of the component algebra — `configure : provider × args → provider` — which bakes the constants in and re-exports only the API, so the consumer can neither observe nor re-run the configuration. `configure` returns the provider's root capability handle(s) — the `x-impl` resources its exported interfaces operate on — and the consumer-facing `default()` accessor hands out exactly the handle that `configure` produced. Configuration→capability is therefore explicit in the types: an unconfigured provider has no handle to give, so "used before configured" is unrepresentable rather than a convention. (Encoding note: `configure` lives in a small exported config interface rather than as a bare world-level export, because a component cannot mint handles of a resource type it merely `use`s — see the packaging example below.)

`$` is the **composition operator**. `provider $ consumer` satisfies the consumer's imports from the provider's matching exports, yielding a new component. It is **right-associative**, with the rightmost term the ultimate consumer:

```
virtualfs $ virtualnet $ browser  ==  virtualfs $ (virtualnet $ browser)
```

Both providers compose into `browser`. Composition connects an export to an import *only where the consumer actually imports that interface*; unmatched consumer imports remain residuals for the next layer or the surrounding context. Re-association therefore changes meaning:

```
(virtualnet $ virtualfs) $ browser
```

wires `virtualnet` into `virtualfs` only, so `browser` sees `virtualfs`'s exports but never `virtualnet`'s. (Meaningful only if `virtualfs` itself imports net.) This elaborates the example in Virtualization.

**Algebraic properties.** Write `imports(m)` / `exports(m)` for a module's import and export sets — sets of *slots*, each a name carrying an interface type and version, where a slot's name defaults to its interface name (see *Capability slots, `rename`, and `with`*). Composition obeys:

- **Sealing.** In `p $ c`, every import of `c` matched by an export of `p` is *sealed*: it is not an import of the result, and no outer layer — nor the ambient context at run time — can see it or re-satisfy it. The innermost provider wins; a capability decision made close to the consumer can be further attenuated from inside, but never undone from outside.
- **Residuals.** `imports(p $ c) = imports(p) ∪ (imports(c) ∖ exports(p))`: the consumer's unmatched imports flow outward, and the provider's own imports become obligations of the composition.
- **Kind preservation & layering.** `exports(p $ c) = exports(c)`: composition never changes what the rightmost term *is* — providers composed into a binary yield a binary; into a provider, a provider. A provider's exports that the consumer does not import are **dropped**, not re-exported: nothing crosses a composition boundary the consumer didn't declare. (Reusable multi-API bundles are built with `&` instead — see *Environments and the `&` operator* — not by changing `$`.)
- **Identity.** The empty provider (no imports, no exports) is the identity: `empty $ c ≡ c`.
- **Non-associativity.** `$` is not associative — re-association changes who serves whom, as above. Concretely, `(a $ b) $ c ≡ a $ (b $ c)` only when `a` exports nothing that `c` imports and `b` doesn't already provide; hence the fixed right-associative reading.
- **Composition is early context-override.** Modulo fusion, running `p $ c` in context `Γ` behaves like running `c` in the context `exports(p)` layered over `Γ`. Doing the override with `$` — at compose time rather than run time — is exactly what lets the compiler inline the layer and erase its cost (see Performance).

**Precedence.** Argument application binds tighter than `$`, so each module's flags attach to that module before composition:

```
virtualfs --dir /tmp/sandbox $ browser --url https://example.com
==  (virtualfs --dir /tmp/sandbox) $ (browser --url https://example.com)
```

**Executors** are the dual role: an executor *drives or observes* a run (spawn on demand, restart on failure, single-step), where a provider merely *substitutes* an implementation. Rule of thumb: **substitution → provider; supervision → executor.** Statefulness is not the discriminator (a NAT table lives fine inside a provider); driving the run is. Executors come in two flavors with very different privilege: an *interpreting* executor (a debugger, a test harness, the `interpret` slow-path) needs no special capability at all, while a *native* executor (the shell, a root scheduler) holds the Compile capability — the genuinely privileged authority (see *Execution APIs*). The shell is itself an executor that composes providers and runs the result.

### Environments and the `&` operator

Layering with `$` deliberately throws a provider's unmatched exports away — nothing crosses a boundary the consumer didn't declare. That is the right default for *applying* providers to a program, but it means a `$`-chain of providers cannot be packaged up as a value: `time.monotonic-stub $ memfs` simply discards the clock. Building reusable environments is a second operator.

`&` is the **extension operator**: `x & y` is the environment `x` extended — and, where they overlap, overridden — by `y`. Both operands are providers and the result is a provider:

- **Wiring.** Every import of `y` matched by an export of `x` is satisfied by `x` (and sealed, exactly as with `$`).
- **Exports.** `exports(x & y) = exports(y) ∪ (exports(x) ∖ exports(y))` — the right-biased union: `y` shadows `x` wherever both export the same interface.
- **Imports.** `imports(x & y) = imports(x) ∪ (imports(y) ∖ exports(x))`.

`&` is *not* commutative — order is dependency-and-override order, later (righter) layers building on and overriding earlier ones — but it **is** associative, with the empty provider as identity, so environments chain without parentheses: in `x & y & z`, each import is satisfied by the nearest layer to its left that exports it, and each interface is exported by the rightmost layer that provides it. Precedence is application > `&` > `$`.

The two operators fit together by an **action law**:

```
(x & y) $ c  ≡  x $ y $ c
```

`&` is exactly the packaging-up of a `$`-chain of providers into a single value — except that, unlike the chain, the bundle keeps its unconsumed exports visible, which is what makes it usable as an environment in its own right. The override direction is the same in both operators: closer to the ultimate consumer wins (`$`'s sealing), rightward wins (`&`'s shadowing).

```
# A coherent deterministic environment: exports time *and* net, and virtualnet's own
# time import is wired to that same clock — one instance, shared.
> time.monotonic-stub & virtualnet $ app     # ≡ time.monotonic-stub $ virtualnet $ app

# Overriding a slice of a base profile: shadowing is the override.
> posix-base & loopback-net $ app            # posix-base's net is shadowed; the rest shows through

# Middleware: a wrapper imports an interface and re-exports it.
> realnet & nat $ app                        # app sees nat's net, which is backed by realnet's
```

Binaries do not participate in `&` (the result would be both runnable and composable, which the module-kind rule forbids); the final application of an environment to a binary is always `$`. An environment is also how an executor's grantable capabilities are represented: what an executor may pass on to its children is just an environment value it possesses — handed down by its parent, narrowed with `only`, extended with `&` (see *Execution APIs*).

### Capability slots, `rename`, and `with`

A module's ports are **slots**: a slot is a *(name, interface type)* pair, and the name defaults to the interface name — which is why everything above could say simply "imports `eo9:net/net`". Distinct slots of the same type are how a program asks for more than one instance of a capability:

```wit
world backup-tool {
    // Component-level imports are name-keyed, so two slots of one interface type are representable;
    // this is the Eo9 world dialect for it.
    import system-fs:  eo9:fs/fs@1.0.0;   // the tree being backed up
    import scratch-fs: eo9:fs/fs@1.0.0;   // staging space for temporary state
    // ... main, outcome types ...
}
```

`$` and `&` match exports to imports **by slot name** (with the interface-name default, single-instance programs behave exactly as before). `only` keeps matching by interface *type* — the security-relevant question is what kind of capability may cross the gate, not what it is locally called.

Wiring particular providers to particular slots is pure relabeling:

- `rename a b` — a gate term (like `only`) that relabels slot `a` to `b` on everything to its right. It applies to imports and exports alike, is equivalent to composing an auto-generated forwarding adapter, and costs nothing after fusion.
- `with p as name` — binds provider `p` to the slot `name`: sugar for renaming `p`'s export slot to `name` and composing. `p` must export exactly one interface (use `rename` explicitly otherwise). The keyword-first form keeps parsing one-directional — the parser sees `with`, then a provider expression, then `as`, then a slot name — and several bindings may be given in one `with`, comma-separated. Tuples bind positionally: `with (a, b) as (x, y)` means `a as x, b as y` (arities must match).

```
> with realfs as system-fs, memfs as scratch-fs $ backup-tool --src /home --dst /backups
```

### Packaging and submodules

A WIT **package** groups related worlds and interfaces — think of it as a crate: the provider is the `lib`, sibling binaries are the `[[bin]]` targets, and they share the package's interfaces and types (so a tool and the provider it serves can never drift, and they version together as one semver-tagged package).

Worlds are flat — WIT has no nested worlds — so hierarchy is a naming convention, not containment. The bare package name resolves to a designated **default world**; a dotted suffix selects a sibling:

```wit
package eo9:virtualfs@1.0.0 {
    // on-disk layout etc., shared so the provider and its tools never drift
    interface format { record superblock { version: u32, root: string } }

    // the provider — the package's default world; addressed bare as `virtualfs`
    world default {
        use format.{superblock};
        import eo9:disk/disk@1.0.0;             // underlying storage — a residual import
        export eo9:fs/fs@1.0.0;                 // provides the standard fs API
        // configuration is a small exported interface; its `configure` returns the fs-impl
        // handle that `default()` then hands out (a bare world-level export cannot mint
        // handles of a resource type the world merely `use`s from an import):
        export config;   // config.configure: async func(dir: string) -> result<fs-impl, config-error>
    }

    // a binary tool — addressed as `virtualfs.create`
    world create {
        use format.{superblock};
        import eo9:disk/disk@1.0.0;
        export main: func(dir: string, size: u64) -> result<program-success, program-failure>;
    }
}
```

| shell name         | WIT path                | kind     |
|--------------------|-------------------------|----------|
| `virtualfs`        | `eo9:virtualfs/default` | provider |
| `virtualfs.create` | `eo9:virtualfs/create`  | binary   |

The default world is *not* named after the package (so the bare name never doubles as `virtualfs.virtualfs`) and is *not* named `main` (which already means a binary's runnable function). Each world compiles to its own component artifact, shipped together as the package. Deeper hierarchy has no world-level form: flatten the name (`virtualfs.repair`) or split a large subsystem into its own package (`eo9:virtualfs-tools`).

### The module store and compilation cache

Module storage and naming take their inspiration from Nix:

- **Content-addressed module store.** Modules live in a hash-indexed store; a module's identity *is* its content hash. Bare names (`browser`, `virtualfs.create`) resolve through the store (via profiles/manifests — exact mechanism TODO), and resolution hands back the immutable handle that loading and compilation key on (see *Execution APIs*). Multiple versions coexist naturally; an upgrade re-points a name, never mutates a module. The filesystem's deterministic content hashes (see Filesystem API) are the natural substrate.
- **Deterministic, hash-keyed compilation cache.** Compilation must be deterministic, so the compiled artifact for a fused composition is cached under the hash of everything it depends on: the content hashes of every module in the composition, the `configure` constants baked in, the compile options, the target, and the compiler version. Frequently-run (environment, program) pairs therefore compile once and launch from cache; entries are evicted LRU/MFU. Determinism makes the cache shareable and auditable — the same key always yields bit-identical output (TODO: pin down the determinism requirements on codegen).

### Programs as values

Naming or composing a program never runs it. A program is an **open component**: a value with (possibly) unsatisfied imports. *Running* is a separate operation — instantiate the component against a context that satisfies its residual imports, then call `main`. This is the Component Model's component-vs-instance distinction, and it gives the operators clean types:

- `$` `: component, component -> component` — composition; the result is still open.
- `&` `: provider, provider -> provider` — environment extension (see *Environments and the `&` operator*); the result is still open.
- `run` / `interpret` / `exec` `: component -> execution` — close the imports from *their own* context, then invoke `main`.

At the **top level** of a shell command the shell implicitly `run`s the resulting component against the shell's context. In an **argument position** a component is just a value — passed, not run.

**Type-directed arguments.** How the shell parses an argument is determined by the declared type of the parameter it fills (the same mechanism as `--verbose true ⇄ verbose: bool`). A `component`-typed parameter makes the shell evaluate its argument as a *program expression* (resolve names, apply `$`); a `string`-typed parameter takes the same text literally. The parameter type is the sole disambiguator between a module literal and text — there is no quoting sigil, and it is checked statically.

| parameter type | command position     | argument position         |
|----------------|----------------------|---------------------------|
| `component`    | composed **and run** | composed, passed **open** |
| `string`       | —                    | literal text              |

For example:

```
> virtualnet $ browser              # composed, then run by the shell
> interpret (virtualnet $ browser)  # composed, passed open to `interpret`, which runs it its own (slow) way
```

In the second case the residual imports of the parenthesized module are satisfied by `interpret`, not the shell — so an interpreter, debugger, or sandbox gets full control over the import environment of the module it is handed.

**Grouping.** An argument is an atom (a name or literal) or a parenthesized expression; `$` lives at the expression level, so a composition used as a single argument must be wrapped in `()`. Without it, `interpret virtualnet $ browser` would parse as `(interpret virtualnet) $ browser`. We use `()` for grouping only — no `[]` or quoting sigils.

**Representation.** A `component` is data. Unlike the OS-API handles we deliberately do *not* pass as arguments (behavior comes from imports), a module genuinely is bytecode, so passing it as an argument is correct. In-shell it is an opaque `component` resource (composed in-process, pre-validated); `load`/`save` convert to and from `list<u8>` when a module must cross a boundary — e.g. shipping it to another machine to interpret.

### The capability algebra: optional, `none`, `deny`, and `only`

Composition as defined so far can only *add* implementations: a provider satisfies imports, and whatever it doesn't satisfy flows outward to be satisfied ambiently at run time. Three further forms complete the algebra — declaring a capability optional, dropping or denying one, and bounding everything to the right of a point to a fixed allow-list. None of them introduce new runtime machinery: they are ordinary worlds and providers plus one static judgment, and they all compile away under fusion.

**Optional capabilities are typed, not metadata.** Every API declares its root resource and an accessor for obtaining it (this is also how a program gets its `disk-impl`/`net-impl` handle in the first place); the resource itself lives in a types-only sibling interface, so the `-optional` flavor and the stubs can name it without importing any authority. Tooling mechanically derives an `-optional` flavor whose accessor returns `option`:

```wit
package eo9:net@1.0.0 {
    interface types {                         // types only — no authority; safe for anything to import
        resource net-impl;
    }
    interface net {
        use types.{net-impl};
        default: func() -> net-impl;          // the capability's root handle
        // ... ops take borrow<net-impl> ...
    }
    interface net-optional {                  // derived from `net`
        use types.{net-impl};
        default: func() -> option<net-impl>;  // absence is in the type
    }
}
```

- A program that *requires* net imports `eo9:net/net`; one that merely *can use* it imports `eo9:net/net-optional`. Required-vs-optional is therefore visible in the import list itself — to `imports()` introspection, to the loader, and to audit tooling — and the capability set remains statically enumerable.
- **Subsumption.** An export of `X` also satisfies an import of `X-optional`, via a mechanically derived adapter (`default = some(·)`): a present capability always satisfies an optional want.
- **Loader rule.** A missing *required* import is rejected before execution, as above. A missing *optional* import is auto-sealed with `X.none`, so for optional imports *never granted ≡ explicitly dropped ≡ composed with `X.none`* — one semantics.
- **Zero-cost.** After fusion, an absent optional's `default()` is the constant `none` and the dependent code path is dead-code-eliminated; a present one constant-folds the `option` away.
- In world-authoring syntax, `import optional eo9:net/net@1.0.0;` is sugar that lowers to importing the `-optional` flavor.

**Dropping: `X.none`, `X.deny`, and friends.** Dropping is done with ordinary hand-written stub providers that live in each API's package as sibling worlds (addressed by the usual dotted names). They are not auto-generated: each is written once, in that API's own vocabulary, and only where it makes sense.

- Every API package includes `X.none` — the trivial provider exporting `eo9:X/X-optional` with `default()` answering `none`. This one is universal, because the loader and `only` use it to seal absent optional imports.
- An `X.deny` ("present but refusing") exists only for APIs where refusal is meaningful, and fails each op with that API's own error cases — `net.deny` answers every request with net's own denied/unreachable errors. There is deliberately no `time.deny`: denying a clock is meaningless. Such APIs instead ship honest attenuating stubs — e.g. `time.monotonic-stub`, a deterministic stand-in clock — which are just ordinary providers.

| shell name            | role                            | exports                                              |
|-----------------------|---------------------------------|------------------------------------------------------|
| `net.none`            | absence (every API has one)     | `eo9:net/net-optional`, answering `none`             |
| `net.deny`            | refusal (only where sensible)   | `eo9:net/net`, every op failing in net's own errors  |
| `time.monotonic-stub` | attenuation (ordinary provider) | `eo9:time/time`, a deterministic stand-in clock      |

```
> net.none $ browser                  # browser imports net optionally → it observes "no net"
> net.deny $ fetcher --url https://…  # fetcher requires net → every net op fails, in net's own error vocabulary
```

Dropping is therefore just composition, and the sealing law is what makes it a real drop: after `net.none $ browser` there is no residual net import left for an outer layer or the ambient context to satisfy. Laws: `p $ X.none $ c ≡ X.none $ c` whenever `p` provides only X (an outer grant cannot undo an inner drop), and `X.none $ c ≡ c` when `c` never imports X. The shell warns whenever a composed provider's exports match nothing — which is also how you notice you "dropped" a capability the program actually *requires* (that drop is a no-op and the requirement still reaches the ambient context; use `X.deny` or `only` for that case).

**Restriction: `only`.** `only` bounds everything to its right to a fixed allow-list of APIs, and fails *before anything runs* if the right-hand side hard-requires anything outside it.

```
> only eo9:time,eo9:fs $ cruncher --input data.bin   # cruncher requires only fs+time → runs
> only eo9:time,eo9:fs $ browser --url https://…     # browser requires net → compose-time error
> only eo9:fs $ virtualnet $ browser                 # OK: net is satisfied *inside* the gate (loopback);
                                                     #     real net can never reach the composition
```

The allow-list is just a **world** — a set of interface names (an entry admits both the required and `-optional` flavor of that interface, matched by the same semver rule as imports). `only w` is a *gate term*: not a component (what it must seal depends on its consumer), but a second kind of left operand for `$`, with `gate $ component -> component`. Argument application binds tighter than `$` as usual, and a named world may stand in for the inline list: `only sandbox.no-net $ …`.

Semantics of `only w $ c`, where `c` is the whole composition to the right (right-associativity has already collapsed it):

1. Every **required** residual import of `c` not in `w` is a **compose-time error**, naming the offenders. This is earlier than the load-time check — nothing is instantiated.
2. Every **optional** residual import of `c` not in `w` is sealed with `X.none`.
3. Exports are untouched; the result is an ordinary open component with `imports(only w $ c) ⊆ w ∩ imports(c)`.

The only new primitive is the static judgment in step 1; step 2 is sugar over `X.none` composition. Laws:

- `only w` is idempotent, and restrictions **intersect**: `only v $ only w $ c ≡ only (v ∩ w) $ c`. A restriction can always be narrowed from outside, never widened — attenuation is monotone.
- **Position matters.** Providers to the *right* of the gate are inside it (their residual imports are checked and sealed too); providers to the *left* can only feed through interfaces the gate admits. "Satisfy inside, then restrict" and "restrict, then it must not need it" are both expressible, as in the third example above.
- With an empty allow-list the result is a fully closed program — pure compute.
- The result is still an ordinary component value: it can be passed to `interpret`, `save`d, or shipped, and the bound travels with it.

A gate at the far left of a top-level command bounds what the shell's ambient context may inject into that command — the per-command least-privilege form. Standard policy worlds (e.g. `eo9:sandbox/no-net`, `eo9:sandbox/pure`) make common restrictions nameable and reusable; a policy world compiles to no component at all — it is pure interface, referenced by name. An interpreting executor can enforce the same bound dynamically, by simply declining to forward anything outside `w`; `only` is the static form — it fails before anything runs, and the restricted component is inspectable and shippable.

# Deliverables

There are a few deliverables we want for the MVP:

## Basic OS API specs

### Execution APIs

Running programs decomposes into pieces with very different privilege. The guiding asymmetry: **an interpreter bug only harms the interpreter's own sandbox; a compiler bug mints unsafe native code and harms everyone.** So the privilege line sits at codegen, not at "running programs".

**Component algebra (unprivileged).** Pure value manipulation on program bytecode. No authority is required — this could be an ordinary library; exposing it as an API just avoids every executor bundling the tooling.

```wit
interface component-algebra {
    resource component;          // an open program value: binary or provider

    /// One slot a component still needs (a residual import).
    record import-need {
        slot:      string,       // slot name (defaults to the interface name)
        interface: string,       // e.g. "eo9:net/net"
        version:   string,       // semver it was built against (satisfied per the semver rule above)
        required:  bool,         // mandatory vs. optional import
    }

    load:     func(image: list<u8>) -> result<component, load-error>;
    save:     func(c: borrow<component>) -> list<u8>;
    describe: func(c: borrow<component>) -> component-info;   // kind, imports, exports, arg signature

    compose:  func(p: component, c: component) -> result<component, compose-error>;                 // `$`
    extend:   func(base: component, layer: component) -> result<component, compose-error>;          // `&`
    restrict: func(c: component, allow: list<interface-ref>) -> result<component, restrict-error>;  // `only`
    rename:   func(c: component, from: string, to: string) -> result<component, rename-error>;      // slot relabeling
    configure: func(p: component, args: list<named-arg>) -> result<component, configure-error>;     // bind provider config at compose time
}
```

**Loading is immutability-first.** Opening a file *for execution* yields an **immutable handle** — only filesystems that can promise immutability (COW or content-addressed backends) can back execution — and the component algebra turns immutable handles into `component` values; `load` from raw `list<u8>` remains for components that arrive over other channels. Immutability gives TOCTOU-free loading and a stable content identity, which is exactly what compilation caches, signatures, and the filesystem's content hashes key on (see Filesystem API).

**Interpretation (unprivileged).** An interpreter is just a program; `eo9:interp` ships as a standard component, but anyone can write one. An interpreted child's imports are satisfied by the interpreter *from its own imports*, so the child can never exceed what the interpreter already holds and chooses to forward — confinement is automatic and requires no capability at all. Inspection of interpreted children (single-stepping, watchpoints, deterministic replay) is likewise free, because the interpreter mediates every step. There is no separate "inspect" privilege anywhere in the system: you can always inspect what you interpret, native children are inspectable only to the degree chosen at compile time (debug info, safepoint maps), and there are simply no handles to tasks you didn't create.

**Compile and Task APIs (privileged).** The dangerous authority is asking the TCB to generate native code and admit it for execution; holding Compile is what makes a program a *native* executor (see *Composition and the `$` operator*). Sketch:

```wit
/// Types-only, so holding `task` never implies holding `compile` (or vice versa).
interface images {
    /// An opaque compiled artifact; admitted for execution via `task`, never read back as bytes.
    resource image;
}

interface compile {
    use component-algebra.{component};
    use images.{image};

    // `c` must be a *closed binary*: every capability decision was already made — inspectably — with the
    // component algebra. Options select debug info / safepoint maps, i.e. how inspectable the native task is.
    compile: func(c: component, opts: compile-opts) -> result<image, compile-error>;
}

interface task {
    use images.{image};

    resource task;

    /// Static resource limits, fixed at spawn. CPU is not here — CPU is fuel, and fuel is donated.
    record spawn-limits {
        max-memory: option<u64>,   // ceiling on linear-memory growth, enforced at memory.grow
    }

    // `args` are main's named, typed arguments, WAVE-encoded and checked against main's signature.
    spawn: func(i: borrow<image>, args: list<named-arg>, limits: spawn-limits) -> result<task, spawn-error>;  // borrow: one cached image, many spawns

    variant resume-outcome { out-of-fuel, blocked, done(program-outcome) }

    /// Donate `fuel` to the task and run it now, on the caller's own CPU time; returns when the fuel is
    /// spent, the task blocks on I/O, or it finishes. Fuel is conserved — a scheduler can only donate fuel
    /// it was itself donated — so CPU budgets compose down the task tree. Fuel-metered yields keep
    /// interleaving deterministic.
    resume: func(t: borrow<task>, fuel: u64) -> resume-outcome;

    /// Readiness and lifecycle are ordinary async calls — the same vocabulary a program already uses for
    /// its own I/O. "Wait for any of my children plus my own I/O" is plain async code.
    runnable: async func(t: borrow<task>);                      // returns when a blocked task can make progress
    wait:     async func(t: borrow<task>) -> program-outcome;   // returns when the task finishes
    kill:     async func(t: borrow<task>) -> program-outcome;

    // TODO: the multi-core rule (a task is resumed by at most one scheduler at a time); the thread-spawn
    // capability for intra-task parallelism (see the bullets below).
}
```

- **Closed before compile.** There is no ambient `context` and no `override` mechanism: substitution and interposition are composition, decided with `$`/`&`/`only` before codegen and visible in the component value. The shell has no private powers — its top-level rule is literally "compose my environment onto the command, compile, spawn".
- **Environments are just data.** What an executor may grant onward is an environment value it *possesses*: handed down by its parent (boot hands one to `init`/the shell), passable as an argument like any component, narrowed with `only`, extended with `&`. Possessing driver bytecode is harmless by itself — without Compile it can only be interpreted, and a driver's own imports of raw hardware capabilities (MMIO regions, interrupt lines) are satisfied only by the OS core at instantiation. Those hardware roots are the only true ambient context in the system.
- **Schedulers are ordinary programs.** Codegen inserts fuel-metered yield points (fuel rather than epoch deadlines, for determinism), so native tasks are cooperatively schedulable *by construction*. A scheduler is then just a program holding `task` handles and deciding which to resume — nested schedulers, supervisors, and deterministic test schedulers are all unprivileged. The irreducibly privileged residue is the compiler that guarantees the yields, and the root holder of timer interrupts and the idle loop.
- **One concurrency vocabulary.** Programs, providers, supervisors, and schedulers all use the Component Model's async machinery — async-typed functions, structured tasks/subtasks, waitable-sets, and `future`/`stream` values where a first-class handle is needed — for everything concurrent. The Task API adds no parallel notion of its own: `wait`/`runnable` are ordinary futures, so "block on any of my children plus my own I/O" is plain async code. The single genuinely OS-level primitive is `resume(task, fuel)`, because donating CPU has no Component Model analog. We build on the Component Model async ABI even while it stabilizes rather than growing a parallel mechanism; its host side is one implementation shared by usermode and bare metal.
- **How readiness is implemented.** Under the hood the OS core implements the host side of that ABI with per-task completion queues and edge-triggered doorbells: a backend pushes a completion record and rings the doorbell only on the empty→non-empty transition; on its next resume the task drains the queue and dispatches to its parked waitable-sets. O(1) per completion and at most one wake per batch (the io_uring shape) — this is what lets the disk/net APIs scale to millions of concurrent ops. A parent never learns *what* a child is blocked on: that is the child's suspended state, and a supervisor that wants to know asks a diagnostic query or interprets the child. Fuel fixes the CPU interleaving, and completion *order* becomes deterministic exactly when the providers are deterministic (`fs.memfs`, `entropy.seeded`, `time.frozen`) — the deterministic-test story goes all the way down.
- **There is no fork.** Concurrency inside a program is Component Model structured concurrency — free, not a capability, and unable to exceed the program's own imports. Creating a new *schedulable entity* (new linear memory, new capability set, new fuel budget) is `spawn`, which is necessarily a capability because it is precisely the act of granting capabilities and CPU. Unix-style `fork()` is a non-goal.
- **Parallelism is a capability.** A task starts with one execution context; CPU parallelism *across* tasks is the scheduler's business and works from day one. Parallelism *within* a task (shared-memory threads) is host-mediated in WASM by construction, so Eo9 exposes it as a capability (`eo9:threads` — TODO, tracking the upstream shared-everything-threads proposal): untrusted code is simply not granted it (a second context plus shared memory is a high-resolution clock — see Security), and deterministic environments don't grant it either, since fuel-determinism only holds for single-context tasks.
- **Resource limits.** Memory: a per-task ceiling fixed at spawn and enforced where WASM already asks the host for memory — `memory.grow`; there is no ambient malloc to police (allocators are guest code inside the program's own linear memory). CPU: fuel, conserved down the task tree as above. Everything else (DMA buffers, disk space, bandwidth, handle counts) flows through providers, so quotas there are provider configuration — ordinary attenuation. Shell surface for limits (a `limit` gate or spawn flags) is TODO.
- **Kill and linearity.** The global contract is small: a killed task never observes anything again, and anything it transferred away (an `own<buffer>` in flight) belongs to the transferee, which completes or aborts the operation on its own schedule and then drops the now-unreceivable result — nothing dangles, nothing leaks. Whether a half-done operation is aborted or completed is each provider's documented, per-API semantics.
- **State sharing.** Fusion shares *implementation*, never state. Spawning two children from one environment gives each its own fused copy of the provider code; they share state only where a provider's backing resources are shared (the real disk, a common store) — which is the provider's business, not the API's.
- **Arguments and outcomes.** The canonical value encoding is **WAVE** (the Component Model's value text format): `eosh` parses flags and prints results as WAVE, and a generic executor binds `main`'s arguments and renders outcomes the same way. `program-outcome` is a flat three-way variant: `success(…)` and `failure(…)` carry the program's own success/failure variants, WAVE-rendered with their type; `abnormal(abnormal-exit)` covers runs that never returned (`trapped(reason)`, `killed`). A program's `main` itself still returns only `result<program-success, program-failure>` — a program cannot report its own trap; the abnormal arm belongs to the executor's view. WAVE is type-directed, which is fine: an executor always holds the component and therefore its types, and an outcome that outlives its component carries a type descriptor alongside the value. A caller that statically knows the callee's world goes typed and lossless instead.

### Disk API

Raw block-device access: offset-addressed reads/writes of owned buffers against a `disk-impl`. Deliberately
carries no filesystem semantics — that is the Filesystem API's job, layered on top.

TODO - we want to support ultrahigh concurrency and DMA

Standard stubs: `disk.none`, `disk.readonly`, `disk.mem` (RAM-backed).

### Filesystem API

Unix-comparable filesystem operations, layered over a disk (or other) backend.

TODO - we want to provide standard FS stuff, but also some optional additions, exposed where the backing store supports them, like deterministic name/content based hashes all the way up the FS tree.
You can easily look at the pre-computed hash of any FS node (file or dir) to see if it or its descendants have changed since last snapshot.
Lets us build backend-agnostic versions of stuff like ZFS tree walk for backup.

Also needed: **immutable handles** — opening a file *for execution* yields an immutable handle (COW / content-addressed backends only), which program loading, compile caching, and signing key on (see *Execution APIs*).

**The native filesystem (`eofs`).** Eo9 bundles its own filesystem, and it is just a provider: it imports `eo9:disk` (plus time/entropy) and exports `eo9:fs`, so the same component runs on bare metal over a real block device and under usermode Eo9 over a file-backed or in-memory disk. Design: copy-on-write with never-overwrite-in-place and atomic root flips (crash consistency by construction, no fsck); a Merkle tree throughout, so every node's content hash is precomputed all the way to the root (integrity checking, cheap change detection, hash-guided incremental backup); snapshots are retained roots, with deferred reclamation of unreferenced blocks; block compression is **on by default** (lz4-class for the MVP; the format tags the codec per block so zstd or others can be added without a format change); hashes use a fast cryptographic hash (blake3, shared with the module store). Immutable execution handles are structural on eofs — opening for execution pins a subtree hash, no copying. Hosted providers (such as the unix-directory provider in usermode) are compatibility shims; eofs is the real thing. MVP non-goals: multi-device/RAID, dedup, quotas, encryption.

Standard stubs: `fs.none`, `fs.deny`, `fs.readonly`, `fs.memfs`.

### Net API

TODO - similar goals to disk

Standard stubs: `net.none`, `net.deny`, `net.loopback`.

### Text API

TODO - std{i,o,err}

Standard stubs: `text.none`, `text.null` (discard), `text.capture` (buffer or forward over the Message API).

### Message API

TODO - typed message channels between running programs: the substrate for pipes and std{i,o} plumbing, parent↔child
supervision traffic, and interposition providers that forward to their parent (see *Execution APIs*).

### Entropy API

TODO

Standard stubs: `entropy.none`, `entropy.seeded` (deterministic PRNG from a fixed seed — reproducible tests).

### Time API

TODO

Standard stubs: `time.none`, `time.monotonic-stub` (deterministic stand-in clock), `time.frozen`, `time.fuzzy` (degraded/jittered resolution for side-channel mitigation; see Security).

### Perf Measurement API

TODO

Standard stubs: `perf.none`, `perf.null` (accept and discard). Note: perf counters are themselves a timing side channel — gate them like time.

### PCI API

Raw PCI/PCIe device access for drivers (`eo9:pci`): enumeration, configuration-space and BAR register access
(width-explicit rather than buffer-oriented, since register access is width-sensitive), MSI/MSI-X/INTx
interrupts awaited as ordinary async calls, and per-device DMA buffer allocation. It carries no device-class
semantics and no policy — which devices are visible is entirely the provider's choice, and `pci.filtered`
makes "exactly this one device" grants an ordinary compose-time attenuation. A PCI capability that can enable
bus mastering is, absent an IOMMU, effectively full-memory authority, so hardware-root providers are expected
to hand out the narrowest device handle that works and to contain DMA with the IOMMU where one exists; like
all hardware roots, the real capability is satisfied only by the OS core at instantiation.

Standard stubs: `pci.none`, `pci.deny`, `pci.filtered` (allow-listed device addresses).

### TODO - other APIs


## Usermode binary

We want an `eo9` binary which provides (in macos/linux/etc) a usermode implementation of `eo9` with appropriate OS APIs
backed by standard *nix APIs. You can invoke this appropriately to get an Eo9 instance running the specified program (which could be a shell).

## Bootable QEMU Images

We want bootable images for Eo9 for AMD64, AArch64, and rv64gc. These images should support running programs headless, as well as booting to shell.

## Test Suite

We want both a usermode and in-QEMU test suite.

# Implementation Details

OS core is written in Rust. Codegen is Cranelift, embedded via Wasmtime where practical (usermode first); the bare-metal codegen/runtime strategy (on-target codegen vs. host-side AOT feeding a slim runtime) is TODO.

The scheduler is ours, and it is the *same* scheduler on bare metal and in usermode: the usermode `eo9` binary hosts the bare-metal scheduler rather than delegating to a host async runtime. This keeps transitive dependencies small and makes usermode a faithful test bed for the real thing.

Bundling an interpreter (`eo9:interp`) is optional for the MVP — nothing strictly requires it — but it is useful early for compatibility testing and in production for ultra-high-security interpretation-only execution. Which interpreter to bundle is TBD.

## Shell

We should provide a built-in shell for Eo9. Call it `eosh`.

The shell should support invoking programs and providers, composing them with `$` and `&`, the capability forms `x.none`, `x.deny`, and `only` (see *The capability algebra*), and slot wiring with `rename` and `with … as …` (see *Capability slots, `rename`, and `with`*). It also supports `let`-bindings for session-local names of component and environment values — `let det-env = time.monotonic-stub & virtualnet` — so composed environments can be named and reused. Bare program names resolve through the filesystem/package store: resolution opens the file for execution, yielding the immutable handle that `load` consumes (see *Execution APIs*).

# Overall Guiding Principles

There are a few important guiding principles for the design and implementation of this OS.
1. It should be elegant and beautiful.
2. It should be safe by construction.
3. It should have clear, algebraically-expressed properties whenever possible.

We should never take hacks or shortcuts. Do things properly and with mathematical elegance.

**Almost nothing in this spec is sacred.** Every concrete choice here — operators and their laws, API shapes, encodings, naming — is provisional. If implementation reveals that a choice is a pain, a sticking point, or simply less elegant than an alternative, we have total freedom (and the obligation) to change the design rather than work around it. The spec serves elegance, not the other way around: change the design, update the spec, move on. The only sacred things are the three guiding principles above and the no-hacks-no-shortcuts rule; everything else is up for renegotiation.
