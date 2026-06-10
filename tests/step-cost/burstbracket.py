#!/usr/bin/env python3
"""Bracketed drive-stats burst probe (area/38-first-poll-parks).

Replicates the silicon bench protocol on QEMU: drive-stats dumps are triggered by
Ctrl-C-ing a nested eosh (the consumed Ctrl-C fires the `drive-stats[ctrl-c]` dump in
the task-wait path), bracketing a 20-tracked-key burst typed at the console prompt and
an empty control window:

    dump A  →  20 tracked keys (one name-position word)  →  dump B
            →  1.5 s empty (nested eosh sitting at its prompt)  →  dump C

Burst-only net = (B−A) − (C−B): the nested-eosh bracket overhead (its spawn, its
task.wait spin while alive, its teardown) is the same in both windows and cancels.

Requires a kernel image built with EO9_KERNEL_FEATURES_EXTRA=drive-stats.

Usage:
  python3 tests/step-cost/burstbracket.py [--accel tcg|hvf] [--config station|plain]
"""

import argparse
import os
import re
import select
import subprocess
import sys
import time

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
IMAGE = os.path.join(
    REPO, "kernel", "target", "aarch64-unknown-none", "release", "eo9-kernel"
)

PROMPT = b"eosh> "
CTRL_C = b"\x03"
STEP_TIMEOUT = 180.0
# 20 tracked keys: one long name-position word (every char is tracked — the M3
# name-mark oracle runs on each; same class the silicon burst used).
BURST = b"netrtlnetrtlnetrtlnr"

FIELDS = [
    "passes", "hot", "child-polls", "child-rung", "svc-polls", "svc-rung",
    "wake-event", "wake-deadline", "wake-backstop", "gate-catch", "edge-bounce",
]


def read_until(proc, marker, deadline_s, sink):
    seen = bytearray()
    deadline = time.monotonic() + deadline_s
    while marker not in seen:
        if time.monotonic() > deadline:
            raise TimeoutError(
                f"timed out waiting for {marker!r}; last: {bytes(seen[-400:])!r}"
            )
        byte = proc.stdout.read(1)
        if not byte:
            raise EOFError(f"serial closed; last: {bytes(seen[-400:])!r}")
        seen += byte
        sink += byte
    return bytes(seen)


def drain_for(proc, duration_s, sink):
    deadline = time.monotonic() + duration_s
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return
        ready, _, _ = select.select([proc.stdout], [], [], remaining)
        if not ready:
            return
        byte = proc.stdout.read(1)
        if not byte:
            return
        sink += byte


def send(proc, data):
    proc.stdin.write(data)
    proc.stdin.flush()


def parse_dump(text, label):
    stats = {}
    for field in FIELDS:
        m = re.search(rf"(?<![\w-]){re.escape(field)}=(\d+)", text)
        if not m:
            raise RuntimeError(f"dump {label}: field {field} missing in: {text!r}")
        stats[field] = int(m.group(1))
    m = re.search(r"hostcalls:([^\n]*)", text)
    if m:
        for name, val in re.findall(r"([\w-]+)=(\d+)", m.group(1)):
            stats["hc." + name] = int(val)
    m = re.search(r"key-echo: count=(\d+) total-us=(\d+) max-us=(\d+)", text)
    if m:
        stats["keyecho.count"] = int(m.group(1))
        stats["keyecho.total_us"] = int(m.group(2))
        stats["keyecho.max_us"] = int(m.group(3))
    m = re.search(r"parks:([^\n]*)", text)
    if m:
        for name, val in re.findall(r"(\w+)=(\d+)", m.group(1)):
            stats["park." + name] = int(val)
    return stats


