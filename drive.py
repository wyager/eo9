#!/usr/bin/env python3
"""Scripted serial driver for the backstop-detector battery.

Usage: drive.py <logfile> <xtask-arg>... -- <step>...
Each step is either  wait:<marker>  or  send:<line>  (sent as one unpaced write,
the paste-fix contract). The session always ends by waiting for qemu exit.
"""
import subprocess
import sys
import time


def main() -> int:
    argv = sys.argv[1:]
    log_path = argv[0]
    split = argv.index("--")
    xtask_args = argv[1:split]
    steps = argv[split + 1 :]

    log = open(log_path, "wb")
    child = subprocess.Popen(
        ["cargo", "xtask", "qemu", *xtask_args],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        cwd="/Users/wy/code/eo9/.claude/worktrees/backstop",
    )
    assert child.stdin is not None and child.stdout is not None
    import fcntl
    import os

    fd = child.stdout.fileno()
    fcntl.fcntl(fd, fcntl.F_SETFL, fcntl.fcntl(fd, fcntl.F_GETFL) | os.O_NONBLOCK)

    seen = b""

    def pump() -> None:
        nonlocal seen
        try:
            chunk = child.stdout.read()
        except (BlockingIOError, TypeError):
            chunk = None
        if chunk:
            seen += chunk
            log.write(chunk)
            log.flush()

    def wait_for(marker: bytes, what: str, timeout: float = 600.0) -> None:
        deadline = time.time() + timeout
        start = len(seen)
        while time.time() < deadline:
            pump()
            if marker in seen[max(0, start - 4096) :]:
                return
            if child.poll() is not None:
                pump()
                break
            time.sleep(0.05)
        else:
            print(f"TIMEOUT waiting for {what!r}", flush=True)
            child.kill()
            sys.exit(2)
        if marker not in seen[max(0, start - 4096) :]:
            print(f"EXITED before {what!r}", flush=True)
            sys.exit(3)

    for step in steps:
        kind, _, value = step.partition(":")
        if kind == "wait":
            wait_for(value.encode(), value)
        elif kind == "send":
            child.stdin.write(value.encode() + b"\n")
            child.stdin.flush()
        elif kind == "sleep":
            deadline = time.time() + float(value)
            while time.time() < deadline:
                pump()
                time.sleep(0.05)
        else:
            raise SystemExit(f"unknown step {step!r}")

    deadline = time.time() + 180
    while child.poll() is None and time.time() < deadline:
        pump()
        time.sleep(0.1)
    pump()
    if child.poll() is None:
        child.kill()
        print("KILLED: qemu did not exit", flush=True)
        return 4
    print(f"qemu exited {child.returncode}", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
