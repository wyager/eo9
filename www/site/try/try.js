// The eo9.org /try page: a small in-page terminal and launcher for the real Eo9 example
// components (hand-written, no dependencies).
//
// Honesty note, also stated on the page and in `about`: this prompt is NOT eosh, the Eo9 shell.
// It is a launcher that does what the eo9 usermode runtime does for a single program — check the
// program's imports against what the host provides (refusing before execution if a required
// capability is missing), bind typed flags to main's signature, instantiate, and render the
// typed outcome. The programs themselves are the real Eo9 components; their behavior is theirs.

import { MemFs, decodeText, makeImports } from './host.js';

// --- Page state --------------------------------------------------------------------------------

const hasJSPI = typeof WebAssembly.Suspending === 'function';

const state = {
  manifest: null, // loaded from components/manifest.json
  grants: { text: true, time: true, fs: true },
  memfs: new MemFs(),
  modules: new Map(), // component name -> import() promise
  history: [],
  historyIndex: -1,
  running: false,
};

/** The capabilities this browser host can provide, keyed by the short name used by grant/revoke. */
const CAPABILITIES = {
  text: { iface: 'eo9:text/text', what: 'standard output, wired to this terminal' },
  time: { iface: 'eo9:time/time', what: "wall-clock time from your browser's clock" },
  fs: { iface: 'eo9:fs/fs', what: 'an in-memory filesystem that lives in this page' },
};

/** Types-only interfaces and the shared buffer resource: no authority, always available. */
const PLUMBING = new Set(['eo9:text/types', 'eo9:time/types', 'eo9:fs/types', 'eo9:io/buffers']);

// --- Terminal ----------------------------------------------------------------------------------

const outputEl = document.getElementById('term-output');
const inputEl = document.getElementById('term-input');
const screenEl = document.getElementById('term');

let currentLine = null;

function ensureLine() {
  if (!currentLine) {
    currentLine = document.createElement('div');
    currentLine.className = 'line';
    outputEl.appendChild(currentLine);
  }
  return currentLine;
}

/** Stream text into the terminal; '\n' ends the current line. `cls` styles the appended spans. */
function write(text, cls) {
  const segments = String(text).split('\n');
  segments.forEach((segment, index) => {
    if (index > 0) {
      currentLine = null;
      ensureLine();
    }
    if (segment) {
      const span = document.createElement('span');
      if (cls) span.className = cls;
      span.textContent = segment;
      ensureLine().appendChild(span);
    }
  });
  screenEl.scrollTop = screenEl.scrollHeight;
}

function println(text = '', cls) {
  write(text + '\n', cls);
}

/** The eo9:text sink handed to the host: program output, kept visually distinct from launcher text. */
const programSink = {
  write: (stream, text) => write(text, stream === 'err' ? 'prog-err' : 'prog-out'),
};

// --- Value rendering ---------------------------------------------------------------------------

/** Render a lifted Component Model value the way the launcher prints outcomes (WAVE-flavored). */
function renderValue(value) {
  if (value === null || value === undefined) return 'none';
  if (typeof value === 'bigint' || typeof value === 'number' || typeof value === 'boolean') {
    return String(value);
  }
  if (typeof value === 'string') return JSON.stringify(value);
  if (value instanceof Uint8Array) return `<${value.length} bytes>`;
  if (Array.isArray(value)) return `(${value.map(renderValue).join(', ')})`;
  if (typeof value === 'object') {
    if ('tag' in value) {
      return value.val === undefined ? value.tag : `${value.tag}(${renderValue(value.val)})`;
    }
    const fields = Object.entries(value).map(([key, val]) => `${key}: ${renderValue(val)}`);
    return `{${fields.join(', ')}}`;
  }
  return String(value);
}

// --- Capability checks (the loader rule, performed by the launcher before instantiation) --------

