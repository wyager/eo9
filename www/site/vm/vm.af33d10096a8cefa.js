// The Eo9 try-it page: load the blob (wasmtime + Pulley, compiled to wasm32), wire its import
// surface to the page, and boot the eosh shell straight into the terminal. Plain imports:
// terminal output, clocks, randomness. The genuinely-blocking imports (sleep, read-key,
// read-line, fetch-from-store) are JSPI `WebAssembly.Suspending` functions: the blob calls
// them synchronously, the browser parks the whole blob on the underlying promise (a timer,
// the visitor's keyboard, a fetch), and resumes it with the result. Everything Eo9-shaped
// happens inside the blob; this file is just a terminal, a keyboard, and a fetch cache.
//
// The terminal is a real (minimal) one: keydown events are encoded as the serial console's
// byte stream (printables, Enter `\r`, Backspace 0x7f, Tab, arrows as `ESC [ A/B/C/D`,
// Ctrl-C as 0x03 — exactly what usb.kbd and the kernel's serial decoder agree on) and fed
// to the blob's read-key import, so eosh runs its REAL per-keystroke editor in the page —
// incremental parsing, TAB completion, red dead-name marking, history recall. The output
// stream is interpreted by the render layer below, which implements exactly the editor's
// emitted-sequence contract (eosh-inc/src/editor.rs, the area/40 table; the same subset
// fbcon renders): `\b`, `\r`, `\r\n`, `CSI K`, `CSI A`, `CSI <n> G`, and the SGR marker
// pairs 31/0 and 7/27. Everything else CSI-shaped is consumed and ignored (the fbcon
// posture), so an unknown sequence can never leak `[A`-style garbage into the page.

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

// --- the terminal render layer ---------------------------------------------------------------
//
// A fixed-width character grid: one <div> per terminal row inside #vm-output, each row an
// array of cells (character + CSS class). The column count is the geometry the editor's
// wrap-aware repaint computes against, so it MUST equal the `term-width` record the blob
// declares in its session manifest (www/web-eo9/blob/src/providers.rs::session_manifest);
// the two constants are a contract pair.
const TERM_COLS = 100;

class Term {
  constructor(element) {
    this.el = element;
    this.reset();
  }

  reset() {
    this.el.textContent = "";
    this.rows = []; // each: { cells: [{ch, cls}], div: Element|null, dirty: bool }
    this.row = 0;
    this.col = 0;
    this.red = false; // SGR 31 .. 0
    this.inv = false; // SGR 7 .. 27
    this.esc = 0; // 0 idle, 1 saw ESC, 2 inside CSI
    this.csi = ""; // CSI parameter/intermediate bytes collected so far
    this.cursorVisible = false;
    this.cursorRow = -1; // the row the cursor span was last rendered on
    this.ensureRow(0);
  }

  ensureRow(index) {
    while (this.rows.length <= index) {
      this.rows.push({ cells: [], div: null, dirty: true });
    }
    return this.rows[index];
  }

  // The CSS class for a cell written now, under `baseCls` (the chunk-level style: the
  // stderr marker's vm-error, or a page status line's vm-cmd) and the current SGR state.
  cellClass(baseCls) {
    let cls = baseCls;
    if (this.red) cls = cls ? cls + " vm-red" : "vm-red";
    if (this.inv) cls = cls ? cls + " vm-inv" : "vm-inv";
    return cls;
  }

  put(ch, baseCls) {
    if (this.col >= TERM_COLS) {
      // Auto-wrap for plain program output. The editor never relies on this (it emits
      // an explicit \r\n whenever a character fills the last column), so there is no
      // deferred-last-column state to get wrong.
      this.newline();
    }
    const row = this.ensureRow(this.row);
    while (row.cells.length < this.col) row.cells.push({ ch: " ", cls: "" });
    row.cells[this.col] = { ch, cls: this.cellClass(baseCls) };
    this.col += 1;
    row.dirty = true;
  }

  newline() {
    this.row += 1;
    this.col = 0;
    this.ensureRow(this.row);
  }

