// Node (v25, JSPI) verification harness for the /vm blob's fs/io providers, mirroring
// www/site/vm/vm.js's import glue. Not part of CI (needs node + JSPI); run after
// `cargo xtask build-web-vm`:  node www/web-eo9/verify-fs.mjs
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const vmDir = join(here, "..", "site", "vm");
const assets = JSON.parse(readFileSync(join(vmDir, "assets.json"), "utf8"));
const blobPath = join(vmDir, assets.blob.replace(/^\/vm\//, ""));
const storePath = (name) =>
  join(vmDir, (assets.store[name] || `/vm/store/${name}.cwasm`).replace(/^\/vm\//, ""));

const decoder = new TextDecoder();
const encoder = new TextEncoder();
let memory = null;
let fetched = null;
// Output arrives as raw terminal chunks (newlines included — the page interprets the
// stream; see vm.js); accumulate and split on newlines, stripping the per-chunk U+0001
// stderr marker.
let raw = "";

const imports = {
  env: {
    host_write: (ptr, len) => {
      raw += decoder.decode(new Uint8Array(memory.buffer, ptr, len)).replace(/^\u0001/, "");
    },
    host_now_ms: () => Date.now(),
    host_monotonic_ns: () => performance.now() * 1e6,
    host_random_fill: (ptr, len) => {
      const view = new Uint8Array(memory.buffer, ptr, len);
      for (let i = 0; i < len; i++) view[i] = (Math.random() * 256) | 0;
    },
    host_fetch_copy: (destPtr, len) => {
      if (fetched) new Uint8Array(memory.buffer, destPtr, len).set(fetched.subarray(0, len));
      fetched = null;
    },
    // The page-canvas blit: display-only (readback comes from the blob backing); no-op here.
    host_gfx_present: () => {},
    host_sleep_ms: new WebAssembly.Suspending((ms) => new Promise((r) => setTimeout(r, ms))),
    host_read_line: new WebAssembly.Suspending(async () => -1),
    // This harness is a line-based transport: -3 sends eosh down its read-line path.
    host_read_key: new WebAssembly.Suspending(async () => -3),
    host_fetch_len: new WebAssembly.Suspending(async (namePtr, nameLen) => {
      const name = decoder.decode(new Uint8Array(memory.buffer, namePtr, nameLen));
      try {
        fetched = new Uint8Array(readFileSync(storePath(name)));
        return fetched.length;
      } catch {
        return -1;
      }
    }),
    host_compile_copy: () => {},
    host_compile_len: new WebAssembly.Suspending(async () => -1),
  },
};

const { instance } = await WebAssembly.instantiate(readFileSync(blobPath), imports);
const x = instance.exports;
memory = x.memory;

const into = (text) => {
  const bytes = encoder.encode(text);
  if (bytes.length === 0) return [0, 0];
  const ptr = x.web_alloc(bytes.length);
  new Uint8Array(memory.buffer, ptr, bytes.length).set(bytes);
  return [ptr, bytes.length];
};
const runProgram = WebAssembly.promising(x.run_program);

if (x.boot() !== 0) throw new Error("boot failed");
const [nP, nL] = into("readwrite");
const [aP, aL] = into(["/scratch/note.txt", "hello disk"].join(""));
const rc = await runProgram(nP, nL, aP, aL);
x.web_free(nP, nL);
x.web_free(aP, aL);

console.log(raw);
const ok = rc === 0 && /round-tripped\(10\)/.test(raw);
console.log(`\nreadwrite rc=${rc} -> ${ok ? "PASS" : "FAIL"}`);
process.exit(ok ? 0 : 1);
