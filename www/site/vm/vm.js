// The Eo9 web VM page: load the blob (wasmtime + Pulley, compiled to wasm32) and wire its
// import surface to the page. Plain imports: terminal output, clocks, randomness. The
// genuinely-blocking imports (sleep, read-line, fetch-from-store) are JSPI
// `WebAssembly.Suspending` functions: the blob calls them synchronously, the browser parks
// the whole blob on the underlying promise (a timer, the visitor's Enter key, a fetch), and
// resumes it with the result. Everything Eo9-shaped happens inside the blob; this file is
// just a terminal, a keyboard, and a fetch cache.

const output = document.getElementById("vm-output");
const buttons = {
  hello: document.getElementById("btn-hello"),
  fuel: document.getElementById("btn-fuel"),
  entropy: document.getElementById("btn-entropy"),
  program: document.getElementById("btn-program"),
  sleepy: document.getElementById("btn-sleepy"),
  park: document.getElementById("btn-park"),
  readline: document.getElementById("btn-readline"),
};
const seedInput = document.getElementById("seed");
const countInput = document.getElementById("count");
const programSelect = document.getElementById("program");
const programArgs = document.getElementById("program-args");
const terminalInput = document.getElementById("vm-input");

const ARG_PLACEHOLDERS = {
  hello: "browser true",
  cruncher: "9 200000",
  outcomes: 'fail "sad path"',
};

// Argument fields are joined with the ASCII unit separator before crossing into the blob.
const FIELD_SEPARATOR = "\u001f";

let memory = null;
let exportsRef = null;
const decoder = new TextDecoder();
const encoder = new TextEncoder();

const hasJSPI =
  typeof WebAssembly.Suspending === "function" && typeof WebAssembly.promising === "function";

function writeLine(text, cls) {
  const line = document.createElement("div");
  if (cls) line.className = cls;
  line.textContent = text;
  output.appendChild(line);
  output.scrollTop = output.scrollHeight;
}

// --- plain imports -------------------------------------------------------------------------

function hostWrite(ptr, len) {
  const bytes = new Uint8Array(memory.buffer, ptr, len);
  writeLine(decoder.decode(bytes));
}

function hostNowMs() {
  return Date.now();
}

function hostMonotonicNs() {
  return performance.now() * 1e6;
}

function hostRandomFill(ptr, len) {
  // crypto.getRandomValues caps one call at 64 KiB; chunk to stay under it.
  let offset = 0;
  while (offset < len) {
    const chunk = Math.min(len - offset, 65536);
    crypto.getRandomValues(new Uint8Array(memory.buffer, ptr + offset, chunk));
    offset += chunk;
  }
}

// --- suspending imports (JSPI) ----------------------------------------------------------------

// One pending read-line at a time: the input box resolves it on Enter.
let pendingReadLine = null;

function armReadLine() {
  terminalInput.disabled = false;
  terminalInput.focus();
  return new Promise((resolve) => {
    pendingReadLine = resolve;
  });
}

terminalInput.addEventListener("keydown", (event) => {
  if (event.key !== "Enter" || pendingReadLine === null) return;
  const line = terminalInput.value;
  terminalInput.value = "";
  terminalInput.disabled = true;
  const resolve = pendingReadLine;
  pendingReadLine = null;
  writeLine(`> ${line}`, "vm-cmd");
  resolve(line);
});

async function hostSleepMs(ms) {
  await new Promise((resolve) => setTimeout(resolve, ms));
}

async function hostReadLine(ptr, cap) {
  const line = await armReadLine();
  const bytes = encoder.encode(line);
  const len = Math.min(bytes.length, cap);
  new Uint8Array(memory.buffer, ptr, len).set(bytes.subarray(0, len));
  return len;
}

// The most recent store fetch, copied into the blob by host_fetch_copy.
let fetchedArtifact = null;

async function hostFetchLen(namePtr, nameLen) {
  const name = decoder.decode(new Uint8Array(memory.buffer, namePtr, nameLen));
  if (!/^[a-z0-9-]{1,64}$/.test(name)) return -1;
  try {
    const response = await fetch(`/vm/store/${name}.cwasm`);
    if (!response.ok) return -1;
    fetchedArtifact = new Uint8Array(await response.arrayBuffer());
    return fetchedArtifact.length;
  } catch {
    return -1;
  }
}

function hostFetchCopy(destPtr, len) {
  if (fetchedArtifact === null) return;
  new Uint8Array(memory.buffer, destPtr, len).set(fetchedArtifact.subarray(0, len));
  fetchedArtifact = null;
}

// Fallbacks when the browser has no JSPI: report "unavailable" so the blob errors cleanly
// (the page also disables the affected buttons and says why).
function unavailableSleep() {}
function unavailableReadLine() {
  return -2;
}
function unavailableFetchLen() {
  return -2;
}

// --- helpers ----------------------------------------------------------------------------------

function parseSeed(text) {
  const trimmed = text.trim();
  let value;
  try {
    value = BigInt(trimmed);
  } catch {
    return null;
  }
  if (value < 0n || value > 0xffffffffffffffffn) return null;
  return value;
}

// Split an args string into fields (double quotes group words, e.g. `fail "sad path"`).
function splitArgs(text) {
  const fields = [];
  const pattern = /"([^"]*)"|(\S+)/g;
  let match;
  while ((match = pattern.exec(text)) !== null) {
    fields.push(match[1] !== undefined ? match[1] : match[2]);
  }
  return fields;
}