def dump_bracket(proc, sink, label):
    """Spawn a nested eosh, Ctrl-C it: the consumed Ctrl-C fires the drive-stats dump.
    Returns the parsed cumulative counters."""
    send(proc, b"eosh\r")
    # The nested session prints its own prompt.
    read_until(proc, PROMPT, STEP_TIMEOUT, sink)
    time.sleep(0.3)
    mark = len(sink)
    send(proc, CTRL_C)
    read_until(proc, b"drive-stats[ctrl-c] parks:", STEP_TIMEOUT, sink)
    read_until(proc, b"\n", STEP_TIMEOUT, sink)
    stats = parse_dump(bytes(sink[mark:]).decode(errors="replace"), label)
    # Back at the outer console prompt.
    read_until(proc, PROMPT, STEP_TIMEOUT, sink)
    return stats


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--accel", choices=["hvf", "tcg"], default="tcg")
    parser.add_argument("--config", choices=["plain", "station"], default="station")
    args = parser.parse_args()

    if not os.path.exists(IMAGE):
        sys.exit(f"kernel image missing: {IMAGE}")

    cmd = ["qemu-system-aarch64"]
    if args.accel == "hvf":
        cmd += ["-accel", "hvf", "-cpu", "host"]
    else:
        cmd += ["-cpu", "max"]
    cmd += [
        "-M", "virt,gic-version=2,highmem=off",
        "-device", "virtio-rng-pci",
        "-smp", "1", "-m", "512M", "-nographic",
        "-kernel", IMAGE,
    ]
    if args.config == "station":
        cmd += [
            "-append", "station pci console-sink",
            "-device", "pci-ohci,id=eo9ohci",
            "-device", "usb-hub,bus=eo9ohci.0,port=1",
            "-device", "usb-kbd,bus=eo9ohci.0,port=1.1",
        ]
    print(f"# {' '.join(cmd)}", file=sys.stderr)
    proc = subprocess.Popen(
        cmd, cwd=REPO, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL, bufsize=0,
    )
    sink = bytearray()
    try:
        read_until(proc, PROMPT, STEP_TIMEOUT, sink)
        if args.config == "station":
            # Wait for the kbd service to reach steady state.
            for _ in range(30):
                send(proc, b"svc log kbd\r")
                out = read_until(proc, PROMPT, STEP_TIMEOUT, sink)
                if b"forwarding boot-protocol keystrokes" in out:
                    break
                time.sleep(2)
        time.sleep(1.0)

        a = dump_bracket(proc, sink, "A")

        # The burst: 20 tracked keys at the console prompt, back-to-back (the next key
        # goes out as soon as the previous echo lands — no settle, so idle pacing
        # cannot pollute the per-key counts), then Ctrl-C clears the line (at the bare
        # prompt this is editor-side only; the dump needs the consumed-by-waiter path).
        t0 = time.monotonic()
        latencies = []
        for byte in BURST:
            k0 = time.monotonic()
            send(proc, bytes([byte]))
            read_until(proc, bytes([byte]), STEP_TIMEOUT, sink)
            latencies.append((time.monotonic() - k0) * 1e3)
        wall_burst = time.monotonic() - t0
        send(proc, CTRL_C)  # clear the line
        time.sleep(0.3)

        b = dump_bracket(proc, sink, "B")

        # Empty control: nested eosh sits at its prompt for 1.5 s inside the bracket.
        send(proc, b"eosh\r")
        read_until(proc, PROMPT, STEP_TIMEOUT, sink)
        time.sleep(1.5)
        mark = len(sink)
        send(proc, CTRL_C)
        read_until(proc, b"drive-stats[ctrl-c] parks:", STEP_TIMEOUT, sink)
        read_until(proc, b"\n", STEP_TIMEOUT, sink)
        c = parse_dump(bytes(sink[mark:]).decode(errors="replace"), "C")
        read_until(proc, PROMPT, STEP_TIMEOUT, sink)

        send(proc, b"poweroff\r")
        drain_for(proc, 8.0, sink)
        try:
            proc.wait(timeout=30)
        except subprocess.TimeoutExpired:
            proc.kill()
    finally:
        if proc.poll() is None:
            proc.kill()
        proc.wait()

    keys = list(dict.fromkeys(list(a) + list(b) + list(c)))
    ba = {f: b.get(f, 0) - a.get(f, 0) for f in keys}
    cb = {f: c.get(f, 0) - b.get(f, 0) for f in keys}
    net = {f: ba[f] - cb[f] for f in keys}
    nkeys = len(BURST)
    lat = sorted(latencies)
    print(f"accel={args.accel} config={args.config} burst_wall={wall_burst:.2f}s "
          f"({nkeys} tracked keys back-to-back) "
          f"latency ms median={lat[nkeys//2]:.1f} min={lat[0]:.1f} max={lat[-1]:.1f}")
    print(f"{'field':<22}{'B-A':>10}{'C-B':>10}{'net':>10}{'net/key':>10}")
    for f in keys:
        print(f"{f:<22}{ba[f]:>10}{cb[f]:>10}{net[f]:>10}{net[f]/nkeys:>10.1f}")
    # Liveness lines seen anywhere this boot:
    for raw in bytes(sink).splitlines():
        if b"liveness:" in raw:
            print("# " + raw.decode(errors="replace").strip())


if __name__ == "__main__":
    main()