function capabilityFor(interfaceName) {
  const base = interfaceName.split('@')[0];
  if (PLUMBING.has(base)) return { kind: 'plumbing' };
  for (const [name, info] of Object.entries(CAPABILITIES)) {
    if (info.iface === base) return { kind: 'capability', name };
  }
  return { kind: 'unsupported' };
}

/** Returns a list of human-readable refusal reasons; empty means the program may run. */
function refusalReasons(entry) {
  const reasons = [];
  for (const imp of entry.imports) {
    const cap = capabilityFor(imp.interface);
    if (cap.kind === 'plumbing') continue;
    if (cap.kind === 'unsupported') {
      if (imp.required) {
        reasons.push(`required import ${imp.interface} is not something this browser host can provide`);
      }
      continue;
    }
    if (imp.required && !state.grants[cap.name]) {
      reasons.push(
        `missing required import ${imp.interface} — the "${cap.name}" capability is not granted (try \`grant ${cap.name}\`)`,
      );
    }
  }
  return reasons;
}

// --- Typed flags -------------------------------------------------------------------------------

function usage(entry) {
  const flags = entry.params.map((p) => `--${p.name} <${p.ty}>`).join(' ');
  return `usage: ${entry.name}${flags ? ' ' + flags : ''}`;
}

/** Parse `--flag value` tokens against main's typed signature. Returns { values } or { error }. */
function parseFlags(entry, tokens) {
  const given = new Map();
  for (let i = 0; i < tokens.length; i += 2) {
    const flag = tokens[i];
    if (!flag.startsWith('--')) return { error: `expected a --flag, got \`${flag}\`\n${usage(entry)}` };
    if (i + 1 >= tokens.length) return { error: `flag ${flag} needs a value\n${usage(entry)}` };
    given.set(flag.slice(2), tokens[i + 1]);
  }

  const values = [];
  for (const param of entry.params) {
    if (!given.has(param.name)) {
      return { error: `missing required flag --${param.name}\n${usage(entry)}` };
    }
    const raw = given.get(param.name);
    given.delete(param.name);
    const converted = convertFlag(raw, param.ty);
    if (converted.error) {
      return { error: `--${param.name}: ${converted.error}\n${usage(entry)}` };
    }
    values.push(converted.value);
  }
  if (given.size > 0) {
    return { error: `unknown flag --${[...given.keys()][0]}\n${usage(entry)}` };
  }
  return { values };
}

/** Convert one flag's text to the JS value the transpiled component expects for its WIT type. */
function convertFlag(raw, ty) {
  switch (ty) {
    case 'string':
      return { value: raw };
    case 'bool':
      if (raw === 'true') return { value: true };
      if (raw === 'false') return { value: false };
      return { error: `expected true or false, got \`${raw}\`` };
    case 'u64':
    case 's64': {
      if (!/^-?\d+$/.test(raw) || (ty === 'u64' && raw.startsWith('-'))) {
        return { error: `expected a ${ty}, got \`${raw}\`` };
      }
      try {
        return { value: BigInt(raw) };
      } catch {
        return { error: `expected a ${ty}, got \`${raw}\`` };
      }
    }
    case 'u8':
    case 'u16':
    case 'u32':
    case 's8':
    case 's16':
    case 's32': {
      const n = Number(raw);
      if (!Number.isInteger(n) || (ty.startsWith('u') && n < 0)) {
        return { error: `expected a ${ty}, got \`${raw}\`` };
      }
      return { value: n };
    }
    case 'f32':
    case 'f64': {
      const f = Number(raw);
      if (Number.isNaN(f) && raw !== 'nan') return { error: `expected a ${ty}, got \`${raw}\`` };
      return { value: f };
    }
    default:
      return { error: `the launcher only parses primitive flag types (this one is \`${ty}\`)` };
  }
}

// --- Running a program -------------------------------------------------------------------------

function loadModule(entry) {
  if (!state.modules.has(entry.name)) {
    state.modules.set(entry.name, import(`./components/${entry.module}`));
  }
  return state.modules.get(entry.name);
}

