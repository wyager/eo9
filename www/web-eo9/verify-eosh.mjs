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
  // The try-it page's "Explore the sandbox" loop: help, ls /bin, describe (one binary, one
  // provider), env — then "compose what you learned" runs at the end (after EXPLOREMARK).
  "help",
  "ls /bin",
  "describe hello",
  "describe entropy.seeded",
  // `env`: the blob seeds a session manifest at /session (the eo9-session 1 format), so
  // the builtin reports the page's actual grants instead of "no information available".
  "env",
  // Optional arguments: a bare `hello` greets the default name; --name still overrides.
  "hello",
  "hello --name user",
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
  // The page's bare-default forms of the same lockdowns: the pass case greets the default
  // name, the refusal case must not add a greeting (the Hello-world count check below).
  "only eo9:text/text,eo9:time/time $ hello",
  "only eo9:text/text $ hello",
  // describe of a composition: the new `wiring` exec function renders the composition tree
  // (the interposed provider is visible), the same view `describe --wiring` gives natively.
  "describe entropy.seeded $ rng",
  // a virtualized clock: time.frozen (a /bin provider) is configured to the epoch and sealed
  // over hello, so the timestamp it prints is exactly 0.000000000 — every run, in any browser.
  "time.frozen --now-seconds 0 --monotonic-ns 0 $ hello --name frozen --excited true",
  // the page's bare form of the same composition (hello's defaults).
  "time.frozen --now-seconds 0 --monotonic-ns 0 $ hello",
  // a `provider $ consumer` composition: entropy.seeded (a /bin provider) feeds rng. The fused
  // result has no pre-AOT'd artifact, so the blob compiles it *in-blob* (Cranelift -> Pulley) and
  // runs it — no server, no network (none is wired in this harness). Run twice (marker between)
  // to show the seeded provider makes it deterministic across runs.
  "entropy.seeded $ rng --count 3",
  "echo --text RNGMARK",
  "entropy.seeded $ rng --count 3",
  // the `&` form (extend/shadow): the right operand wins where exports overlap, so layering a
  // *configured* entropy.seeded over the default-configured one must change the stream rng sees.
  // The fused three-op result (configure + extend + compose) is also compiled in-blob.
  "echo --text AMPMARK",
  "entropy.seeded & entropy.seeded --seed 7 $ rng --count 2",
  // The explore section's "compose what you learned" example.
  "echo --text EXPLOREMARK",
  "entropy.seeded --seed 7 $ rng --count 2",
  "exit",
];
lines.length = 0;
const eoshBoot = WebAssembly.promising(x.eosh_boot);
const bootRc = await eoshBoot();
const interactive = lines.join("\n");

console.log("--- one-shot ---\n" + oneShot + "\n--- interactive ---\n" + interactive);

// Pure-digit lines come only from rng (u64 per line); split on the markers to separate the
// runs: two identical default-seeded `$` runs (determinism), then the `&` run with a
// re-configured right layer (must differ from the default stream).
const [beforeAmp, ampPart] = interactive.split("AMPMARK");
const parts = beforeAmp.split("RNGMARK");
const run1 = (parts[0]?.match(/^\d{3,}$/gm) || []).slice(-3);
const run2 = (parts[1]?.match(/^\d{3,}$/gm) || []).slice(0, 3);
const deterministic =
  run1.length === 3 && run2.length === 3 && run1.join(",") === run2.join(",");
const [ampOnly, explorePart] = (ampPart || "").split("EXPLOREMARK");
const ampRun = (ampOnly?.match(/^\d{3,}$/gm) || []).slice(0, 2);
const exploreRun = (explorePart?.match(/^\d{3,}$/gm) || []).slice(0, 2);
// Bare `hello` (default name), the bare only-pass form, and the bare frozen-clock form each
// greet "world" exactly once; the bare only-refusal must not add a fourth.
const helloWorldCount = (interactive.match(/Hello, world/g) || []).length;

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
  [
    "help lists the operators and builtins",
    /compose: satisfy the program's imports/.test(interactive) &&
      /builtins: help, env/.test(interactive),
  ],
  ["ls /bin lists the program store", /hello\.wasm/.test(interactive)],
  [
    "describe hello: a binary with optional typed arguments",
    /kind: binary/.test(interactive) && /--name: option<string>/.test(interactive),
  ],
  [
    "describe entropy.seeded: a provider with its configure argument",
    /kind: provider/.test(interactive) && /--seed: u64/.test(interactive),
  ],
  [
    "env reports the browser session's capabilities",
    /capabilities granted to this shell:/.test(interactive) &&
      /the page terminal/.test(interactive) &&
      /programs started from this shell receive:/.test(interactive),
  ],
  ["bare hello greets the default name", helloWorldCount >= 1],
  ["hello --name user overrides the default", /Hello, user/.test(interactive)],
  [
    "bare lockdown/frozen forms ran (and the bare refusal did not)",
    helloWorldCount === 3 && /\[0\.000000000\] Hello, world\./.test(interactive),
  ],
  ["explore composition (seed 7 over rng) ran with 2 values", exploreRun.length === 2],
  ["only admitting text+time runs hello", /Hello, boxed/.test(interactive)],
  ["only-text runs a text-only program", /restricted/.test(interactive)],
  ["only with the package shorthand (eo9:text) runs echo", /shorthand/.test(interactive)],
  ["only-text refuses hello (needs time)", !/Hello, nope/.test(interactive)],
  [
    "describe shows the wiring tree (compose node, provider and consumer layers)",
    /wiring:/.test(interactive) &&
      /\$ compose/.test(interactive) &&
      /provider:/.test(interactive) &&
      /consumer:/.test(interactive),
  ],
  [
    "frozen clock: time.frozen sealed over hello prints the configured instant",
    /\[0\.000000000\] Hello, frozen!/.test(interactive) || /\[0\.0+\] Hello, frozen/.test(interactive),
  ],
  // The composition was NOT refused — it compiled inside the blob and ran (3 rng numbers),
  // with no server reachable from this harness at all.
  ["in-blob compiled+ran entropy.seeded $ rng (no server)", run1.length === 3],
  ["compose did not hit the codegen refusal", !/needs the compiler/.test(interactive)],
  ["seeded compose is deterministic across runs", deterministic],
  ["in-blob compiled+ran the & form (configure + extend + compose)", ampRun.length === 2],
  [
    "& is right-biased: the --seed 7 layer shadows the default-seeded one",
    ampRun.length === 2 && run1.length === 3 && ampRun[0] !== run1[0],
  ],
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
