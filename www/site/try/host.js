// The eo9.org /try browser host (hand-written, no dependencies).
//
// This module plays the role the eo9 usermode runtime plays natively: it provides the root
// capabilities that the transpiled example components import — eo9:text (wired to the in-page
// terminal), eo9:time (the browser's clock), eo9:fs (an in-memory filesystem that lives for the
// page session) — plus the eo9:io buffer resource they move bytes with. The components
// themselves are the real Eo9 example programs, translated from the Component Model binary
// format into ES modules + core wasm at build time (see www/try-build); nothing here simulates
// program behavior.

// --- Resource classes -------------------------------------------------------------------------
// These are the JS representations of the imported WIT resources. The transpiled components call
// the functions below with these objects as handles; the host owns their representation.

export class TextImpl {}
export class TimeImpl {}
export class FsImpl {}

export class FsFile {
  constructor(path) {
    this.path = path;
  }
}

/** eo9:io/buffers `buffer`: an owned block of memory, transferred to a backend and back. */
export class IoBuffer {
  constructor(len) {
    this.bytes = new Uint8Array(Number(len));
  }
  len() {
    return BigInt(this.bytes.length);
  }
  read(offset, len) {
    const start = Number(offset);
    const count = Number(len);
    if (start + count > this.bytes.length) {
      throw new Error(`buffer read out of bounds (${start}+${count} > ${this.bytes.length})`);
    }
    return this.bytes.slice(start, start + count);
  }
  write(offset, data) {
    const start = Number(offset);
    if (start + data.length > this.bytes.length) {
      throw new Error(`buffer write out of bounds (${start}+${data.length} > ${this.bytes.length})`);
    }
    this.bytes.set(data, start);
  }
}

// --- The in-page filesystem -------------------------------------------------------------------

/** A tiny in-memory filesystem: path -> Uint8Array. It persists for the page session, so a file
 * written by one run (`readwrite`) is still there for the next, and `files` can show it. */
export class MemFs {
  constructor() {
    this.files = new Map();
  }
  normalize(path) {
    const parts = [];
    for (const segment of String(path).split('/')) {
      if (segment === '' || segment === '.') continue;
      if (segment === '..') {
        parts.pop();
        continue;
      }
      parts.push(segment);
    }
    return '/' + parts.join('/');
  }
  entries() {
    return [...this.files.entries()].sort(([a], [b]) => (a < b ? -1 : 1));
  }
}

// --- Host construction ------------------------------------------------------------------------

const utf8 = new TextDecoder();

/**
 * Build the import object for one program run.
 *
 * `grants` decides which capability interfaces exist at all — the launcher refuses a program
 * whose required imports are not granted before it ever gets here, mirroring the eo9 loader
 * rule. Types-only interfaces and the shared buffer resource carry no authority and are always
 * present.
 *
 * @param {{text: boolean, time: boolean, fs: boolean}} grants
 * @param {{write: (stream: string, text: string) => void}} terminal sink for eo9:text output
 * @param {MemFs} memfs the page-session filesystem backing eo9:fs
 */
export function makeImports(grants, terminal, memfs) {
  const theText = new TextImpl();
  const theTime = new TimeImpl();
  const theFs = new FsImpl();

  const imports = {
    'eo9:io/buffers': { Buffer: IoBuffer },
    'eo9:text/types': { TextImpl },
    'eo9:time/types': { TimeImpl },
    'eo9:fs/types': { FsImpl },
  };

  if (grants.text) {
    imports['eo9:text/text'] = {
      default: () => theText,
      write: (_t, to, text) => {
        terminal.write(to, text);
      },
      // One line of stdin per call; the page has no stdin stream, so programs that ask see
      // end-of-input. (None of the shipped examples read stdin.)
      readLine: async () => null,
    };
  }

  if (grants.time) {
    imports['eo9:time/time'] = {
      default: () => theTime,
      now: () => {
        const ms = Date.now();
        return {
          seconds: BigInt(Math.floor(ms / 1000)),
          nanoseconds: (ms % 1000) * 1_000_000,
        };
      },
    };
  }

  if (grants.fs) {
    imports['eo9:fs/fs'] = {
      File: FsFile,
      default: () => theFs,
      open: async (_fs, path, flags) => {
        const normalized = memfs.normalize(path);
        const exists = memfs.files.has(normalized);
        if (!exists && !flags.create) {
          throw fsError('not-found');
        }
        if (!exists || flags.truncate) {
          memfs.files.set(normalized, new Uint8Array(0));
        }
        return new FsFile(normalized);
      },
      write: async (file, offset, src) => {
        const start = Number(offset);
        const data = src.bytes;
        const existing = memfs.files.get(file.path) ?? new Uint8Array(0);
        const grown = new Uint8Array(Math.max(existing.length, start + data.length));
        grown.set(existing);
        grown.set(data, start);
        memfs.files.set(file.path, grown);
        return [src, { tag: 'ok', val: { bytesWritten: BigInt(data.length) } }];
      },
      read: async (file, offset, dst) => {
        const start = Number(offset);
        const existing = memfs.files.get(file.path) ?? new Uint8Array(0);
        const available = Math.max(0, existing.length - start);
        const count = Math.min(dst.bytes.length, available);
        dst.bytes.set(existing.slice(start, start + count));
        return [dst, { tag: 'ok', val: { bytesRead: BigInt(count) } }];
      },
    };
  }

  return imports;
}

/** An eo9:fs `fs-error` carried on a thrown error, for the open path's result<file, fs-error>. */
function fsError(tag, detail) {
  const error = new Error(detail ? `${tag}: ${detail}` : tag);
  error.payload = detail === undefined ? { tag } : { tag, val: detail };
  return error;
}

/** Decode file bytes for the `files` builtin (lossy for non-UTF-8 content, which is fine for display). */
export function decodeText(bytes) {
  return utf8.decode(bytes);
}