  // CSI K: erase from the cursor to the end of the row.
  eraseToEnd() {
    const row = this.ensureRow(this.row);
    if (row.cells.length > this.col) {
      row.cells.length = this.col;
      row.dirty = true;
    }
  }

  handleCsiFinal(final) {
    if (final === "K") {
      this.eraseToEnd();
    } else if (final === "A") {
      // Cursor up one row, column preserved.
      if (this.row > 0) this.row -= 1;
    } else if (final === "G") {
      // Cursor to absolute column n (1-based).
      const n = parseInt(this.csi, 10);
      const col = Number.isFinite(n) && n >= 1 ? n - 1 : 0;
      this.col = Math.min(col, TERM_COLS);
    } else if (final === "m") {
      for (const part of this.csi.split(";")) {
        const code = part === "" ? 0 : parseInt(part, 10);
        if (code === 0) {
          this.red = false;
          this.inv = false;
        } else if (code === 31) {
          this.red = true;
        } else if (code === 7) {
          this.inv = true;
        } else if (code === 27) {
          this.inv = false;
        }
        // Any other SGR: consumed, ignored.
      }
    }
    // Any other final (Home/End/Delete shapes, …): consumed, ignored — nothing leaks.
  }

  // Interpret one chunk of output. `baseCls` styles every cell the chunk writes (the
  // stderr marker routes "vm-error" here; page status lines route "vm-cmd").
  feed(text, baseCls) {
    for (const ch of text) {
      if (this.esc === 1) {
        if (ch === "[" || ch === "O") {
          this.esc = 2;
          this.csi = "";
        } else if (ch === "\u001b") {
          // ESC ESC: stay armed.
        } else {
          this.esc = 0; // a lone ESC: dropped, the byte after decodes normally
          this.feedPlain(ch, baseCls);
        }
        continue;
      }
      if (this.esc === 2) {
        const code = ch.codePointAt(0);
        if (code >= 0x20 && code <= 0x3f) {
          this.csi += ch; // parameter / intermediate bytes
        } else {
          this.esc = 0;
          this.handleCsiFinal(ch);
        }
        continue;
      }
      if (ch === "\u001b") {
        this.esc = 1;
        continue;
      }
      this.feedPlain(ch, baseCls);
    }
    this.render();
  }

  feedPlain(ch, baseCls) {
    if (ch === "\r") {
      this.col = 0;
    } else if (ch === "\n") {
      this.newline();
    } else if (ch === "\b") {
      if (this.col > 0) this.col -= 1;
    } else if (ch.codePointAt(0) < 0x20) {
      // Other C0 controls (BEL from a TAB with nothing to offer, …): ignored.
    } else {
      this.put(ch, baseCls);
    }
  }

  setCursorVisible(visible) {
    if (this.cursorVisible !== visible) {
      this.cursorVisible = visible;
      this.ensureRow(this.row).dirty = true;
      if (this.cursorRow >= 0 && this.cursorRow < this.rows.length) {
        this.rows[this.cursorRow].dirty = true;
      }
      this.render();
    }
  }

  render() {
    // The cursor moved rows: the old row must drop its cursor span.
    if (this.cursorRow !== this.row && this.cursorRow >= 0 && this.cursorRow < this.rows.length) {
      this.rows[this.cursorRow].dirty = true;
    }
    if (this.cursorVisible) this.ensureRow(this.row).dirty = true;
    for (let i = 0; i < this.rows.length; i++) {
      const row = this.rows[i];
      if (row.div === null) {
        row.div = document.createElement("div");
        this.el.appendChild(row.div);
        row.dirty = true;
      }
      if (!row.dirty) continue;
      row.dirty = false;
      row.div.textContent = "";
      // Group consecutive same-class cells into spans (plain runs become bare text).
      let runText = "";
      let runCls = null;
      const flush = () => {
        if (runText === "") return;
        if (runCls) {
          const span = document.createElement("span");
          span.className = runCls;
          span.textContent = runText;
          row.div.appendChild(span);
        } else {
          row.div.append(runText);
        }
        runText = "";
      };
      for (const cell of row.cells) {
        if (cell.cls !== runCls) {
          flush();
          runCls = cell.cls;
        }
        runText += cell.ch;
      }
      flush();
      if (this.cursorVisible && i === this.row) {
        const cursor = document.createElement("span");
        cursor.className = "vm-cursor";
        row.div.appendChild(cursor);
      } else if (row.cells.length === 0) {
        // An empty div collapses to zero height; keep blank rows one line tall.
        row.div.append(" ");
      }
    }
    this.cursorRow = this.cursorVisible ? this.row : -1;
    this.el.scrollTop = this.el.scrollHeight;
  }

