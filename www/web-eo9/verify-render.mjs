// Node (v25+, JSPI) RENDERING harness: runs the page's actual vm.js against the real blob
// with a minimal DOM stand-in, drives the keyboard exactly as a visitor would, and asserts
// on the rendered transcript. The other harnesses feed the blob through their own
// host_write and never exercise vm.js's rendering contract — which is exactly how the
// prompt-accumulation regression shipped invisibly. Since the read-key path landed this is
// also where the EDITOR is verified end to end in the page glue: per-keystroke echo, the
// red dead-name marking (a vm-red span in the DOM), TAB completion (argument flags), ^C,
// history recall, and paste — all through vm.js's keydown encoder, the blob's read-key
// import, and the terminal render layer. Run after `cargo xtask build-web-vm`:
//   node www/web-eo9/verify-render.mjs
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { runInThisContext } from "node:vm";

const here = dirname(fileURLToPath(import.meta.url));
const vmDir = join(here, "..", "site", "vm");
const assets = JSON.parse(readFileSync(join(vmDir, "assets.json"), "utf8"));

// --- a DOM small enough to read in one sitting ---------------------------------------------

class FakeText {
  constructor(text) {
    this.text = String(text);
  }
  get textContent() {
    return this.text;
  }
}

class FakeElement {
  constructor(tag) {
    this.tag = tag;
    this.parentNode = null;
    this.children = [];
    this.className = "";
    this.ownText = "";
    this.scrollTop = 0;
  }
  appendChild(node) {
    node.parentNode = this;
    this.children.push(node);
    return node;
  }
  append(text) {
    this.children.push(new FakeText(text));
  }
  remove() {
    if (this.parentNode === null) return;
    const index = this.parentNode.children.indexOf(this);
    if (index >= 0) this.parentNode.children.splice(index, 1);
    this.parentNode = null;
  }
  get lastElementChild() {
    for (let i = this.children.length - 1; i >= 0; i--) {
      if (this.children[i] instanceof FakeElement) return this.children[i];
    }
    return null;
  }
  get scrollHeight() {
    return 0;
  }
  get textContent() {
    return this.ownText + this.children.map((child) => child.textContent).join("");
  }
  set textContent(value) {
    this.ownText = String(value);
    this.children = [];
  }
}

const output = new FakeElement("div");
const listeners = {}; // event name -> handler (vm.js registers one keydown + one paste)

globalThis.document = {
  getElementById: (id) => (id === "vm-output" ? output : null),
  createElement: (tag) => new FakeElement(tag),
  addEventListener: (name, handler) => {
    listeners[name] = handler;
  },
};
// vm.js guards `target instanceof HTMLElement`; give it a class nothing here instantiates.
globalThis.HTMLElement = class HTMLElement {};

// fetch: serve the manifest and the fingerprinted assets straight from disk.
globalThis.fetch = async (url) => {
  const path = String(url).replace(/^\/vm\//, "");
  try {
    const bytes = readFileSync(join(vmDir, path));
    return {
      ok: true,
      status: 200,
      json: async () => JSON.parse(bytes.toString("utf8")),
      arrayBuffer: async () =>
        bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength),
      // vm.js prefers instantiateStreaming(response.clone()); steer it to the buffered path.
      clone() {
        return this;
      },
    };
  } catch {
    return { ok: false, status: 404 };
  }
};

// --- run the real vm.js ---------------------------------------------------------------------

runInThisContext(readFileSync(join(here, "..", "site", "vm", "vm.js"), "utf8"), {
  filename: "vm.js",
});

// --- drive the keyboard ---------------------------------------------------------------------

// Rows are divs; their text is the rendered line (the cursor span contributes "").
const transcriptLines = () => output.children.map((child) => child.textContent);
const promptCount = () => transcriptLines().filter((line) => line.startsWith("eosh>")).length;
const liveLineText = () => {
  const lines = transcriptLines();
  // The live prompt row is the last row with any content (trailing blank rows render " ").
  for (let i = lines.length - 1; i >= 0; i--) {
    if (lines[i].trim() !== "") return lines[i];
  }
  return "";
};

// Walk a rendered subtree for spans of a class; returns their text contents.
function spansIn(root, cls) {
  const found = [];
  const walk = (node) => {
    if (!(node instanceof FakeElement)) return;
    if (node.className.split(" ").includes(cls)) found.push(node.textContent);
    for (const child of node.children) walk(child);
  };
  walk(root);
  return found;
}
const spansWithClass = (cls) => spansIn(output, cls);

