#!/usr/bin/env python3
"""Drive a QEMU eosh session, run commands repeatedly, timestamp echo->ok and capture spawn-trace lines."""
import subprocess, sys, time, os, select, re

CMDS = sys.argv[1:] or ["gpu.virtio $ draw"]
REPEAT = int(os.environ.get("REPEAT", "11"))
XARGS = os.environ.get("XARGS", "pci gpu").split()

proc = subprocess.Popen(
    ["cargo", "xtask", "qemu", "aarch64"] + XARGS,
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, bufsize=0)

buf = b""
log = open("bench_spawn.log", "ab")
def read_until(patterns, timeout):
    """Read until any pattern appears (returns index) or timeout."""
    global buf
    deadline = time.time() + timeout
    while time.time() < deadline:
        for i, pat in enumerate(patterns):
            if pat in buf:
                idx = buf.find(pat) + len(pat)
                buf = buf[idx:]
                return i
        r, _, _ = select.select([proc.stdout], [], [], 0.5)
        if r:
            chunk = os.read(proc.stdout.fileno(), 65536)
            if not chunk:
                return -2
            log.write(chunk); log.flush()
            buf += chunk
            sys.stdout.write(chunk.decode(errors="replace")); sys.stdout.flush()
    return -1

assert read_until([b"eosh> "], 900) >= 0, "no prompt"
results = {}
for cmd in CMDS:
    times = []
    for i in range(REPEAT):
        proc.stdin.write(cmd.encode() + b"\n"); proc.stdin.flush()
        # wait for the echo of the command (kernel echoes typed chars)
        assert read_until([cmd.encode()], 60) >= 0, "no echo"
        t0 = time.time()
        r = read_until([b"ok: ", b"error"], 900)
        assert r >= 0, "no outcome"
        t1 = time.time()
        assert read_until([b"eosh> "], 60) >= 0, "no prompt back"
        times.append((t1 - t0) * 1000)
        print(f"\n### {cmd!r} run {i}: {times[-1]:.0f} ms", flush=True)
    results[cmd] = times

proc.stdin.write(b"poweroff\n"); proc.stdin.flush()
read_until([b"SYSTEM_OFF", b"system off", b"halt"], 120)
try: proc.wait(timeout=30)
except subprocess.TimeoutExpired: proc.kill()
print("\n=== RESULTS ===")
for cmd, times in results.items():
    warm = sorted(times[1:])
    print(f"{cmd!r}: cold={times[0]:.0f}ms warm_median={warm[len(warm)//2]:.0f}ms warm_all={[f'{t:.0f}' for t in times[1:]]}")
