// Node (v25, JSPI) verification harness for booting eosh in the /vm blob, mirroring
// www/site/vm/vm.js's import glue. Not part of CI (needs node + JSPI); run after
// `cargo xtask build-web-vm`:  node www/web-eo9/verify-eosh.mjs
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const vmDir = join(here, "..", "site", "vm");
const assets = JSON.parse(readFileSync(join(vmDir, "assets.json"), "utf8"));
const blobPath =
  process.env.BLOB || join(vmDir, assets.blob.replace(/^\/vm\//, ""));

const decoder = new TextDecoder();
const encoder = new TextEncoder();
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
  },
};

const { instance } = await WebAssembly.instantiate(readFileSync(blobPath), imports);
const x = instance.exports;
memory = x.memory;

function passStr(s) {
  const bytes = encoder.encode(s);
  const ptr = x.web_alloc(bytes.length);
  new Uint8Array(memory.buffer, ptr, bytes.length).set(bytes);
  return [ptr, bytes.length];
}

if (x.boot() !== 0) throw new Error("boot failed");

// Sub-step 1 (the floor): eosh instantiates against the in-blob exec/text/fs surface.
const eoshInstantiate = WebAssembly.promising(x.eosh_instantiate);
const instRc = await eoshInstantiate();

// Sub-step 2: eosh runs a command one-shot.
const eoshCommand = WebAssembly.promising(x.eosh_command);
const [ptr, len] = passStr("hello --name web --excited true");
const cmdRc = await eoshCommand(ptr, len);
x.web_free(ptr, len);

const text = lines.join("\n");
console.log(text);

const checks = [
  ["eosh instantiates (floor)", instRc === 0 && /eosh: instantiated/.test(text)],
  ["eosh ran hello (greeting)", /Hello, web/.test(text)],
  ["hello outcome greeted", /greeted/.test(text)],
  ["eosh command rc == 0", cmdRc === 0],
];
let ok = true;
for (const [label, pass] of checks) {
  if (!pass) ok = false;
  console.log(`  ${pass ? "ok" : "FAIL"}: ${label}`);
}
console.log(`\neosh boot: instantiate rc=${instRc}, command rc=${cmdRc} -> ${ok ? "PASS" : "FAIL"}`);
process.exit(ok ? 0 : 1);