// The live prompt row's div (the last row with content) — red-mark checks scope here:
// a SUBMITTED red line legitimately stays red in the scrollback, exactly like a real
// terminal; only the live row's marking must clear when the dead input is erased.
function liveRowDiv() {
  for (let i = output.children.length - 1; i >= 0; i--) {
    const child = output.children[i];
    if (child instanceof FakeElement && child.textContent.trim() !== "") return child;
  }
  return null;
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function waitFor(what, predicate, timeoutMs = 120_000) {
  const start = Date.now();
  while (!predicate()) {
    if (Date.now() - start > timeoutMs) {
      console.error(`FAIL timed out waiting for ${what}`);
      console.error("transcript so far:");
      for (const line of transcriptLines()) console.error(`  | ${line}`);
      process.exit(1);
    }
    await sleep(25);
  }
}

function key(k, modifiers = {}) {
  listeners.keydown({
    key: k,
    metaKey: false,
    ctrlKey: false,
    altKey: false,
    target: null,
    preventDefault() {},
    ...modifiers,
  });
}

let submits = 0;
async function type(command) {
  for (const ch of command) key(ch);
  key("Enter");
  submits += 1;
}

let consumedPrompts = 0;
async function atPrompt() {
  await waitFor(`prompt #${consumedPrompts + 1}`, () => promptCount() > consumedPrompts);
  consumedPrompts = promptCount();
}

// --- the session ----------------------------------------------------------------------------

const checks = [];
function check(name, ok, detail) {
  checks.push([name, ok]);
  console.log(`${ok ? "PASS" : "FAIL"} ${name}${ok || detail === undefined ? "" : ` — ${detail}`}`);
}

await waitFor("boot + first prompt", () => promptCount() > 0);
consumedPrompts = promptCount();

// Empty Enters: the historical accumulation repro (each loop writes another `eosh> `).
await type("");
await atPrompt();
await type("");
await atPrompt();

// A normal command (stdout from the child + the shell's own outcome line). The editor
// echoes per keystroke, so the typed command is visible on the prompt row as it is typed.
for (const ch of "hello") key(ch);
await waitFor("per-key echo", () => liveLineText().includes("eosh> hello"));
key("Enter");
submits += 1;
await waitFor("hello outcome", () => transcriptLines().some((l) => l.includes("ok: greeted")));
await atPrompt();

// A multi-line builtin.
await type("help");
await waitFor("help output", () => transcriptLines().some((l) => l.includes("explore the sandbox")));
await atPrompt();

// A standard-error line (unresolvable command). NOTE: `nosuchprogram` is name-dead while
// typed (the editor marks it red — that is the point of the marking: it will not run);
// Enter still submits, and the session renders the resolution error.
await type("nosuchprogram");
await waitFor("the error line", () => transcriptLines().some((l) => l.includes("cannot resolve")));
await atPrompt();

await type("ls /bin");
await waitFor("ls outcome", () => transcriptLines().some((l) => l.includes("listed(")));
await atPrompt();

// --- the editor: red dead-name marking ------------------------------------------------------

// `net.` prefix-extends real /bin programs (green); `x` kills every candidate — the editor
// opens the SGR-31 marker and the render layer turns it into a vm-red span.
for (const ch of "net.x") key(ch);
await waitFor("the dead name echoed", () => liveLineText().includes("net."));
await waitFor("a red span on the dead character", () => {
  const row = liveRowDiv();
  return row !== null && spansIn(row, "vm-red").some((text) => text.includes("x"));
});
check("dead-name marking renders as a vm-red span", true);

// Backspace clears the mark: erase the dead `x`, the red span disappears from the live row
// (rows already submitted keep their marking — terminal scrollback is history).
key("Backspace");
await waitFor("the red span cleared", () => {
  const row = liveRowDiv();
  return row !== null && !spansIn(row, "vm-red").some((text) => text.trim() !== "");
});
check("backspace over the dead character clears the red span", true);

// Ctrl-C cancels the rest of the line (prints ^C, submits empty — a fresh prompt follows).
key("c", { ctrlKey: true });
submits += 1;
await waitFor("^C rendered", () => transcriptLines().some((l) => l.includes("^C")));
await atPrompt();

// --- the editor: TAB completion --------------------------------------------------------------

// The space after `hello` is a word boundary: the embedder resolves hello's argument hints
// (describe + manual, memoized). `--n` then TABs to the unique flag `--name ` (trailing
// space — a unique completion always appends one).
for (const ch of "hello --n") key(ch);
await waitFor("the flag prefix echoed", () => liveLineText().includes("hello --n"));
key("Tab");
await waitFor("TAB completed the flag", () => liveLineText().includes("hello --name "));
check("TAB after `hello --n` completes `--name `", true);
for (const ch of "tabbed") key(ch);
key("Enter");
submits += 1;
await waitFor("the completed command ran", () =>
  transcriptLines().some((l) => l.includes("Hello, tabbed")),
);
await atPrompt();

// --- the editor: history recall ---------------------------------------------------------------

// Up recalls the newest command onto the live row (an in-place repaint — \r CSI K, never a
// new prompt row); Enter reruns it.
key("ArrowUp");
await waitFor("recall of the completed command", () =>
  liveLineText().includes("hello --name tabbed"),
);
const tabbedBefore = transcriptLines().filter((l) => l.includes("Hello, tabbed")).length;
key("Enter");
submits += 1;
await waitFor(
  "the recalled command reran",
  () => transcriptLines().filter((l) => l.includes("Hello, tabbed")).length > tabbedBefore,
);
await atPrompt();

// Browsing reaches older entries; Down comes back; Down past the newest restores the
// stash. (History keeps duplicates: the rerun put `hello --name tabbed` in twice, so
// `ls /bin` sits three steps up.)
key("ArrowUp"); // hello --name tabbed (newest)
await waitFor("recall newest again", () => liveLineText().includes("hello --name tabbed"));
key("ArrowUp"); // hello --name tabbed (the duplicate)
key("ArrowUp"); // ls /bin
await waitFor("recall the older entry", () => liveLineText().includes("ls /bin"));
key("ArrowDown"); // back down to the duplicate
await waitFor("down moves back to the newer entry", () =>
  liveLineText().includes("hello --name tabbed"),
);
key("ArrowDown"); // the newest entry
key("ArrowDown"); // leave browsing: the (empty) fresh line returns
for (const ch of "echo stash kept") key(ch);
await waitFor("typed a fresh line", () => liveLineText().includes("echo stash kept"));
key("ArrowUp");
await waitFor("browsed away from the fresh line", () =>
  liveLineText().includes("hello --name tabbed"),
);
key("ArrowDown");
await waitFor("the stash came back", () => liveLineText().includes("echo stash kept"));
key("Enter");
submits += 1;
await waitFor("the restored line ran (variadic echo)", () =>
  transcriptLines().some((l) => l.trim() === "stash kept"),
);
await atPrompt();

// --- paste -----------------------------------------------------------------------------------

// Paste feeds the same byte path; the newline becomes Enter and submits.
listeners.paste({
  target: null,
  clipboardData: { getData: () => "echo pasted ok\n" },
  preventDefault() {},
});
submits += 1;
await waitFor("the pasted line ran", () =>
  transcriptLines().some((l) => l.trim() === "pasted ok"),
);
await atPrompt();

await type("exit");
await waitFor("session end", () => transcriptLines().some((l) => l.includes("session ended")));

// --- assertions -------------------------------------------------------------------------------

const lines = transcriptLines();

// THE regression: a transcript row must never carry more than one `eosh>`.
const accumulated = lines.filter((line) => line.split("eosh>").length > 2);
check(
  "no row accumulates more than one eosh> prefix",
  accumulated.length === 0,
  `offenders: ${JSON.stringify(accumulated.slice(0, 3))}`,
);

// Prompts are their own rows: every eosh> sits at the start of its row (the page's own
// "·" status lines may mention the prompt by name).
const misplaced = lines.filter((line) => {
  const at = line.indexOf("eosh>");
  return at > 0 && !line.startsWith("\u00b7 ");
});
check(
  "every eosh> prompt starts its row",
  misplaced.length === 0,
  `offenders: ${JSON.stringify(misplaced.slice(0, 3))}`,
);

// The typed command froze on its prompt row (per-key echo, then the submit \r\n).
check(
  "the typed command renders on the prompt row",
  lines.some((line) => /^eosh> hello\s*$/.test(line)),
  `prompt rows: ${JSON.stringify(lines.filter((l) => l.includes("eosh>")).slice(0, 8))}`,
);

// Output and outcome lines are their own rows, not glued behind a prompt.
check(
  "the outcome line is not glued to a prompt",
  lines.some((line) => /^ok: greeted$/.test(line.trim())),
);
check(
  "child stdout is its own row",
  lines.some((line) => /Hello, world/.test(line) && !line.includes("eosh>")),
);

// One prompt row per read: the initial prompt plus one per submission except the final
// `exit` — and arrow browsing / TAB / in-place repaints must never mint extra prompt rows.
const promptLines = lines.filter((line) => line.startsWith("eosh>"));
check(
  "exactly one prompt row per command read",
  promptLines.length === submits,
  `got ${promptLines.length}, expected ${submits}: ${JSON.stringify(promptLines)}`,
);

// Recall repainted in place: the recalled rerun shows as a second frozen prompt row.
check(
  "the recalled command froze on its own prompt row",
  promptLines.filter((line) => /^eosh> hello --name tabbed\s*$/.test(line)).length === 2,
  `prompt rows: ${JSON.stringify(promptLines)}`,
);

// The stderr in-band marker must never reach the transcript.
check(
  "no U+0001 marker bytes in rendered rows",
  lines.every((line) => !line.includes("\u0001")),
);

// The error line carries the page's error styling (vm.js routed it through the marker).
check(
  "the stderr line got the vm-error class",
  spansWithClass("vm-error").some((text) => text.includes("cannot resolve")),
);

// No raw escape bytes may ever reach the DOM (the render layer consumes the whole CSI
// alphabet and ignores the rest — the fbcon posture).
check(
  "no raw ESC bytes in rendered rows",
  lines.every((line) => !line.includes("\u001b")),
);

const failed = checks.filter(([, ok]) => !ok).length;
console.log(`${checks.length - failed}/${checks.length} rendering checks passed`);
process.exit(failed === 0 ? 0 : 1);
