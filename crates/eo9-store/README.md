# eo9-store

The Eo9 module store: content-addressed component storage, name resolution,
and the hash-keyed compilation cache that makes repeat runs start instantly.

## Part of Eo9

Eo9 is a capability-secure operating system built on the WebAssembly Component
Model: programs are wasm components, every capability (filesystem, clock, entropy,
network, devices) is an explicit typed import, and capabilities are granted, attenuated,
and mocked by composing components with a small algebra. The same programs run in a
usermode host, on a bare-metal kernel (aarch64 / riscv64 / x86_64), and in a browser.

Repository, specification, and documentation: <https://github.com/wyager/eo9>

## License

MIT
