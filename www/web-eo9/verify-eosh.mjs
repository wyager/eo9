// Node (v25, JSPI) verification harness for booting eosh in the /vm blob, mirroring
// www/site/vm/vm.js's import glue. Not part of CI (needs node + JSPI); run after
// `cargo xtask build-web-vm`:
//   node www/web-eo9/verify-eosh.mjs
//
// Everything here is offline: there is no server and no network import — compositions typed at
// the eosh prompt are fused by the algebra and compiled *inside the blob* (Cranelift -> Pulley),
// so a passing run is direct proof that compose -> compile -> run needs no server involvement.
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const vmDir = join(here, "..", "site", "vm");
const assets = JSON.parse(readFileSync(join(vmDir, "assets.json"), "utf8"));
const blobPath =
  process.env.BLOB || join(vmDir, assets.blob.replace(/^\/vm\//, ""));

function finish(code) {
  process.exit(code);
}

const decoder = new TextDecoder();
const encoder = new TextEncoder();
let memory = null;
const lines = [];
let inputQueue = []; // command lines fed to the interactive eosh prompt via read-line

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
    host_read_line: new WebAssembly.Suspending(async (ptr, cap) => {
      if (inputQueue.length === 0) return -1; // end of input
      const bytes = encoder.encode(inputQueue.shift());
      if (bytes.length > cap) return -1;
      new Uint8Array(memory.buffer, ptr, bytes.length).set(bytes);
      return bytes.length;
    }),
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

if (x.boot() !== 0) {
  console.error("boot failed");
  finish(1);
}

// Sub-step 1 (the floor): eosh instantiates against the in-blob exec/text/fs surface.
const eoshInstantiate = WebAssembly.promising(x.eosh_instantiate);
const instRc = await eoshInstantiate();

// Sub-step 2: eosh runs a command one-shot.
const eoshCommand = WebAssembly.promising(x.eosh_command);
const [ptr, len] = passStr("hello --name web --excited true");
const cmdRc = await eoshCommand(ptr, len);
x.web_free(ptr, len);

const oneShot = lines.join("\n");

// Sub-step 3: the interactive `eosh>` prompt — feed command lines through read-line (JSPI),
// the same path the page terminal uses. eosh runs each and reads the next until EOF/exit.
inputQueue = [
  "echo --text hi",
  // Positional + variadic arguments: the path-taking coreutils take a trailing
  // list<string>, so cat reads two files and a *bare* ls works (missing tail -> []).
  "cat /welcome.txt /docs/about.txt",
  "ls",
  // only-attenuation: the gate admitting exactly hello's needs runs it (restricted linker
  // serves only text+time); a text-only program runs under only-text (served only text);
  // dropping a required capability is refused (restrict: required-outside-allow).
  "only eo9:text/text,eo9:time/time $ hello --name boxed --excited true",
  "only eo9:text/text $ echo --text restricted",
  "only eo9:text $ echo --text shorthand",
  "only eo9:text/text $ hello --name nope --excited true",
  // a `provider $ consumer` composition: entropy.seeded (a /bin provider) feeds rng. The fused
  // result has no pre-AOT'd artifact, so the blob compiles it *in-blob* (Cranelift -> Pulley) and
  // runs it — no server, no network (none is wired in this harness). Run twice (marker between)
  // to show the seeded provider makes it deterministic across runs.
  "entropy.seeded $ rng --count 3",
  "echo --text RNGMARK",
  "entropy.seeded $ rng --count 3",
  "exit",
];
lines.length = 0;
const eoshBoot = WebAssembly.promising(x.eosh_boot);
const bootRc = await eoshBoot();
const interactive = lines.join("\n");

console.log("--- one-shot ---\n" + oneShot + "\n--- interactive ---\n" + interactive);

// Pure-digit lines come only from rng (u64 per line); split on the marker to separate the two
// runs and compare them for determinism.
const parts = interactive.split("RNGMARK");
const run1 = (parts[0]?.match(/^\d{3,}$/gm) || []).slice(-3);
const run2 = (parts[1]?.match(/^\d{3,}$/gm) || []).slice(0, 3);
const deterministic =
  run1.length === 3 && run2.length === 3 && run1.join(",") === run2.join(",");

const checks = [
  ["eosh instantiates (floor)", instRc === 0 && /eosh: instantiated/.test(oneShot)],
  ["eosh ran hello (greeting)", /Hello, web/.test(oneShot)],
  ["hello outcome greeted", /greeted/.test(oneShot)],
  ["eosh command rc == 0", cmdRc === 0],
  ["interactive: echo printed hi", /\bhi\b/.test(interactive)],
  [
    "interactive: cat read two positional paths",
    /Hello from the Eo9 web VM filesystem/.test(interactive) &&
      /capability-secure OS/.test(interactive) &&
      /printed\(/.test(interactive),
  ],
  [
    "interactive: bare ls listed / (empty-tail default)",
    /welcome\.txt/.test(interactive) && /listed\(/.test(interactive),
  ],
  ["only admitting text+time runs hello", /Hello, boxed/.test(interactive)],
  ["only-text runs a text-only program", /restricted/.test(interactive)],
  ["only with the package shorthand (eo9:text) runs echo", /shorthand/.test(interactive)],
  ["only-text refuses hello (needs time)", !/Hello, nope/.test(interactive)],
  // The composition was NOT refused — it compiled inside the blob and ran (3 rng numbers),
  // with no server reachable from this harness at all.
  ["in-blob compiled+ran entropy.seeded $ rng (no server)", run1.length === 3],
  ["compose did not hit the codegen refusal", !/needs the compiler/.test(interactive)],
  ["seeded compose is deterministic across runs", deterministic],
  ["interactive: session exited", bootRc === 0 && /success\(exited\)/.test(interactive)],
];
let ok = true;
for (const [label, pass] of checks) {
  if (!pass) ok = false;
  console.log(`  ${pass ? "ok" : "FAIL"}: ${label}`);
}
console.log(
  `\neosh boot: instantiate rc=${instRc}, command rc=${cmdRc}, interactive rc=${bootRc} -> ${ok ? "PASS" : "FAIL"}`,
);
finish(ok ? 0 : 1);