function coreModuleLoader(entry) {
  return async (file) => {
    const url = `./components/${entry.name}/${file}`;
    const response = await fetch(url);
    if (!response.ok) throw new Error(`failed to fetch ${url}: HTTP ${response.status}`);
    if (WebAssembly.compileStreaming) {
      try {
        return await WebAssembly.compileStreaming(response.clone());
      } catch {
        // Fall through to ArrayBuffer compilation (e.g. an unexpected content type).
      }
    }
    return WebAssembly.compile(await response.arrayBuffer());
  };
}

async function runProgram(entry, flagTokens) {
  if (entry.asyncMain && !hasJSPI) {
    println(
      `${entry.name} has an async main, which the transpiled form drives with JSPI ` +
        `(WebAssembly.Suspending) — and this browser does not support JSPI yet. ` +
        `Recent Chromium-based browsers do.`,
      'launcher-err',
    );
    return;
  }

  const reasons = refusalReasons(entry);
  if (reasons.length > 0) {
    println(`refused before execution (the loader rule — nothing was instantiated):`, 'launcher-err');
    for (const reason of reasons) println(`  ${reason}`, 'launcher-err');
    return;
  }

  const flags = parseFlags(entry, flagTokens);
  if (flags.error) {
    println(flags.error, 'launcher-err');
    return;
  }

  let module;
  try {
    module = await loadModule(entry);
  } catch (err) {
    println(`failed to load the transpiled component: ${err}`, 'launcher-err');
    return;
  }

  const imports = makeImports(state.grants, programSink, state.memfs);
  const started = performance.now();
  try {
    const instance = await module.instantiate(coreModuleLoader(entry), imports);
    let outcome = instance.main(...flags.values);
    if (outcome instanceof Promise) outcome = await outcome;
    println(`outcome = success(${renderValue(outcome)})`, 'launcher');
  } catch (err) {
    if (err && err.payload !== undefined) {
      println(`outcome = failure(${renderValue(err.payload)})`, 'launcher');
    } else {
      const message = err && err.message ? err.message : String(err);
      println(`outcome = abnormal(trapped(${JSON.stringify(message)}))`, 'launcher-err');
    }
  }
  println(`(${(performance.now() - started).toFixed(1)} ms)`, 'dim');
}

// --- Builtins ----------------------------------------------------------------------------------

function findComponent(name) {
  return state.manifest?.components.find((c) => c.name === name);
}

function cmdHelp() {
  println('launcher commands (this is not eosh — see `about`):');
  println('  list                     the programs available on this page');
  println('  <name> [--flag value …]  run a program (also: run <name> …)');
  println('  describe <name>          a program\'s imports, typed flags, and outcome variants');
  println('  grants                   what the browser host currently provides');
  println('  grant <cap> / revoke <cap>   change what the next run receives (text, time, fs)');
  println('  files                    the in-page filesystem\'s contents');
  println('  about                    what is real on this page and what is not');
  println('  clear                    clear the terminal');
}

function cmdAbout() {
  println('What is real here:', 'launcher');
  println('  • The programs are the actual Eo9 example components from the repository, translated');
  println('    1:1 from the Component Model binary format into JS + core wasm and executed by your');
  println('    browser\'s own WebAssembly engine.');
  println('  • Their imports are their capability set. Before a run, this launcher checks every');
  println('    import against what the host provides and refuses the program if a required one is');
  println('    missing — the same rule the eo9 loader applies, which you can try with `revoke`.');
  println('  • The browser host provides the root capabilities: eo9:text → this terminal,');
  println('    eo9:time → your clock, eo9:fs → an in-memory filesystem in this page.');
  println('What is not here (yet):', 'launcher');
  println('  • eosh, the real Eo9 shell, and the composition algebra ($, &, only, configure with');
  println('    stub providers). This prompt is a hand-written launcher, not eosh.');
  println('  • The native compiler. On a real Eo9 system providers are fused into the program and');
  println('    compiled to machine code; here the browser\'s wasm engine runs each component.');
  println('See the text below the terminal for the full story.');
}

