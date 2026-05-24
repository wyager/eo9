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

Eo9 supports any platform where we can compile WASM, including 32-bit MMUless ARM and RISC-V.

## Eo9-as-program

Because Eo9 is fundamentally just a WASM compiler and some standard APIs, we can run Eo9 as a usermode program on
any OS (including Eo9 itself).


## The Details

### Eo9 API design

Eo9 OS APIs are designed around modern patterns that support a high decree of concurrency and asynchronicity.

Eo9 OS APIs are built around futures which resolve asynchronously and can be blocked on individually or jointly. For example, the disk API looks like

```
fn read(fs : &FsImpl, offset: u64, dst: Buffer) -> Async<Result<ReadResult, ReadError>>
fn write(fs : &FsImpl, offset: u64, src: Buffer) -> Async<Result<WriteResult, WriteError>>
```

Implementations are designed to scale up to millions of concurrent read/write ops to handle the reality of modern high-IOPS SSDs/filesystems/RAID implementations.

### WASM runtime

Each Eo9 program is a WASM module.

The WASM module imports the set of OS APIs it wants access to. Required OS APIs are imported as a mandatory type, and optional OS APIs are imported as an optional type.

We use WIT for import/export specification.

The WASM module exports, at minimum, a `main` entrypoint, which we invoke for normal program execution.

At execution, the OS scans over the set of imports and ensures that we know how to provide resources of the specified name/types.

Resources are defined by the Eo9 standard and are versioned. For example,

```
interface disk.v1 {
    resource fs-impl;

    record read-result { bytes-read: u64 }
    variant read-error { not-found, io(string), out-of-range }

    read: func(fs: borrow<fs-impl>, offset: u64, dst: list<u8>)
        -> future<result<read-result, read-error>>;   // your Async<Result<…>>
}

world browser {
    import disk;
    import net;
    export run: func();
}
```