  // A page status line (boot progress, errors): rendered through the same grid.
  status(text, cls) {
    if (this.col !== 0) this.feed("\r\n", "");
    this.feed(text + "\r\n", cls || "");
  }
}

const term = new Term(output);

// Expose the model for the byte-level harness (www/web-eo9/verify-term.mjs): unit tests
// feed the editor's contract sequences into a fresh Term over a fake element. Harmless in
// the browser (one extra global).
globalThis.__eo9Term = { Term, TERM_COLS };

// --- plain imports -------------------------------------------------------------------------

function hostWrite(ptr, len) {
  const bytes = new Uint8Array(memory.buffer, ptr, len);
  let text = decoder.decode(bytes);
  // Chunks the OS wrote to standard error arrive with a leading U+0001 marker (an in-band
  // signal from the blob, never visible text): strip it and style the chunk's cells.
  if (text.charCodeAt(0) === 1) {
    term.feed(text.slice(1), "vm-error");
  } else {
    term.feed(text, "");
  }
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

// --- keyboard input (the read-key byte queue) -------------------------------------------------
//
// Keydown events are encoded as the serial console's byte stream and queued; the blob's
// suspended read-key import drains the queue one byte at a time. Keys typed while the shell
// is busy executing a command stay queued (type-ahead), exactly like a real terminal.

const keyQueue = [];
let keyWaiter = null; // resolve fn of the promise a parked read is waiting on
let keyMode = false; // true once the blob first asked for a key (the editor took over)
let lineReader = false; // a host_read_line call is parked on the queue
let sessionLive = false; // the interactive shell session is running

function pushKeyBytes(bytes) {
  for (const b of bytes) keyQueue.push(b);
  if (keyWaiter !== null && keyQueue.length > 0) {
    const waiter = keyWaiter;
    keyWaiter = null;
    waiter();
  }
}

async function nextKeyByte() {
  while (keyQueue.length === 0) {
    term.setCursorVisible(true);
    await new Promise((resolve) => {
      keyWaiter = resolve;
    });
  }
  term.setCursorVisible(false);
  return keyQueue.shift();
}

// One byte for the blob's read-key import. Never returns -1 (end of input) in the browser:
// the page's keyboard only closes when the page does.
async function hostReadKey() {
  keyMode = true;
  return await nextKeyByte();
}

// Encode one keydown event as the serial byte sequence, or null when the key is not the
// terminal's (browser shortcuts, IME, function keys, …).
function encodeKey(event) {
  if (event.metaKey || event.altKey) return null;
  if (event.ctrlKey) {
    if (event.key.length === 1) {
      const key = event.key.toLowerCase();
      // Ctrl-C cancels the line — but never steal a copy: with a selection active the
      // browser keeps it.
      if (key === "c" && !hasSelection()) return [0x03];
      if (key === "d") return [0x04];
    }
    return null;
  }
  switch (event.key) {
    case "Enter":
      return [0x0d];
    case "Backspace":
      return [0x7f];
    case "Tab":
      return [0x09];
    case "ArrowUp":
      return [0x1b, 0x5b, 0x41]; // ESC [ A
    case "ArrowDown":
      return [0x1b, 0x5b, 0x42]; // ESC [ B
    case "ArrowRight":
      return [0x1b, 0x5b, 0x43]; // ESC [ C
    case "ArrowLeft":
      return [0x1b, 0x5b, 0x44]; // ESC [ D
    default:
      // One printable character (possibly multi-byte UTF-8; astral chars are one
      // code point but two UTF-16 units, hence the spread).
      if ([...event.key].length === 1) return [...encoder.encode(event.key)];
      return null;
  }
}

function hasSelection() {
  if (typeof getSelection !== "function") return false;
  const selection = getSelection();
  return selection !== null && String(selection) !== "";
}

// Is some read parked on (or draining into) the keyboard queue? Once the editor took over
// (keyMode), every keystroke belongs to the terminal for the rest of the session.
function readerActive() {
  return (keyMode && sessionLive) || lineReader;
}

const READLINE_HINT =
  "(the shell isn't reading right now — wait for the eosh> prompt, then just type)";
let lastHintAt = 0;
function readlineHint() {
  const now = Date.now();
  if (now - lastHintAt < 2000) return;
  lastHintAt = now;
  term.status(READLINE_HINT, "vm-cmd");
}

// Behave like a terminal: while the shell is reading, keys anywhere on the page (outside
// text-selection modifiers and form fields) are the terminal's.
document.addEventListener("keydown", (event) => {
  const target = event.target;
  const inFormField =
    target instanceof HTMLElement &&
    (target.tagName === "INPUT" ||
      target.tagName === "TEXTAREA" ||
      target.tagName === "SELECT" ||
      target.tagName === "BUTTON" ||
      target.isContentEditable);
  if (inFormField) return;
  if (readerActive()) {
    const bytes = encodeKey(event);
    if (bytes !== null) {
      event.preventDefault();
      pushKeyBytes(bytes);
    }
  } else if (event.key === "Enter" && !event.metaKey && !event.ctrlKey && !event.altKey) {
    // The shell isn't reading: explain instead of doing nothing.
    readlineHint();
  }
});

// Pasting while the shell is reading types into the terminal through the same byte path;
// newlines become Enter (`\r`), so a multi-line paste submits line by line.
document.addEventListener("paste", (event) => {
  if (!readerActive()) return;
  const target = event.target;
  if (target instanceof HTMLElement && (target.tagName === "INPUT" || target.tagName === "TEXTAREA")) {
    return;
  }
  const text = event.clipboardData ? event.clipboardData.getData("text") : "";
  if (text === "") return;
  event.preventDefault();
  const normalized = text.replace(/\r\n/g, "\n").replace(/[\r\n]/g, "\r");
  pushKeyBytes([...encoder.encode(normalized)]);
});

// --- suspending imports (JSPI) ----------------------------------------------------------------

// The byte index where the trailing (possibly incomplete) UTF-8 character starts, or
// null when the tail is not UTF-8-shaped (used by the line reader's echo).
function utf8CharStart(line) {
  let i = line.length - 1;
  while (i >= 0 && (line[i] & 0xc0) === 0x80) i -= 1;
  return i >= 0 ? i : null;
}

// The line-based read (the WIT `text.read-line`, kept for programs that ask for whole
// lines): a minimal line discipline over the same key-byte queue — printables echo,
// Backspace erases one character, Enter submits. Escape sequences are consumed silently
// (the kernel decoder's posture); the editor never comes through here.
async function hostReadLine(ptr, cap) {
  lineReader = true;
  const line = [];
  let esc = 0; // 0 idle, 1 saw ESC, 2 inside CSI
  try {
    for (;;) {
      const byte = await nextKeyByte();
      if (esc === 1) {
        esc = byte === 0x5b || byte === 0x4f ? 2 : byte === 0x1b ? 1 : 0;
        continue;
      }
      if (esc === 2) {
        if (byte < 0x20 || byte > 0x3f) esc = 0;
        continue;
      }
      if (byte === 0x1b) {
        esc = 1;
      } else if (byte === 0x0d || byte === 0x0a) {
        term.feed("\r\n", "");
        break;
      } else if (byte === 0x7f || byte === 0x08) {
        if (line.length > 0) {
          // Pop one whole UTF-8 character (continuation bytes, then the lead).
          while (line.length > 0) {
            const dropped = line.pop();
            if ((dropped & 0xc0) !== 0x80) break;
          }
          term.feed("\b \b", "");
        }
      } else if (byte >= 0x20) {
        if (line.length < cap) {
          line.push(byte);
          // Echo ASCII as typed; multi-byte UTF-8 echoes once its last byte arrives.
          if (byte < 0x80) {
            term.feed(String.fromCharCode(byte), "");
          } else {
            const start = utf8CharStart(line);
            if (start !== null) {
              const tail = new Uint8Array(line.slice(start));
              const text = decoder.decode(tail);
              if (!text.includes("�")) term.feed(text, "");
            }
          }
        }
      }
    }
  } finally {
    lineReader = false;
  }
  const bytes = new Uint8Array(line);
  const len = Math.min(bytes.length, cap);
  new Uint8Array(memory.buffer, ptr, len).set(bytes.subarray(0, len));
  return len;
}

// The page's framebuffer: presented pixels are blitted onto the canvas under the terminal,
// which stays hidden until something actually draws. Pixels arrive as tightly packed
// xrgb8888 rows (memory bytes B,G,R,X) of the rectangle at (x,y); the canvas is sized from
// the framebuffer dimensions the blit carries, so the blob is the single source of truth.
// Display-only: readback is answered from the blob's own backing copy, so a host with no
// canvas (the node harnesses) can ignore these calls entirely.
function hostGfxPresent(ptr, len, fbW, fbH, x, y, w, h) {
  const canvas = document.getElementById("vm-display");
  if (!canvas || typeof canvas.getContext !== "function" || w === 0 || h === 0) return;
  if (canvas.width !== fbW) canvas.width = fbW;
  if (canvas.height !== fbH) canvas.height = fbH;
  const ctx = canvas.getContext("2d");
  if (!ctx) return;
  canvas.classList.add("vm-display-on");
  const src = new Uint8Array(memory.buffer, ptr, len);
  const img = ctx.createImageData(w, h);
  for (let i = 0, n = w * h; i < n; i++) {
    img.data[i * 4] = src[i * 4 + 2]; // R (xrgb8888 memory order is B,G,R,X)
    img.data[i * 4 + 1] = src[i * 4 + 1]; // G
    img.data[i * 4 + 2] = src[i * 4]; // B
    img.data[i * 4 + 3] = 255;
  }
  ctx.putImageData(img, x, y);
}

async function hostSleepMs(ms) {
  await new Promise((resolve) => setTimeout(resolve, ms));
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
function unavailableReadKey() {
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
      host_gfx_present: hostGfxPresent,
      host_sleep_ms: hasJSPI ? new WebAssembly.Suspending(hostSleepMs) : unavailableSleep,
      host_read_line: hasJSPI ? new WebAssembly.Suspending(hostReadLine) : unavailableReadLine,
      host_read_key: hasJSPI ? new WebAssembly.Suspending(hostReadKey) : unavailableReadKey,
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
    term.reset();
    term.status(`could not load the Eo9 OS: ${error}`, "vm-error");
    if (typeof WebAssembly !== "object") {
      term.status("(this page needs a browser with WebAssembly enabled)", "vm-error");
    } else {
      term.status(
        "(the message above is the real cause — usually the download failed; the browser console has details)",
        "vm-error",
      );
    }
    return;
  }

  term.reset();
  const failures = exports.boot();
  if (failures !== 0) {
    term.status("boot reported a failure — see above", "vm-error");
    return;
  }
  if (!hasJSPI) {
    term.status(
      "this browser has no JavaScript Promise Integration (JSPI), which the shell's keyboard " +
        "input needs, so the interactive prompt cannot run here. Current Chrome or Edge has JSPI.",
      "vm-error",
    );
    return;
  }

  // Boot the shell. eosh_boot calls the JSPI read-key import, so it must be wrapped with
  // WebAssembly.promising and awaited; it returns when the visitor types `exit`.
  const eoshBoot = WebAssembly.promising(exports.eosh_boot);
  term.status("· booting eosh — just type at the eosh> prompt", "vm-cmd");
  sessionLive = true;
  try {
    const code = await eoshBoot();
    if (code !== 0) term.status("the shell reported a failure (see above)", "vm-error");
  } catch (error) {
    term.status(`the shell trapped: ${error}`, "vm-error");
  } finally {
    sessionLive = false;
    term.setCursorVisible(false);
  }
  term.status("· shell session ended — reload the page for a fresh one", "vm-cmd");
}

main();