function cmdList() {
  println('programs on this page:');
  for (const entry of state.manifest.components) {
    const jspi = entry.asyncMain && !hasJSPI ? '  [needs JSPI — unavailable in this browser]' : '';
    println(`  ${entry.name.padEnd(10)} ${entry.summary}${jspi}`);
  }
  println('');
  println(`try: ${state.manifest.components[0].example}`);
}

function cmdDescribe(name) {
  const entry = findComponent(name);
  if (!entry) {
    println(`describe: no such program \`${name}\` (see \`list\`)`, 'launcher-err');
    return;
  }
  println(`${entry.name} — ${entry.summary}`);
  println(`  kind:     binary (main: ${entry.asyncMain ? 'async func' : 'func'})`);
  if (entry.imports.length === 0) {
    println('  imports:  (none — a fully closed program; it can affect nothing outside itself)');
  } else {
    println('  imports:');
    for (const imp of entry.imports) {
      const cap = capabilityFor(imp.interface);
      let note;
      if (cap.kind === 'plumbing') {
        note = 'types/buffers only — carries no authority';
      } else if (cap.kind === 'capability') {
        note = `capability "${cap.name}": ${state.grants[cap.name] ? 'granted' : 'not granted'}`;
      } else {
        note = 'not provided by this browser host';
      }
      println(`    ${imp.interface}  (${imp.required ? 'required' : 'optional'}; ${note})`);
    }
  }
  const flags = entry.params.map((p) => `--${p.name} <${p.ty}>`).join('  ');
  println(`  flags:    ${flags || '(none)'}`);
  if (entry.success.length) println(`  success:  ${entry.success.join(', ')}`);
  if (entry.failure.length) println(`  failure:  ${entry.failure.join(', ')}`);
  println(`  example:  ${entry.example}`);
}

function cmdGrants() {
  println('capabilities provided by the browser host:');
  for (const [name, info] of Object.entries(CAPABILITIES)) {
    const status = state.grants[name] ? 'granted' : 'revoked';
    println(`  ${name.padEnd(5)} ${status.padEnd(8)} ${info.iface} — ${info.what}`);
  }
  println('`grant <name>` / `revoke <name>` change what the next run receives.');
}

function cmdGrantRevoke(which, name) {
  if (!name || !(name in CAPABILITIES)) {
    println(`${which}: expected one of: ${Object.keys(CAPABILITIES).join(', ')}`, 'launcher-err');
    return;
  }
  state.grants[name] = which === 'grant';
  println(`${name} is now ${state.grants[name] ? 'granted' : 'revoked'} for subsequent runs.`);
}

function cmdFiles() {
  const entries = state.memfs.entries();
  if (entries.length === 0) {
    println('the in-page filesystem is empty (try `' + (findComponent('readwrite')?.example ?? 'readwrite') + '`)');
    return;
  }
  println('the in-page filesystem (persists until you reload the page):');
  for (const [path, bytes] of entries) {
    const preview = bytes.length <= 120 ? `  ${JSON.stringify(decodeText(bytes))}` : '';
    println(`  ${path}  (${bytes.length} bytes)${preview}`);
  }
}

function cmdClear() {
  outputEl.textContent = '';
  currentLine = null;
}

// --- Command loop ------------------------------------------------------------------------------

/** Split a command line into tokens, honoring single and double quotes. */
function tokenize(line) {
  const tokens = [];
  let i = 0;
  while (i < line.length) {
    while (i < line.length && /\s/.test(line[i])) i += 1;
    if (i >= line.length) break;
    let token = '';
    while (i < line.length && !/\s/.test(line[i])) {
      const ch = line[i];
      if (ch === '"' || ch === "'") {
        const closing = line.indexOf(ch, i + 1);
        if (closing === -1) return { error: 'unterminated quote' };
        token += line.slice(i + 1, closing);
        i = closing + 1;
      } else {
        token += ch;
        i += 1;
      }
    }
    tokens.push(token);
  }
  return { tokens };
}

