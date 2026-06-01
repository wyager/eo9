# eofs-core

The engine of eofs, Eo9's native filesystem: a copy-on-write, Merkle-hashed,
checksummed, lz4-compressed on-disk format over an abstract block device.

`no_std` + `alloc`: the same code runs in host tests, inside the wasm filesystem
provider component, and in the bare-metal Eo9 kernel.

## Part of Eo9

Eo9 is a capability-secure operating system built on the WebAssembly Component
Model: programs are wasm components, every capability (filesystem, clock, entropy,
network, devices) is an explicit typed import, and capabilities are granted, attenuated,
and mocked by composing components with a small algebra. The same programs run in a
usermode host, on a bare-metal kernel (aarch64 / riscv64 / x86_64), and in a browser.

Repository, specification, and documentation: <https://github.com/wyager/eo9>

## License

MIT
