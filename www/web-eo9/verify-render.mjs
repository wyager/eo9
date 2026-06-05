// Node (v25, JSPI) RENDERING harness: runs the page's actual vm.js against the real blob
// with a minimal DOM stand-in, drives the keyboard exactly as a visitor would, and asserts
// on the rendered transcript lines. The other harnesses feed the blob through their own
// host_write and never exercise vm.js's one-line-per-write rendering contract — which is
// exactly how the prompt-accumulation regression (a partial `eosh> ` buffered in the blob,
// flushed later glued to other lines) shipped invisibly. Run after `cargo xtask build-web-vm`:
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

const transcriptLines = () => output.children.map((child) => child.textContent);
const promptCount = () => transcriptLines().filter((line) => line.startsWith("eosh>")).length;

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

function key(k) {
  listeners.keydown({
    key: k,
    metaKey: false,
    ctrlKey: false,
    altKey: false,
    target: null,
    preventDefault() {},
  });
}

async function type(command) {
  // The shell is reading iff a prompt line was rendered and armReadLine attached to it;
  // wait for the prompt count to grow past the previously-consumed one.
  for (const ch of command) key(ch);
  key("Enter");
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

// A normal command (stdout from the child + the shell's own outcome line).
await type("hello");
await waitFor("hello outcome", () => transcriptLines().some((l) => l.includes("ok: greeted")));
await atPrompt();

// A multi-line builtin.
await type("help");
await waitFor("help output", () => transcriptLines().some((l) => l.includes("explore the sandbox")));
await atPrompt();

// A standard-error line (unresolvable command).
await type("nosuchprogram");
await waitFor("the error line", () => transcriptLines().some((l) => l.includes("cannot resolve")));
await atPrompt();

// Rapid sequential entry: submit the moment the prompt re-arms.
await type("ls /bin");
await waitFor("ls outcome", () => transcriptLines().some((l) => l.includes("listed(")));
await atPrompt();

// --- arrow-key recall (the live prompt line is the last transcript line) -------------------
const liveLineText = () => transcriptLines()[transcriptLines().length - 1] ?? "";

// Up recalls the newest command onto the live line; Enter reruns it.
key("ArrowUp");
await waitFor("recall of ls /bin", () => liveLineText().includes("ls /bin"));
const listedBefore = transcriptLines().filter((l) => l.includes("listed(")).length;
key("Enter");
await waitFor(
  "the recalled command reran",
  () => transcriptLines().filter((l) => l.includes("listed(")).length > listedBefore,
);
await atPrompt();

// Up-up reaches the older entry; Down comes back; editing a recalled line commits it.
key("ArrowUp"); // ls /bin
await waitFor("recall newest again", () => liveLineText().includes("ls /bin"));
key("ArrowUp"); // nosuchprogram
await waitFor("recall the older entry", () => liveLineText().includes("nosuchprogram"));
key("ArrowDown"); // back down to ls /bin
await waitFor("down moves back to the newer entry", () => liveLineText().includes("ls /bin"));

// Down past the newest restores the stashed fresh line (typed, never submitted).
key("ArrowDown"); // leave browsing: the (empty) fresh line returns
for (const ch of "echo stash kept") key(ch);
await waitFor("typed a fresh line", () => liveLineText().includes("echo stash kept"));
key("ArrowUp");
await waitFor("browsed away from the fresh line", () => liveLineText().includes("ls /bin"));
key("ArrowDown");
await waitFor("the stash came back", () => liveLineText().includes("echo stash kept"));
key("Enter");
await waitFor("the restored line ran (variadic echo)", () =>
  transcriptLines().some((l) => l.trim() === "stash kept"),
);
await atPrompt();

await type("exit");
await waitFor("session end", () => transcriptLines().some((l) => l.includes("session ended")));

// --- assertions -------------------------------------------------------------------------------

const lines = transcriptLines();

// THE regression: a transcript line must never carry more than one `eosh>`.
const accumulated = lines.filter((line) => line.split("eosh>").length > 2);
check(
  "no line accumulates more than one eosh> prefix",
  accumulated.length === 0,
  `offenders: ${JSON.stringify(accumulated.slice(0, 3))}`,
);

// Prompts are their own lines: every eosh> sits at the start of its line (the page's own
// "·" status lines may mention the prompt by name).
const misplaced = lines.filter((line) => {
  const at = line.indexOf("eosh>");
  return at > 0 && !line.startsWith("\u00b7 ");
});
check(
  "every eosh> prompt starts its line",
  misplaced.length === 0,
  `offenders: ${JSON.stringify(misplaced.slice(0, 3))}`,
);

// The typed command froze on its prompt line.
check(
  "the typed command renders on the prompt line",
  lines.some((line) => /^eosh> hello\s*$/.test(line)),
  `prompt lines: ${JSON.stringify(lines.filter((l) => l.includes("eosh>")).slice(0, 8))}`,
);

// Output and outcome lines are their own lines, not glued behind a prompt.
check(
  "the outcome line is not glued to a prompt",
  lines.some((line) => /^ok: greeted$/.test(line.trim())),
);
check(
  "child stdout is its own line",
  lines.some((line) => /Hello, world/.test(line) && !line.includes("eosh>")),
);

// One prompt line per read: 9 commands were typed (2 empty, hello, help, nosuchprogram,
// ls /bin, the recalled rerun, the stash-restored echo, exit), so exactly 9 lines start
// with the prompt — arrow browsing itself must never mint extra prompt lines.
const promptLines = lines.filter((line) => line.startsWith("eosh>"));
check(
  "exactly one prompt line per command read",
  promptLines.length === 9,
  `got ${promptLines.length}: ${JSON.stringify(promptLines)}`,
);

// Recall renders in place: the rerun shows as a frozen `eosh> ls /bin` line a second time.
check(
  "the recalled command froze on its own prompt line",
  promptLines.filter((line) => /^eosh> ls \/bin\s*$/.test(line)).length === 2,
  `prompt lines: ${JSON.stringify(promptLines)}`,
);

// The stderr in-band marker must never reach the transcript.
check(
  "no U+0001 marker bytes in rendered lines",
  lines.every((line) => !line.includes("\u0001")),
);

// The error line carries the page's error styling (vm.js routed it through the marker).
check(
  "the stderr line got the vm-error class",
  output.children.some(
    (child) =>
      child instanceof FakeElement &&
      child.className === "vm-error" &&
      child.textContent.includes("cannot resolve"),
  ),
);

const failed = checks.filter(([, ok]) => !ok).length;
console.log(`${checks.length - failed}/${checks.length} rendering checks passed`);
process.exit(failed === 0 ? 0 : 1);
