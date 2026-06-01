# eo9-embed

Embed a usermode Eo9 instance in a Rust program: a small builder API over
eo9-runtime with pluggable root providers, so an application can run sandboxed,
capability-scoped Eo9 programs in-process.

## Part of Eo9

Eo9 is a capability-secure operating system built on the WebAssembly Component
Model: programs are wasm components, every capability (filesystem, clock, entropy,
network, devices) is an explicit typed import, and capabilities are granted, attenuated,
and mocked by composing components with a small algebra. The same programs run in a
usermode host, on a bare-metal kernel (aarch64 / riscv64 / x86_64), and in a browser.

Repository, specification, and documentation: <https://github.com/wyager/eo9>

## License

MIT
