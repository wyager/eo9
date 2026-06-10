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
  python3 tests/step-cost/echolat.py [--accel hvf|tcg] [--config plain|station] [--out FILE]

The default accelerator is hvf (native execution on Apple Silicon — same ISA family
as the board's Cortex-A76, our cranelift codegen, in-wasm dlmalloc). Results print
as TSV: pass, position, byte, latency_us.

--config station boots the same image the board's station boot runs: the `station
pci console-sink` boot token (init supervising the kbd service chain
`usb.ohci-pci $ usb.kbd restart restart.always` with eosh as the supervised
console child) and the check-station USB topology (OHCI + hub + keyboard). The
typing still happens on the serial console — what changes is the *executor
position* of eosh: a fuel-sliced child on init's drive loop instead of the root
program with an unsliced pool (area/34-fuel-yield-latency H1).
"""

import argparse
import os
import select
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


def drain_for(proc, duration_s, sink):
    """Read whatever arrives within `duration_s`; returns the byte count.

    Used after each keystroke's echo to count the FULL output the keystroke
    produced (repaint escape sequences, recolors). The byte count is a guest
    property — identical on QEMU and on the board for the same eosh build —
    so it converts directly into board output-path cost (TX + fbcon tee) per key.
    """
    count = 0
    deadline = time.monotonic() + duration_s
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return count
        ready, _, _ = select.select([proc.stdout], [], [], remaining)
        if not ready:
            return count
        byte = proc.stdout.read(1)
        if not byte:
            return count
        sink += byte
        count += 1


def latency_pass(proc, name, payload, results, sink, settle_s=0.05):
    """Send `payload` one byte at a time; record write→echo latency per byte.

    The guest echoes printable bytes verbatim (possibly preceded by marker escape
    sequences, e.g. SGR 31 before the first dead byte); waiting for the byte itself
    skips any prefix. A settle pause between keys keeps keystrokes from pipelining
    (we want per-key cost, not throughput); the settle doubles as the window for
    counting the keystroke's total output bytes (echo + repaint), recorded per key.
    """
    for index, byte in enumerate(payload):
        want = bytes([byte])
        t0 = time.monotonic()
        proc.stdin.write(want)
        proc.stdin.flush()
        prefix = read_until(proc, want, STEP_TIMEOUT, sink)
        dt_us = (time.monotonic() - t0) * 1e6
        tail = drain_for(proc, settle_s, sink)
        results.append((name, index, chr(byte), dt_us, len(prefix) + tail))


class Qmp:
    """Minimal QMP client for injecting keys on the emulated USB keyboard."""

    def __init__(self, path):
        import json
        import socket

        self.json = json
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(path)
        self.sock.settimeout(30)
        self.buf = b""
        self._read_until(b'"QMP"')
        self._execute('{"execute":"qmp_capabilities"}')

    def _read_until(self, needle):
        while needle not in self.buf:
            self.buf += self.sock.recv(4096)

    def _execute(self, line):
        self.buf = b""
        self.sock.sendall(line.encode() + b"\n")
        self._read_until(b'"return"')

    def key(self, qcode):
        for down in ("true", "false"):
            self._execute(
                '{"execute":"input-send-event","arguments":{"events":[{"type":"key",'
                '"data":{"down":%s,"key":{"type":"qcode","data":"%s"}}}]}}' % (down, qcode)
            )


def usbkbd_pass(proc, qmp, payload, results, sink, settle_s=0.05):
    """Inject `payload` on the emulated USB keyboard one key at a time; record
    inject→serial-echo latency per key. This times the whole service path: OHCI
    interrupt → usb.kbd HID decode → console-sink inject → eosh echo."""
    for index, char in enumerate(payload):
        want = char.encode()
        t0 = time.monotonic()
        qmp.key(char)
        prefix = read_until(proc, want, STEP_TIMEOUT, sink)
        dt_us = (time.monotonic() - t0) * 1e6
        tail = drain_for(proc, settle_s, sink)
        results.append(("usbkbd", index, char, dt_us, len(prefix) + tail))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--accel", choices=["hvf", "tcg"], default="hvf")
    parser.add_argument("--config", choices=["plain", "station"], default="plain")
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
    qmp_path = None
    if args.config == "station":
        # The board's station boot: init + the kbd service chain + eosh as the
        # supervised console child (check-station's exact topology and token).
        qmp_path = os.path.join(REPO, "kernel", "target", "eo9-echolat-qmp.sock")
        if os.path.exists(qmp_path):
            os.unlink(qmp_path)
        cmd += [
            "-append", "station pci console-sink",
            "-device", "pci-ohci,id=eo9ohci",
            "-device", "usb-hub,bus=eo9ohci.0,port=1",
            "-device", "usb-kbd,bus=eo9ohci.0,port=1.1",
            "-qmp", f"unix:{qmp_path},server=on,wait=off",
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
        if args.config == "station":
            # Let the kbd service reach steady state (hub traversal + boot-protocol
            # configuration) before measuring, so enumeration churn does not pollute
            # the first pass. Same poll check-station uses; `svc log` is inspection.
            for _ in range(30):
                proc.stdin.write(b"svc log kbd\r")
                proc.stdin.flush()
                out = read_until(proc, PROMPT, STEP_TIMEOUT, transcript)
                if b"forwarding boot-protocol keystrokes" in out:
                    break
                time.sleep(2)
            else:
                print("# warning: kbd service never reported forwarding", file=sys.stderr)
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

        # Pass 4 (station only): keys injected on the emulated USB keyboard — the
        # full service path (OHCI interrupt → usb.kbd decode → console-sink inject →
        # eosh echo). This is the path with no UART interrupt behind it: the latency
        # here is what the executor's input-edge delivery is worth.
        if args.config == "station":
            qmp = Qmp(qmp_path)
            usbkbd_pass(proc, qmp, "abcdefghij" * 2, results, transcript)
            proc.stdin.write(CTRL_C)
            proc.stdin.flush()
            read_until(proc, PROMPT, STEP_TIMEOUT, transcript)

        proc.stdin.write(b"poweroff\r")
        proc.stdin.flush()
        # Capture the shutdown tail (drive-stats dumps, liveness findings, the init
        # outcome) before the process exits.
        drain_for(proc, 10.0, transcript)
        try:
            proc.wait(timeout=30)
        except subprocess.TimeoutExpired:
            proc.kill()
    finally:
        if proc.poll() is None:
            proc.kill()
        proc.wait()

    lines = ["pass\tpos\tbyte\tlatency_us\techo_bytes"]
    for name, index, char, dt_us, nbytes in results:
        lines.append(f"{name}\t{index}\t{char!r}\t{dt_us:.0f}\t{nbytes}")

    def stats(name):
        vals = sorted(v for n, _, _, v, _ in results if n == name)
        if not vals:
            return "n/a"
        mid = vals[len(vals) // 2]
        sizes = sorted(b for n, _, _, _, b in results if n == name)
        bmid = sizes[len(sizes) // 2]
        return (
            f"median {mid:.0f} us, min {vals[0]:.0f}, max {vals[-1]:.0f}, n={len(vals)}; "
            f"echo bytes median {bmid}, min {sizes[0]}, max {sizes[-1]}"
        )

    lines.append("")
    lines.append(f"# accel={args.accel} config={args.config}")
    for name in ["demo", "red", "comment", "usbkbd"]:
        lines.append(f"# {name}: {stats(name)}")
    # Surface every kernel liveness finding and drive-stats dump the boot produced —
    # the detectors' verdict belongs next to the latency numbers.
    for raw in bytes(transcript).splitlines():
        if b"liveness:" in raw or b"drive-stats[" in raw:
            lines.append("# " + raw.decode(errors="replace").strip())
    output = "\n".join(lines)
    print(output)
    if args.out:
        with open(args.out, "w") as handle:
            handle.write(output + "\n")


if __name__ == "__main__":
    main()
