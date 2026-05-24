# Eo9 Operating System Spec

Eo9 is an operating system built on modern language-theoretic principles for security, performance, and composability.

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

This would invoke the `browser` program with its
Eo9-standard `fs : Map<FsName,FsImpl>` and `net : Map<NetName,NetImpl>` arguments set to
a file-backed filesystem and a mocked-out loopback-only networking interface.

## Performance

Because each program is distributed as WASM bytecode, programs are compiled down to the native architecture
and (depending on compile settings), driver implementations are inlined into each program.

The consequences of this are:
1. There is no kernel-mode/user-mode boundary. There are no context switches, except for scheduler switches between programs.
2. There is no overhead from memory isolation. No MMU, no nested address translation.
3. There is no overhead from virtualization. Virtualized implementations that do nothing at all simply get compiled out.

## Hardware Support

Eo9 supports any platform where we can compile WASM, including MMUless ARM and RISC-V.

## Eo9-as-program

Because Eo9 is fundamentally just a WASM compiler and some standard APIs, we can run Eo9 as a usermode program on
any OS (including Eo9 itself).


## The Details

### Eo9 API design

Eo9 OS APIs are designed around modern patterns that support a high decree of concurrency and asynchronicity.

Eo9 OS APIs are built around futures which resolve asynchronously and can be blocked on individually or jointly. For example, the disk API looks like

```
fn read(fs : &FsImpl, offset: u64, dst: Buffer) -> Async<(Buffer, Result<ReadResult, ReadError>)>
fn write(fs : &FsImpl, offset: u64, src: Buffer) -> Async<(Buffer, Result<WriteResult, WriteError>)>
```

The `Buffer` is passed by ownership and returned to the caller on completion (on both success and error), so the backend holds it uniquely across the async boundary; see WASM runtime for how this lowers to WIT.

Implementations are designed to scale up to millions of concurrent read/write ops to handle the reality of modern high-IOPS SSDs/filesystems/RAID implementations.

### WASM runtime

Each Eo9 program is a WASM module (a Component Model component).

A WASM module's only channel to the outside world is its imports: core WASM has no syscalls and no ambient I/O, so a program can affect nothing it was not explicitly handed. The import set therefore *is* the capability set — which is what makes Eo9 secure-by-construction (see Security): everything a program can do is statically enumerable from its imports before it ever runs.

The WASM module imports the set of OS APIs it wants access to. Required OS APIs are imported as a mandatory type, and optional OS APIs are imported as an optional type. We use WIT (the Component Model's interface-definition language) for import/export specification; a WIT `world` is precisely the declaration of which APIs, by name and type, a program requires and provides.

The WASM module exports, at minimum, a `main` entrypoint, which we invoke for normal program execution.

At load time, the OS scans the imports and ensures that, for each one, we know how to provide a resource of the specified name and type. Anything we cannot satisfy is rejected before execution.

Interfaces are defined by the Eo9 standard and versioned as semver-tagged WIT packages (e.g. `eo9:disk@1.0.0`), so a program pins the exact API contract it was built against. Shared types such as buffers live in their own package and are pulled in with `use`. For example,

```wit
// Shared I/O types, used across disk, net, text, …
package eo9:io@1.0.0 {
    interface buffers {
        /// A handle to a (possibly DMA-backed) block of memory. Transferred by
        /// `own`, so a backend takes exclusive possession for the life of an op.
        resource buffer {
            constructor(len: u64);
            len: func() -> u64;
        }
    }
}

package eo9:disk@1.0.0 {
    interface disk {
        use eo9:io/buffers@1.0.0.{buffer};

        resource fs-impl;

        record read-result  { bytes-read: u64 }
        record write-result { bytes-written: u64 }
        variant read-error  { not-found, io(string), out-of-range }
        variant write-error { io(string), out-of-range, read-only }

        /// own<buffer> in, own<buffer> back out (on both success and error).
        read:  func(fs: borrow<fs-impl>, offset: u64, dst: own<buffer>)
            -> future<tuple<own<buffer>, result<read-result, read-error>>>;
        write: func(fs: borrow<fs-impl>, offset: u64, src: own<buffer>)
            -> future<tuple<own<buffer>, result<write-result, write-error>>>;
    }
}

// A program targets a `world`, importing the versioned interfaces it needs.
package eo9:browser@0.1.0 {
    world browser {
        import eo9:disk/disk@1.0.0;
        import eo9:net/net@1.0.0;
        export run: func();
    }
}
```

**Ownership and buffers.** WIT has no mutable/immutable data references — there is no `&`/`&mut`. Plain data (lists, records, …) is passed by value, and the only ownership concepts, `own<T>` and `borrow<T>`, apply solely to opaque `resource` handles.

For I/O buffers we use an **owned-buffer round-trip**: the caller transfers an `own<buffer>` to the backend and gets it back when the operation completes. Because `own` is linear (consumed on transfer), the backend has manifestly unique ownership of the buffer for the whole duration of the async operation — no aliasing, and no reference whose lifetime must span an await point. A `borrow<T>`, by contrast, is valid only for the operation it was passed to and may not be retained beyond it; that suits the `fs-impl` handle (a reference to an OS-owned resource) but not a buffer the backend must take exclusive possession of and return. The buffer comes back on *both* the success and error paths — placed outside the `result` so a failed op never leaks it.

Modeling the buffer as a `resource` rather than a `list<u8>` also makes it DMA-friendly: it can be backed by host/driver-managed memory, so `own<buffer>` transfer maps directly onto "who may touch this I/O region right now," and the bytes never move.

**Contract vs. cost.** The Component Model nominally copies data across component boundaries to preserve isolation. Eo9 erases that cost: because driver implementations are compiled into the same module and linear memory as the program (see Performance), there is no runtime boundary between a program and its backends, so the optimizer can elide the canonical-ABI copies — an `own<buffer>` round-trip lowers to passing a pointer within shared linear memory. WIT describes the ownership *contract*; fusion makes it zero-cost.

> Note: The Component Model's async support (`future`/`stream`) was still stabilizing as of this writing; since async I/O is central to Eo9, the concrete encoding of `future<…>` may need to track the upstream spec. `stream<T>` is sequential and so is not used for the offset-addressed, random-access disk/net APIs.

# Deliverables

There are a few deliverables we want for the MVP:

## Basic OS API specs

### Execute API

When provided, allows programs to invoke other WASM programs. In practice, this is usually the top-level WASM compiler (unless virtualized for security reasons).

### Disk API

TODO - we want to support ultrahigh concurrency and DMA

### Filesystem API

TODO - we want to provide standard FS stuff, but also some new things like deterministic name/content based hashes all the way up the FS tree.
You can easily look at the pre-computed hash of any FS node (file or dir) to see if it or its descendants have changed since last snapshot.
Lets us build backend-agnostic versions of stuff like ZFS tree walk for backup.

### Net API

TODO - similar goals to disk

### Text API

TODO - std{i,o,err}

### Entropy API

TODO

### TODO - other APIs


## Usermode binary

We want an `eo9` binary which provides (in macos/linux/etc) a usermode implementation of `eo9` with appropriate OS APIs
backed by standard *nix APIs. You can invoke this appropriately to get an Eo9 instance running the specified program (which could be a shell).

## Bootable QEMU Images

We want bootable images for Eo9 for AMD64, AArch64, and rv64gc. These images should support running programs headless, as well as booting to shell.

## Test Suite

We want both a usermode and in-QEMU test suite.
