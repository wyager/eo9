// The Eo9 web VM page: load the blob (wasmtime + Pulley + embedded Eo9 component images,
// compiled to wasm32) and wire its one import — `env.host_write`, a UTF-8 line of output —
// to the page. Everything Eo9-shaped happens inside the blob; this file is just a terminal.

const output = document.getElementById("vm-output");
const buttons = {
  hello: document.getElementById("btn-hello"),
  fuel: document.getElementById("btn-fuel"),
  entropy: document.getElementById("btn-entropy"),
};
const seedInput = document.getElementById("seed");
const countInput = document.getElementById("count");

let memory = null;
const decoder = new TextDecoder();

function writeLine(text, cls) {
  const line = document.createElement("div");
  if (cls) line.className = cls;
  line.textContent = text;
  output.appendChild(line);
  output.scrollTop = output.scrollHeight;
}

function hostWrite(ptr, len) {
  const bytes = new Uint8Array(memory.buffer, ptr, len);
  writeLine(decoder.decode(bytes));
}

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

async function main() {
  let exports;
  try {
    const response = await fetch("/vm/web-eo9.wasm");
    if (!response.ok) throw new Error(`fetch failed: ${response.status}`);
    const { instance } = await WebAssembly.instantiateStreaming(response, {
      env: { host_write: hostWrite },
    });
    exports = instance.exports;
    memory = exports.memory;
  } catch (error) {
    output.textContent = "";
    writeLine(`could not load the Eo9 blob: ${error}`, "vm-error");
    writeLine("(this page needs a browser with WebAssembly enabled)", "vm-error");
    return;
  }

  output.textContent = "";
  const failures = exports.boot();
  if (failures !== 0) {
    writeLine("boot reported a failure — see above", "vm-error");
    return;
  }

  const run = (name, fn) => {
    writeLine(`· ${name}`, "vm-cmd");
    try {
      const code = fn();
      if (code !== 0) writeLine(`${name}: failed (see above)`, "vm-error");
    } catch (error) {
      writeLine(`${name}: trapped: ${error}`, "vm-error");
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

  for (const button of Object.values(buttons)) button.disabled = false;
}

main();
