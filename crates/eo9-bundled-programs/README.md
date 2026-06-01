# eo9-bundled-programs

Prebuilt Eo9 guest components — the eosh shell, the example programs, the
coreutils, and the standard stub providers — bundled as data so a `cargo install
eo9` build can seed a working system without the wasm guest toolchain.

The contents are produced by the Eo9 repository's `cargo xtask refresh-components`
and are not meant to be edited or consumed directly; depend on the `eo9` binary
crate instead.

## Part of Eo9

Eo9 is a capability-secure operating system built on the WebAssembly Component
Model: programs are wasm components, every capability (filesystem, clock, entropy,
network, devices) is an explicit typed import, and capabilities are granted, attenuated,
and mocked by composing components with a small algebra. The same programs run in a
usermode host, on a bare-metal kernel (aarch64 / riscv64 / x86_64), and in a browser.

Repository, specification, and documentation: <https://github.com/wyager/eo9>

## License

MIT
