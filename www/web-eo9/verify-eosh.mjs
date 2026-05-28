// Node (v25, JSPI) verification harness for booting eosh in the /vm blob, mirroring
// www/site/vm/vm.js's import glue. Not part of CI (needs node + JSPI); run after
// `cargo xtask build-web-vm` and `cargo build --release -p eo9-www`:
//   node www/web-eo9/verify-eosh.mjs
//
// This harness also exercises the real server round-trip: it spawns the eo9-www server in
// plain-HTTP mode and points the blob's host_compile_len at its POST /vm/compile endpoint, so
// `entropy.seeded $ rng` is genuinely fused + compiled on the server and run in the browser blob.
import { readFileSync, existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { spawn } from "node:child_process";

const here = dirname(fileURLToPath(import.meta.url));
const vmDir = join(here, "..", "site", "vm");
const siteDir = join(here, "..", "site");
const assets = JSON.parse(readFileSync(join(vmDir, "assets.json"), "utf8"));
const blobPath =
  process.env.BLOB || join(vmDir, assets.blob.replace(/^\/vm\//, ""));

// --- spawn the real server (plain HTTP) so /vm/compile is a genuine network round-trip ------
const serverBin =
  process.env.EO9_WWW_BIN || join(here, "..", "target", "release", "eo9-www");
if (!existsSync(serverBin)) {
  console.error(
    `eo9-www server binary not found at ${serverBin}\n` +
      "build it first:  cargo build --release -p eo9-www",
  );
  process.exit(2);
}
const port = process.env.EO9_WWW_PORT || "38099";
const base = `http://127.0.0.1:${port}`;
const server = spawn(serverBin, ["--site", siteDir, "--bind", `127.0.0.1:${port}`], {
  stdio: "inherit",
});

function finish(code) {
  server.kill("SIGTERM");
  process.exit(code);
}

// Wait for the server to accept requests.
let ready = false;
for (let i = 0; i < 100; i++) {
  try {
    const r = await fetch(`${base}/vm/assets.json`);
    if (r.ok) {
      ready = true;
      break;
    }
  } catch {
    // not up yet
  }
  await new Promise((r) => setTimeout(r, 100));
}
if (!ready) {
  console.error(`server did not become ready at ${base}`);
  finish(2);
}

const decoder = new TextDecoder();
const encoder = new TextEncoder();
let memory = null;
const lines = [];
let inputQueue = []; // command lines fed to the interactive eosh prompt via read-line
let compiledImage = null; // most recent server-compiled image, copied by host_compile_copy

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
    host_compile_copy: (destPtr, len) => {
      if (compiledImage === null) return;
      new Uint8Array(memory.buffer, destPtr, len).set(compiledImage.subarray(0, len));
      compiledImage = null;
    },
    host_sleep_ms: new WebAssembly.Suspending((ms) => new Promise((r) => setTimeout(r, ms))),
    host_read_line: new WebAssembly.Suspending(async (ptr, cap) => {
      if (inputQueue.length === 0) return -1; // end of input
      const bytes = encoder.encode(inputQueue.shift());
      if (bytes.length > cap) return -1;
      new Uint8Array(memory.buffer, ptr, bytes.length).set(bytes);
      return bytes.length;
    }),
    host_fetch_len: new WebAssembly.Suspending(async () => -1),
    // The real round-trip: POST the composition expression to the server, which fuses the
    // named store programs with the algebra and compiles a pulley32 image.
    host_compile_len: new WebAssembly.Suspending(async (exprPtr, exprLen) => {
      const expr = decoder.decode(new Uint8Array(memory.buffer, exprPtr, exprLen));
      try {
        const r = await fetch(`${base}/vm/compile`, {
          method: "POST",
          headers: { "content-type": "text/plain" },
          body: expr,
        });
        if (!r.ok) return -1;
        compiledImage = new Uint8Array(await r.arrayBuffer());
        return compiledImage.length;
      } catch {
        return -1;
      }
    }),
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
  "cat --path /welcome.txt",
  "ls --path /",
  // only-attenuation: the gate admitting exactly hello's needs runs it (restricted linker
  // serves only text+time); a text-only program runs under only-text (served only text);
  // dropping a required capability is refused (restrict: required-outside-allow).
  "only eo9:text/text,eo9:time/time $ hello --name boxed --excited true",
  "only eo9:text/text $ echo --text restricted",
  "only eo9:text/text $ hello --name nope --excited true",
  // a `provider $ consumer` composition: entropy.seeded (a /bin provider) feeds rng. The fused
  // result has no in-blob artifact, so the blob POSTs `entropy.seeded $ rng` to the server,
  // which compiles it; the blob runs the returned image. Run twice (marker between) to show the
  // seeded provider makes it deterministic across runs.
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
  ["interactive: cat read /welcome.txt", /printed\(/.test(interactive)],
  ["interactive: ls listed /", /listed\(/.test(interactive)],
  ["only admitting text+time runs hello", /Hello, boxed/.test(interactive)],
  ["only-text runs a text-only program", /restricted/.test(interactive)],
  ["only-text refuses hello (needs time)", !/Hello, nope/.test(interactive)],
  // The composition was NOT refused — it compiled on the server and ran (3 rng numbers).
  ["server compiled+ran entropy.seeded $ rng", run1.length === 3],
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
