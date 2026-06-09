#!/usr/bin/env python3
"""Per-keystroke echo-latency probe for the eosh editor under QEMU (study 33,
docs/study/parser-step-cost.md).

Boots the aarch64 kernel image (build it first: `cargo xtask build-kernel aarch64`)
under qemu-system-aarch64 and measures, for every byte sent to the console, the wall
time from write() to that byte's echo appearing on the serial stream. The guest
editor's echo is emitted AFTER the parser work for the keystroke (step + the
name-tracking completions walk), so green-character echo latency = transport
round-trip + editor keystroke cost, and the probe separates them with two controls:

  * RED region   — after a dead character the editor stops stepping the parser
                   entirely (insert_char short-circuits on red_from); every later
                   character costs transport + O(1) editor bookkeeping only.
  * COMMENT body — after `# ` the live state is a tiny CommentRest tail; the step is
                   real but near-free.

Three passes over the console, one boot:
  1. the 87-byte demo line (typing cost per position), then Ctrl-C
  2. `help xxxxxxxx…` (red-region control), then Ctrl-C
  3. `# xxxxxxxx…`    (comment control), then Ctrl-C, then `poweroff`

Usage:
  python3 tests/step-cost/echolat.py [--accel hvf|tcg] [--out FILE]

The default accelerator is hvf (native execution on Apple Silicon — same ISA family
as the board's Cortex-A76, our cranelift codegen, in-wasm dlmalloc). Results print
as TSV: pass, position, byte, latency_us.
"""

import argparse
import os
import subprocess
import sys
import time

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
IMAGE = os.path.join(
    REPO, "kernel", "target", "aarch64-unknown-none", "release", "eo9-kernel"
)

DEMO_LINE = (
    "net.rtl8125 --advertise-max 1000 $ net.l4.over-l2 --address dhcp"
    " $ curl http://yager.io"
)
PROMPT = b"eosh> "
CTRL_C = b"\x03"
STEP_TIMEOUT = 120.0  # generous: first prompt waits for on-target compiles


def read_until(proc, marker, deadline_s, sink):
    """Read stdout bytes until `marker` is seen; returns the bytes read."""
    seen = bytearray()
    deadline = time.monotonic() + deadline_s
    while marker not in seen:
        if time.monotonic() > deadline:
            raise TimeoutError(
                f"timed out waiting for {marker!r}; last output: "
                f"{bytes(seen[-400:])!r}"
            )
        byte = proc.stdout.read(1)
        if not byte:
            raise EOFError(f"serial stream closed; last: {bytes(seen[-400:])!r}")
        seen += byte
        sink += byte
    return bytes(seen)


def latency_pass(proc, name, payload, results, sink, settle_s=0.05):
    """Send `payload` one byte at a time; record write→echo latency per byte.

    The guest echoes printable bytes verbatim (possibly preceded by marker escape
    sequences, e.g. SGR 31 before the first dead byte); waiting for the byte itself
    skips any prefix. A settle pause between keys keeps keystrokes from pipelining
    (we want per-key cost, not throughput).
    """
    for index, byte in enumerate(payload):
        want = bytes([byte])
        t0 = time.monotonic()
        proc.stdin.write(want)
        proc.stdin.flush()
        read_until(proc, want, STEP_TIMEOUT, sink)
        dt_us = (time.monotonic() - t0) * 1e6
        results.append((name, index, chr(byte), dt_us))
        time.sleep(settle_s)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--accel", choices=["hvf", "tcg"], default="hvf")
    parser.add_argument("--out", default=None, help="also write the TSV here")
    args = parser.parse_args()

    if not os.path.exists(IMAGE):
        sys.exit(f"kernel image missing: {IMAGE}\nrun: cargo xtask build-kernel aarch64")

    cmd = ["qemu-system-aarch64"]
    if args.accel == "hvf":
        cmd += ["-accel", "hvf", "-cpu", "host"]
    else:
        cmd += ["-cpu", "max"]
    cmd += [
        "-M", "virt,gic-version=2,highmem=off",
        "-device", "virtio-rng-pci",
        "-smp", "1",
        "-m", "512M",
        "-nographic",
        "-kernel", IMAGE,
    ]
    print(f"# {' '.join(cmd)}", file=sys.stderr)
    proc = subprocess.Popen(
        cmd,
        cwd=REPO,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        bufsize=0,
    )
    transcript = bytearray()
    results = []
    try:
        read_until(proc, PROMPT, STEP_TIMEOUT, transcript)
        # Pass 1: the demo line (green; name words are completions-tracked).
        latency_pass(proc, "demo", DEMO_LINE.encode(), results, transcript)
        proc.stdin.write(CTRL_C)
        proc.stdin.flush()
        read_until(proc, PROMPT, STEP_TIMEOUT, transcript)

        # Pass 2: red-region control. `help x` goes dead at the x; everything after
        # is echo-only (no parser work at all).
        latency_pass(proc, "red-arm", b"help x", results, transcript)
        latency_pass(proc, "red", b"x" * 40, results, transcript)
        proc.stdin.write(CTRL_C)
        proc.stdin.flush()
        read_until(proc, PROMPT, STEP_TIMEOUT, transcript)

        # Pass 3: comment control. After `# ` the state is a CommentRest tail: the
        # parser steps, but over a near-empty tree.
        latency_pass(proc, "comment-arm", b"# ", results, transcript)
        latency_pass(proc, "comment", b"x" * 40, results, transcript)
        proc.stdin.write(CTRL_C)
        proc.stdin.flush()
        read_until(proc, PROMPT, STEP_TIMEOUT, transcript)

        proc.stdin.write(b"poweroff\r")
        proc.stdin.flush()
        try:
            proc.wait(timeout=30)
        except subprocess.TimeoutExpired:
            proc.kill()
    finally:
        if proc.poll() is None:
            proc.kill()
        proc.wait()

    lines = ["pass\tpos\tbyte\tlatency_us"]
    for name, index, char, dt_us in results:
        lines.append(f"{name}\t{index}\t{char!r}\t{dt_us:.0f}")

    def stats(name):
        vals = sorted(v for n, _, _, v in results if n == name)
        if not vals:
            return "n/a"
        mid = vals[len(vals) // 2]
        return f"median {mid:.0f} us, min {vals[0]:.0f}, max {vals[-1]:.0f}, n={len(vals)}"

    lines.append("")
    lines.append(f"# accel={args.accel}")
    for name in ["demo", "red", "comment"]:
        lines.append(f"# {name}: {stats(name)}")
    output = "\n".join(lines)
    print(output)
    if args.out:
        with open(args.out, "w") as handle:
            handle.write(output + "\n")


if __name__ == "__main__":
    main()