async function execute(line) {
  const parsed = tokenize(line);
  if (parsed.error) {
    println(parsed.error, 'launcher-err');
    return;
  }
  const tokens = parsed.tokens;
  if (tokens.length === 0) return;
  const [command, ...rest] = tokens;

  switch (command) {
    case 'help':
      return cmdHelp();
    case 'about':
      return cmdAbout();
    case 'list':
    case 'ls':
    case 'store':
      return cmdList();
    case 'describe':
      return cmdDescribe(rest[0]);
    case 'grants':
      return cmdGrants();
    case 'grant':
    case 'revoke':
      return cmdGrantRevoke(command, rest[0]);
    case 'files':
      return cmdFiles();
    case 'clear':
      return cmdClear();
    case 'run': {
      const entry = findComponent(rest[0]);
      if (!entry) {
        println(`run: no such program \`${rest[0] ?? ''}\` (see \`list\`)`, 'launcher-err');
        return;
      }
      return runProgram(entry, rest.slice(1));
    }
    default: {
      const entry = findComponent(command);
      if (entry) return runProgram(entry, rest);
      println(`unknown command \`${command}\` — \`help\` lists what this launcher can do`, 'launcher-err');
    }
  }
}

async function onSubmit() {
  if (state.running) return;
  const line = inputEl.value;
  inputEl.value = '';
  println(`try> ${line}`, 'echo');
  const trimmed = line.trim();
  if (trimmed) {
    state.history.push(trimmed);
  }
  state.historyIndex = state.history.length;
  if (!state.manifest) {
    println('the component bundle did not load; see the message above.', 'launcher-err');
    return;
  }
  state.running = true;
  inputEl.disabled = true;
  try {
    await execute(trimmed);
  } catch (err) {
    println(`launcher error: ${err && err.message ? err.message : err}`, 'launcher-err');
  } finally {
    state.running = false;
    inputEl.disabled = false;
    inputEl.focus();
  }
}

function onKeyDown(event) {
  if (event.key === 'Enter') {
    event.preventDefault();
    onSubmit();
  } else if (event.key === 'ArrowUp') {
    event.preventDefault();
    if (state.historyIndex > 0) {
      state.historyIndex -= 1;
      inputEl.value = state.history[state.historyIndex];
    }
  } else if (event.key === 'ArrowDown') {
    event.preventDefault();
    if (state.historyIndex < state.history.length - 1) {
      state.historyIndex += 1;
      inputEl.value = state.history[state.historyIndex];
    } else {
      state.historyIndex = state.history.length;
      inputEl.value = '';
    }
  }
}

// --- Startup -----------------------------------------------------------------------------------

async function start() {
  inputEl.addEventListener('keydown', onKeyDown);
  screenEl.addEventListener('click', () => inputEl.focus());

  println('Eo9 /try — real Eo9 example components, running on your browser\'s WebAssembly engine.', 'launcher');
  println('This prompt is a small launcher, not eosh. `help` lists commands; `about` says what is real here.', 'launcher');
  if (!hasJSPI) {
    println('note: this browser has no JSPI (WebAssembly.Suspending), so programs with an async main', 'launcher-err');
    println('      (readwrite) cannot run here; recent Chromium-based browsers support it.', 'launcher-err');
  }
  println('');

  try {
    const response = await fetch('./components/manifest.json');
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    state.manifest = await response.json();
  } catch (err) {
    println(`could not load the component bundle (${err}); if you are running the site from a`, 'launcher-err');
    println('checkout, generate it with `cargo xtask build-web-demo`.', 'launcher-err');
    inputEl.focus();
    return;
  }

  cmdList();
  println('');
  inputEl.focus();
}

start();
