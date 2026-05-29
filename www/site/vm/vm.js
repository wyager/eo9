// The Eo9 try-it page: load the blob (wasmtime + Pulley, compiled to wasm32), wire its import
// surface to the page, and boot the eosh shell straight into the terminal. Plain imports:
// terminal output, clocks, randomness. The genuinely-blocking imports (sleep, read-line,
// fetch-from-store) are JSPI `WebAssembly.Suspending` functions: the blob calls them
// synchronously, the browser parks the whole blob on the underlying promise (a timer, the
// visitor's Enter key, a fetch), and resumes it with the result. Everything Eo9-shaped happens
// inside the blob; this file is just a terminal, a keyboard, and a fetch cache.

const output = document.getElementById("vm-output");

let memory = null;
const decoder = new TextDecoder();
const encoder = new TextEncoder();

// `typeof WebAssembly.Suspending` alone throws if the WebAssembly global itself is missing
// (locked-down browsers do exist) — and a top-level throw here would take the whole page's
// wiring down with it, leaving a silently dead terminal. Guard the global first.
const hasJSPI =
  typeof WebAssembly === "object" &&
  typeof WebAssembly.Suspending === "function" &&
  typeof WebAssembly.promising === "function";

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

// One pending read-line at a time. There is no separate input box: while the shell is
// reading, keystrokes anywhere on the page are typed straight into the terminal — a live
// line (with a block cursor) is rendered inside the terminal output itself, exactly where
// the command will end up.
let pendingReadLine = null;
let liveLine = null; // the in-progress command line element, while the shell is reading
let liveText = null; // its text span
let liveBuffer = "";

function armReadLine() {
  liveBuffer = "";
  liveLine = document.createElement("div");
  liveLine.className = "vm-cmd vm-live";
  liveLine.append("> ");
  liveText = document.createElement("span");
  liveLine.appendChild(liveText);
  const cursor = document.createElement("span");
  cursor.className = "vm-cursor";
  liveLine.appendChild(cursor);
  output.appendChild(liveLine);
  output.scrollTop = output.scrollHeight;
  return new Promise((resolve) => {
    pendingReadLine = resolve;
  });
}

function renderLiveLine() {
  if (liveText !== null) liveText.textContent = liveBuffer;
  output.scrollTop = output.scrollHeight;
}

function submitTerminalLine() {
  if (pendingReadLine === null) return;
  const line = liveBuffer;
  // Freeze the live line into the ordinary echoed command line.
  if (liveLine !== null) {
    liveLine.classList.remove("vm-live");
    liveLine.textContent = `> ${line}`;
  }
  liveLine = null;
  liveText = null;
  liveBuffer = "";
  const resolve = pendingReadLine;
  pendingReadLine = null;
  resolve(line);
}

const READLINE_HINT =
  "(the shell isn't reading right now — wait for the eosh> prompt, then just type and press Enter)";
let lastHintAt = 0;
function readlineHint() {
  const now = Date.now();
  if (now - lastHintAt < 2000) return;
  lastHintAt = now;
  writeLine(READLINE_HINT, "vm-cmd");
}

// Behave like a terminal: while the shell is reading, printable keys, Backspace, and Enter
// anywhere on the page (outside text-selection modifiers and form fields) are the terminal's.
document.addEventListener("keydown", (event) => {
  if (event.metaKey || event.ctrlKey || event.altKey) return;
  const target = event.target;
  const inFormField =
    target instanceof HTMLElement &&
    (target.tagName === "INPUT" ||
      target.tagName === "TEXTAREA" ||
      target.tagName === "SELECT" ||
      target.tagName === "BUTTON" ||
      target.isContentEditable);
  if (inFormField) return;
  if (pendingReadLine !== null) {
    if (event.key.length === 1) {
      liveBuffer += event.key;
      renderLiveLine();
      event.preventDefault();
    } else if (event.key === "Backspace") {
      liveBuffer = liveBuffer.slice(0, -1);
      renderLiveLine();
      event.preventDefault();
    } else if (event.key === "Enter") {
      event.preventDefault();
      submitTerminalLine();
    }
  } else if (event.key === "Enter") {
    // The shell isn't reading: explain instead of doing nothing.
    readlineHint();
  }
});

