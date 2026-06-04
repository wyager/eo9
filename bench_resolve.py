#!/usr/bin/env python3
"""Time echo->ok for repeated lines at the eosh prompt (the resolve-cache benchmark)."""
import subprocess, sys, os, time, select, re

KERNEL = sys.argv[1]
GPU = len(sys.argv) > 2 and sys.argv[2] == "gpu"
cmd = ["qemu-system-aarch64", "-M", "virt,gic-version=2,highmem=off", "-cpu", "max",
       "-device", "virtio-rng-pci", "-smp", "1", "-m", "512M", "-nographic",
       "-kernel", KERNEL]
if GPU:
    cmd += ["-append", "pci", "-device", "virtio-gpu-pci"]

proc = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                        stderr=subprocess.STDOUT, bufsize=0)
buf = b""

def read_until(pattern, timeout):
    global buf
    deadline = time.time() + timeout
    pat = re.compile(pattern)
    while time.time() < deadline:
        m = pat.search(buf.decode("utf-8", "replace"))
        if m:
            return m
        r, _, _ = select.select([proc.stdout], [], [], 0.2)
        if r:
            chunk = os.read(proc.stdout.fileno(), 65536)
            if not chunk:
                break
            buf += chunk
    return None

def run_line(line, timeout=600):
    """Send a line, return seconds from send to the outcome line."""
    global buf
    buf = b""
    proc.stdin.write((line + "\n").encode())
    proc.stdin.flush()
    t0 = time.time()
    m = read_until(r"(ok: |error: |failed: )", timeout)
    if not m:
        print(f"TIMEOUT waiting for outcome of: {line}\n--- tail ---\n{buf.decode('utf-8','replace')[-2000:]}")
        proc.kill(); sys.exit(1)
    elapsed = time.time() - t0
    # drain to the next prompt
    read_until(r"eosh> $", 30)
    return elapsed

if not read_until(r"eosh> $", 900):
    print("no prompt\n" + buf.decode("utf-8", "replace")[-2000:]); proc.kill(); sys.exit(1)

results = {}
lines = []
if GPU:
    lines.append(("gpu draw", "gpu.virtio $ draw", 4))
lines.append(("frozen hello", "time.frozen --now-seconds 0 --monotonic-ns 0 $ hello", 4))
lines.append(("bare hello", "hello", 4))

for label, line, reps in lines:
    times = []
    for i in range(reps):
        times.append(run_line(line))
    results[label] = times

# correctness probes, live: save -> the new name runs; the old name re-resolves
probe = []
probe.append(("save", run_line("save greet2 = hello")))
probe.append(("saved name runs", run_line("greet2")))
probe.append(("old name still runs", run_line("hello")))

proc.stdin.write(b"poweroff\n"); proc.stdin.flush()
try:
    proc.wait(timeout=60)
except subprocess.TimeoutExpired:
    proc.kill()

for label, times in results.items():
    rendered = " ".join(f"{t*1000:.0f}ms" for t in times)
    print(f"{label}: cold {times[0]*1000:.0f}ms | warm {rendered}")
for label, t in probe:
    print(f"probe {label}: {t*1000:.0f}ms ok")
