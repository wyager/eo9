// The /vm self-test, externalized so the site's CSP (script-src 'self') applies to it
// like every other page script. Loaded only by selftest.html.
const terminal = document.getElementById("terminal");
const results = document.getElementById("results");
const verdict = document.getElementById("verdict");

const decoder = new TextDecoder();
const encoder = new TextEncoder();
let memory = null;
let exports = null;
let lines = [];

const hasJSPI =
  typeof WebAssembly.Suspending === "function" && typeof WebAssembly.promising === "function";

function note(line) {
  results.textContent += line + "\n";
}

function hostWrite(ptr, len) {
  const text = decoder.decode(new Uint8Array(memory.buffer, ptr, len));
  lines.push(text);
  terminal.textContent += text + "\n";
}

// Resolve fingerprinted asset URLs via /vm/assets.json (falls back to canonical names).
let assetMap = null;
async function loadAssetMap() {
  if (assetMap) return assetMap;
  try {
    const response = await fetch("/vm/assets.json", { cache: "no-cache" });
    if (response.ok) {
      assetMap = await response.json();
      return assetMap;
    }
  } catch {
    /* fall through */
  }
  assetMap = { blob: "/vm/web-eo9.wasm", store: {} };
  return assetMap;
}
const blobUrl = () => (assetMap && assetMap.blob) || "/vm/web-eo9.wasm";
const storeUrl = (name) =>
  (assetMap && assetMap.store && assetMap.store[name]) || `/vm/store/${name}.cwasm`;

let fetchedArtifact = null;
async function hostFetchLen(namePtr, nameLen) {
  const name = decoder.decode(new Uint8Array(memory.buffer, namePtr, nameLen));
  try {
    const response = await fetch(storeUrl(name));
    if (!response.ok) return -1;
    fetchedArtifact = new Uint8Array(await response.arrayBuffer());
    return fetchedArtifact.length;
  } catch {
    return -1;
  }
}

const imports = {
  env: {
    host_write: hostWrite,
    host_now_ms: () => Date.now(),
    host_monotonic_ns: () => performance.now() * 1e6,
    host_random_fill: (ptr, len) => {
      let offset = 0;
      while (offset < len) {
        const chunk = Math.min(len - offset, 65536);
        crypto.getRandomValues(new Uint8Array(memory.buffer, ptr + offset, chunk));
        offset += chunk;
      }
    },
    host_fetch_copy: (destPtr, len) => {
      if (fetchedArtifact === null) return;
      new Uint8Array(memory.buffer, destPtr, len).set(fetchedArtifact.subarray(0, len));
      fetchedArtifact = null;
    },
    host_sleep_ms: hasJSPI
      ? new WebAssembly.Suspending(async (ms) => {
          await new Promise((resolve) => setTimeout(resolve, ms));
        })
      : () => {},
    // The self-test never reads interactively; report end-of-input.
    host_read_line: hasJSPI
      ? new WebAssembly.Suspending(async () => -1)
      : () => -2,
    host_fetch_len: hasJSPI ? new WebAssembly.Suspending(hostFetchLen) : () => -2,
  },
};

function intoBlob(text) {
  const bytes = encoder.encode(text);
  if (bytes.length === 0) return [0, 0];
  const ptr = exports.web_alloc(bytes.length);
  new Uint8Array(memory.buffer, ptr, bytes.length).set(bytes);
  return [ptr, bytes.length];
}

function sawLine(pattern) {
  return lines.some((line) => pattern.test(line));
}

let failures = 0;
function check(name, condition) {
  note(`${condition ? "ok " : "FAIL"} ${name}`);
  if (!condition) failures += 1;
}

async function run() {
  note(`jspi ${hasJSPI ? "available" : "MISSING"}`);
  await loadAssetMap();
  const response = await fetch(blobUrl());
  const { instance } = await WebAssembly.instantiateStreaming(response, imports);
  exports = instance.exports;
  memory = exports.memory;

  check("boot", exports.boot() === 0);

  lines = [];
  check("run_hello rc", exports.run_hello() === 0);
  check("run_hello greeting", sawLine(/Hello from a WebAssembly component/));
  check("run_hello add", sawLine(/add\(17, 25\) -> 42/));

  lines = [];
  check("run_fuel rc", exports.run_fuel() === 0);
  check("run_fuel metered", sawLine(/fuel metered/));

  lines = [];
  check("run_entropy rc", exports.run_entropy(0xe09, 0, 2) === 0);
  check("run_entropy first draw", sawLine(/0x505f147c387507b6/));
  check("run_entropy second draw", sawLine(/0xe2e264775fe9be54/));

  if (!hasJSPI) {
    note("skipping store/sleepy checks: no JSPI in this browser");
  } else {
    const promising = (fn) => WebAssembly.promising(fn);
    const runProgram = promising(exports.run_program);
    const runSleepy = promising(exports.run_sleepy);

    const runStored = async (name, args) => {
      const [namePtr, nameLen] = intoBlob(name);
      const [argsPtr, argsLen] = intoBlob(args.join("\u001f"));
      try {
        return await runProgram(namePtr, nameLen, argsPtr, argsLen);
      } finally {
        if (nameLen) exports.web_free(namePtr, nameLen);
        if (argsLen) exports.web_free(argsPtr, argsLen);
      }
    };

    lines = [];
    check("store hello rc", (await runStored("hello", ["selftest", "true"])) === 0);
    check("store hello output", sawLine(/Hello, selftest!/));
    check("store hello outcome", sawLine(/outcome = success\(greeted\)/));

    lines = [];
    check("store cruncher rc", (await runStored("cruncher", ["9", "200000"])) === 0);
    check("store cruncher digest", sawLine(/success\(digest\(14341732361190694547\)\)/));

    lines = [];
    check("store outcomes rc (typed failure)", (await runStored("outcomes", ["fail", "sad path"])) === 0);
    check("store outcomes failure", sawLine(/failure\(requested-failure\("sad path"\)\)/));

    lines = [];
    const before = performance.now();
    const parkRc = await WebAssembly.promising(exports.probe_sleep)(300);
    const elapsed = performance.now() - before;
    check("park rc", parkRc === 0);
    check("park page elapsed >= 300 ms", elapsed >= 295);
    note(`park page-side elapsed: ${elapsed.toFixed(1)} ms`);

    lines = [];
    const sleepyRc = await runSleepy();
    check("sleepy reports the stackful-lift limitation honestly",
      sleepyRc !== 0 && sawLine(/stackful/));
  }

  verdict.textContent = failures === 0 ? "PASS" : `FAIL (${failures})`;
  document.title = `eo9-selftest-${verdict.textContent}`;
}

run().catch((error) => {
  note(`unhandled error: ${error}`);
  verdict.textContent = "FAIL (exception)";
  document.title = "eo9-selftest-FAIL";
});
