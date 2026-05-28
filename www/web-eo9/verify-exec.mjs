// Node (v25, JSPI) verification harness for the /vm blob's component-algebra demo,
// mirroring www/site/vm/vm.js's import glue. Not part of CI (needs node + JSPI); run after
// `cargo xtask build-web-vm`:  node www/web-eo9/verify-exec.mjs
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const vmDir = join(here, "..", "site", "vm");
const assets = JSON.parse(readFileSync(join(vmDir, "assets.json"), "utf8"));
const blobPath =
  process.env.BLOB || join(vmDir, assets.blob.replace(/^\/vm\//, ""));

const decoder = new TextDecoder();
let memory = null;
const lines = [];

const imports = {
  env: {
    host_write: (ptr, len) => lines.push(decoder.decode(new Uint8Array(memory.buffer, ptr, len))),
    host_now_ms: () => Date.now(),
    host_monotonic_ns: () => performance.now() * 1e6,
    host_random_fill: (ptr, len) => {
      const view = new Uint8Array(memory.buffer, ptr, len);
      for (let i = 0; i < len; i++) view[i] = (Math.random() * 256) | 0;
    },
    host_fetch_copy: () => {},
    host_sleep_ms: new WebAssembly.Suspending((ms) => new Promise((r) => setTimeout(r, ms))),
    host_read_line: new WebAssembly.Suspending(async () => -1),
    host_fetch_len: new WebAssembly.Suspending(async () => -1),
    host_compile_copy: () => {},
    host_compile_len: new WebAssembly.Suspending(async () => -1),
  },
};

const { instance } = await WebAssembly.instantiate(readFileSync(blobPath), imports);
const x = instance.exports;
memory = x.memory;

const algebraDemo = WebAssembly.promising(x.algebra_demo);
if (x.boot() !== 0) throw new Error("boot failed");
const rc = await algebraDemo();

// In-blob codegen: compile a raw component and an algebra-fused composition inside the blob
// (Cranelift -> Pulley) and run them — fully client-side (every network/server import in this
// harness is stubbed to "unavailable", so a pass proves no server involvement).
const compileDemo = WebAssembly.promising(x.compile_demo);
const rcCompile = await compileDemo();

console.log(lines.join("\n"));
const text = lines.join("\n");
const checks = [
  ["describe: kind = binary", /describe: kind = binary/],
  ["imports eo9:text/text", /import eo9:text\/text/],
  ["imports eo9:time/time", /import eo9:time\/time/],
  ["only -> sealed component", /only .* -> a sealed component/],
  ["execution -> success(greeted)", /success\(greeted\)/],
  ["in-blob codegen: hello compiled client-side", /hello compiled in [\d.]+ ms \(client-side, no server\)/],
  ["in-blob codegen: compiled hello ran (greeted)", /compiled in-blob -> success\(greeted\)/],
  ["in-blob codegen: fused composition compiled", /the fused composition compiled in [\d.]+ ms/],
  ["in-blob codegen: fused composition ran (generated(3))", /fused and compiled in-blob -> success\(generated\(3\)\)/],
  ["in-blob codegen: self-hosted line", /the browser VM is self-hosted/],
];
let ok = rc === 0 && rcCompile === 0;
for (const [label, re] of checks) {
  const pass = re.test(text);
  if (!pass) ok = false;
  console.log(`  ${pass ? "ok" : "MISSING"}: ${label}`);
}
console.log(`\nalgebra_demo rc=${rc} -> ${ok ? "PASS" : "FAIL"}`);
process.exit(ok ? 0 : 1);
