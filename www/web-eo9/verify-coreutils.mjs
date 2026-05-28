// Node (v25, JSPI) verification harness for the coreutils running on the /vm blob against
// the blob's in-memory eo9:fs (seeded with a sample tree). Mirrors www/site/vm/vm.js's
// import glue. Not part of CI (needs node + JSPI); run after `cargo xtask build-web-vm`:
//   node www/web-eo9/verify-coreutils.mjs
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
const US = "\u001f";
let memory = null;
let fetched = null;
let lines = [];

const imports = {
  env: {
    host_write: (ptr, len) => lines.push(decoder.decode(new Uint8Array(memory.buffer, ptr, len))),
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
    host_sleep_ms: new WebAssembly.Suspending((ms) => new Promise((r) => setTimeout(r, ms))),
    host_read_line: new WebAssembly.Suspending(async () => -1),
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
const runProgram = WebAssembly.promising(x.run_program);
if (x.boot() !== 0) throw new Error("boot failed");

const into = (text) => {
  const bytes = encoder.encode(text);
  if (bytes.length === 0) return [0, 0];
  const ptr = x.web_alloc(bytes.length);
  new Uint8Array(memory.buffer, ptr, bytes.length).set(bytes);
  return [ptr, bytes.length];
};

async function run(name, args) {
  lines = [];
  const [nP, nL] = into(name);
  const [aP, aL] = into(args.join(US));
  const rc = await runProgram(nP, nL, aP, aL);
  x.web_free(nP, nL);
  if (aL) x.web_free(aP, aL);
  return { rc, out: lines.join("\n") };
}

// (name, args, predicate over the combined output)
const cases = [
  ["echo", ["hello from the web VM"], (o) => /hello from the web VM/.test(o)],
  ["rng", ["5"], (o) => (o.match(/\d{2,}/g) || []).length >= 5],
  ["cat", ["/welcome.txt"], (o) => /Hello from the Eo9 web VM filesystem/.test(o)],
  ["ls", ["/"], (o) => /welcome\.txt/.test(o) && /docs/.test(o)],
  ["wc", ["/welcome.txt"], (o) => /success/.test(o)],
  ["stat", ["/welcome.txt"], (o) => /success/.test(o)],
  ["head", ["/docs/notes.txt", "2"], (o) => /line one/.test(o) && /line two/.test(o) && !/line three/.test(o)],
  ["cp", ["/welcome.txt", "/docs/copy.txt"], (o) => /success/.test(o)],
  ["mkdir", ["/scratch"], (o) => /success/.test(o)],
  ["touch", ["/scratch/empty"], (o) => /success/.test(o)],
  ["rm", ["/docs/notes.txt"], (o) => /success/.test(o)],
  ["find", ["/", ".txt"], (o) => /welcome\.txt/.test(o)],
];

let pass = 0;
for (const [name, args, ok] of cases) {
  const { rc, out } = await run(name, args);
  const good = rc === 0 && ok(out);
  if (good) pass++;
  const first = out.split("\n").filter((l) => /outcome|->|success|failure|error/.test(l)).slice(-1)[0] || out.split("\n")[0] || "";
  console.log(`${good ? "PASS" : "FAIL"}  ${name} ${args.join(" ")}  ::  ${first.slice(0, 90)}`);
  if (!good) console.log(out);
}
console.log(`\ncoreutils: ${pass}/${cases.length} PASS`);
process.exit(pass === cases.length ? 0 : 1);