// Pasting while the shell is reading types into the terminal too; a newline submits.
document.addEventListener("paste", (event) => {
  if (pendingReadLine === null) return;
  const target = event.target;
  if (target instanceof HTMLElement && (target.tagName === "INPUT" || target.tagName === "TEXTAREA")) {
    return;
  }
  const text = event.clipboardData ? event.clipboardData.getData("text") : "";
  if (text === "") return;
  event.preventDefault();
  const newline = text.indexOf("\n");
  if (newline === -1) {
    liveBuffer += text;
    renderLiveLine();
  } else {
    liveBuffer += text.slice(0, newline).replace(/\r$/, "");
    renderLiveLine();
    submitTerminalLine();
  }
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

// Content-fingerprinted asset URLs, loaded once from /vm/assets.json. The manifest is
// short-cached while the assets it points at are immutable+forever-cached, so a new build
// flips these URLs and clients pick up the new OS immediately. Falls back to the canonical
// names if the manifest is missing (e.g. a dev build before fingerprinting).
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
    // fall through to the canonical names
  }
  assetMap = { blob: "/vm/web-eo9.wasm", store: {} };
  return assetMap;
}
function blobUrl() {
  return (assetMap && assetMap.blob) || "/vm/web-eo9.wasm";
}
function storeUrl(name) {
  return (assetMap && assetMap.store && assetMap.store[name]) || `/vm/store/${name}.cwasm`;
}

// The most recent store fetch, copied into the blob by host_fetch_copy.
let fetchedArtifact = null;

async function hostFetchLen(namePtr, nameLen) {
  const name = decoder.decode(new Uint8Array(memory.buffer, namePtr, nameLen));
  if (!/^[a-z0-9-]{1,64}$/.test(name)) return -1;
  try {
    const response = await fetch(storeUrl(name));
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

// (Compositions are compiled *inside the blob* — Cranelift -> Pulley, the same vendored
// compile layers the bare-metal kernel uses on-target — so there is no server compile call
// to wire here.)

// Fallbacks when the browser has no JSPI: report "unavailable" so the blob errors cleanly
// (the page also says why before trying to boot the shell).
function unavailableSleep() {}
function unavailableReadLine() {
  return -2;
}
function unavailableFetchLen() {
  return -2;
}

// --- load, boot, and hand the page to eosh ------------------------------------------------------

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
    await loadAssetMap();
    const url = blobUrl();
    const response = await fetch(url);
    if (!response.ok) {
      throw new Error(`fetching ${url} failed: HTTP ${response.status}`);
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
    memory = exports.memory;
  } catch (error) {
    // Report the actual cause; don't blame missing WebAssembly support for a network or
    // server problem.
    output.textContent = "";
    writeLine(`could not load the Eo9 OS: ${error}`, "vm-error");
    if (typeof WebAssembly !== "object") {
      writeLine("(this page needs a browser with WebAssembly enabled)", "vm-error");
    } else {
      writeLine(
        "(the message above is the real cause — usually the download failed; the browser console has details)",
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
      "this browser has no JavaScript Promise Integration (JSPI), which the shell's read-line " +
        "needs, so the interactive prompt cannot run here. Current Chrome or Edge has JSPI.",
      "vm-error",
    );
    return;
  }

  // Boot the shell. eosh_boot calls the JSPI read-line import, so it must be wrapped with
  // WebAssembly.promising and awaited; it returns when the visitor types `exit`.
  const eoshBoot = WebAssembly.promising(exports.eosh_boot);
  writeLine("· booting eosh — just type at the eosh> prompt", "vm-cmd");
  try {
    const code = await eoshBoot();
    if (code !== 0) writeLine("the shell reported a failure (see above)", "vm-error");
  } catch (error) {
    writeLine(`the shell trapped: ${error}`, "vm-error");
  }
  writeLine("· shell session ended — reload the page for a fresh one", "vm-cmd");
}

main();