// Write a JS string into blob memory via web_alloc; returns [ptr, len].
function intoBlob(text) {
  const bytes = encoder.encode(text);
  if (bytes.length === 0) return [0, 0];
  const ptr = exportsRef.web_alloc(bytes.length);
  new Uint8Array(memory.buffer, ptr, bytes.length).set(bytes);
  return [ptr, bytes.length];
}

async function main() {
  let exports;
  const imports = {
    env: {
      host_write: hostWrite,
      host_now_ms: hostNowMs,
      host_monotonic_ns: hostMonotonicNs,
      host_random_fill: hostRandomFill,
      host_fetch_copy: hostFetchCopy,
      host_sleep_ms: hasJSPI ? new WebAssembly.Suspending(hostSleepMs) : unavailableSleep,
      host_read_line: hasJSPI ? new WebAssembly.Suspending(hostReadLine) : unavailableReadLine,
      host_fetch_len: hasJSPI ? new WebAssembly.Suspending(hostFetchLen) : unavailableFetchLen,
    },
  };
  try {
    if (typeof WebAssembly !== "object") {
      throw new Error("this browser has no WebAssembly support");
    }
    const response = await fetch("/vm/web-eo9.wasm");
    if (!response.ok) {
      throw new Error(`fetching /vm/web-eo9.wasm failed: HTTP ${response.status}`);
    }
    // Prefer streaming compilation; fall back to buffering the bytes if the engine refuses
    // (older engines, or a misconfigured Content-Type on the response).
    let result = null;
    if (typeof WebAssembly.instantiateStreaming === "function") {
      try {
        result = await WebAssembly.instantiateStreaming(response.clone(), imports);
      } catch {
        result = null;
      }
    }
    if (result === null) {
      result = await WebAssembly.instantiate(await response.arrayBuffer(), imports);
    }
    exports = result.instance.exports;
    exportsRef = exports;
    memory = exports.memory;
  } catch (error) {
    // Report the actual cause; don't blame missing WebAssembly support for a network or
    // server problem.
    output.textContent = "";
    writeLine(`could not load the Eo9 blob: ${error}`, "vm-error");
    if (typeof WebAssembly !== "object") {
      writeLine("(this page needs a browser with WebAssembly enabled)", "vm-error");
    } else {
      writeLine(
        "(the message above is the real cause — usually the blob failed to download; the browser console has details)",
        "vm-error",
      );
    }
    return;
  }

  output.textContent = "";
  const failures = exports.boot();
  if (failures !== 0) {
    writeLine("boot reported a failure — see above", "vm-error");
    return;
  }
  if (!hasJSPI) {
    writeLine(
      "this browser has no JavaScript Promise Integration (JSPI), so the demos that genuinely " +
        "block — the program store, sleepy, and read-line — are disabled; hello / fuel / " +
        "entropy still work. Current Chrome or Edge has JSPI.",
      "vm-error",
    );
  }

  // Synchronous exports are called directly; the ones that may suspend (they call JSPI
  // imports) must be wrapped with WebAssembly.promising and awaited.
  const promising = (fn) => (hasJSPI ? WebAssembly.promising(fn) : fn);
  const runSleepy = promising(exports.run_sleepy);
  const probeSleep = promising(exports.probe_sleep);
  const probeReadLine = promising(exports.probe_read_line);
  const runProgram = promising(exports.run_program);

  const enableIdleButtons = () => {
    for (const button of [buttons.hello, buttons.fuel, buttons.entropy]) button.disabled = false;
    const blocked = !hasJSPI;
    for (const button of [buttons.program, buttons.sleepy, buttons.park, buttons.readline]) {
      button.disabled = blocked;
    }
  };

  let busy = false;
  const run = async (name, fn) => {
    if (busy) return;
    busy = true;
    for (const button of Object.values(buttons)) button.disabled = true;
    writeLine(`· ${name}`, "vm-cmd");
    try {
      const code = await fn();
      if (code !== 0) writeLine(`${name}: failed (see above)`, "vm-error");
    } catch (error) {
      writeLine(`${name}: trapped: ${error}`, "vm-error");
    } finally {
      busy = false;
      enableIdleButtons();
    }
  };

  buttons.hello.onclick = () => run("hello + add", () => exports.run_hello());
  buttons.fuel.onclick = () => run("fuel metering", () => exports.run_fuel());
  buttons.entropy.onclick = () =>
    run("entropy.seeded", () => {
      const seed = parseSeed(seedInput.value);
      if (seed === null) {
        writeLine("seed must be a u64 (decimal or 0x-hex)", "vm-error");
        return 0;
      }
      const count = Math.min(64, Math.max(1, Number(countInput.value) || 1));
      const lo = Number(seed & 0xffffffffn);
      const hi = Number(seed >> 32n);
      return exports.run_entropy(lo, hi, count);
    });

  buttons.park.onclick = () => run("park the VM (300 ms)", () => probeSleep(300));
  buttons.sleepy.onclick = () => run("sleepy (stackful-lift canary)", () => runSleepy());
  buttons.readline.onclick = () => run("read-line", () => probeReadLine());
  buttons.program.onclick = () =>
    run(`store: ${programSelect.value}`, async () => {
      const [namePtr, nameLen] = intoBlob(programSelect.value);
      const [argsPtr, argsLen] = intoBlob(splitArgs(programArgs.value).join(FIELD_SEPARATOR));
      try {
        return await runProgram(namePtr, nameLen, argsPtr, argsLen);
      } finally {
        if (nameLen) exportsRef.web_free(namePtr, nameLen);
        if (argsLen) exportsRef.web_free(argsPtr, argsLen);
      }
    });

  programSelect.onchange = () => {
    programArgs.value = ARG_PLACEHOLDERS[programSelect.value] ?? "";
  };

  enableIdleButtons();
}

main();
