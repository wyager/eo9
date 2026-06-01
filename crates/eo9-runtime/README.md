# eo9-runtime

The wasmtime embedding for usermode Eo9: engine configuration, the Compile and
Task host APIs, fuel-metered resumable execution, capability linking, and WAVE
argument/outcome handling. This is the privileged half of execution; the component
algebra (eo9-component) is the unprivileged half.

## Part of Eo9

Eo9 is a capability-secure operating system built on the WebAssembly Component
Model: programs are wasm components, every capability (filesystem, clock, entropy,
network, devices) is an explicit typed import, and capabilities are granted, attenuated,
and mocked by composing components with a small algebra. The same programs run in a
usermode host, on a bare-metal kernel (aarch64 / riscv64 / x86_64), and in a browser.

Repository, specification, and documentation: <https://github.com/wyager/eo9>

## License

MIT
