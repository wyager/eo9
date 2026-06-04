#!/usr/bin/env python3
"""Drive a QEMU aarch64 eosh session to verify arrow-key history recall.

Foreground, single process; prints a transcript and PASS/FAIL summary lines.
"""
import os, pty, select, subprocess, sys, time, tty

ROOT = os.path.dirname(os.path.abspath(__file__))
TIMEOUT = 420  # cold store builds are slow; the kernel build is warm by now

def spawn():
    master, slave = pty.openpty()
    tty.setraw(slave)  # raw: deliver bytes (incl. ESC sequences) immediately, no echo
    proc = subprocess.Popen(
        ["cargo", "xtask", "qemu", "aarch64"],
        cwd=ROOT, stdin=slave, stdout=slave, stderr=subprocess.STDOUT,
        close_fds=True,
    )
    os.close(slave)
    return proc, master

buf = b""

def wait_for(master, needle, timeout=TIMEOUT):
    global buf
    deadline = time.time() + timeout
    while time.time() < deadline:
        r, _, _ = select.select([master], [], [], 1.0)
        if r:
            try:
                chunk = os.read(master, 65536)
            except OSError:
                break
            if not chunk:
                break
            buf += chunk
        if needle.encode() in buf:
            return True
    return False

def consume():
    global buf
    out, buf = buf, b""
    return out

def send(master, data):
    os.write(master, data if isinstance(data, bytes) else data.encode())

def main():
    proc, master = spawn()
    results = []
    def check(name, ok):
        results.append((name, ok))
        print(f"{'PASS' if ok else 'FAIL'}: {name}", flush=True)
    try:
        ok = wait_for(master, "eosh> ")
        check("boot to prompt", ok)
        if not ok:
            raise SystemExit(1)
        consume()

        # 1. Run two distinct commands to seed history.
        send(master, "echo first run\r")
        check("first command ran", wait_for(master, "first run") and wait_for(master, "eosh> "))
        consume()
        send(master, "echo second thing\r")
        check("second command ran", wait_for(master, "second thing") and wait_for(master, "eosh> "))
        consume()

        # 2. Up recalls the newest line; Enter reruns it.
        send(master, "\x1b[A")
        check("up-arrow echoed the recalled line", wait_for(master, "echo second thing", timeout=15))
        consume()
        send(master, "\r")
        check("recalled line reran", wait_for(master, "second thing") and wait_for(master, "eosh> "))
        consume()

        # 3. Up twice reaches the older entry; appending edits it; Enter runs the edit.
        send(master, "\x1b[A\x1b[A")
        check("up-up echoed the older entry", wait_for(master, "echo first run", timeout=15))
        consume()
        send(master, " edited\r")
        check("edited recall ran joined", wait_for(master, "first run edited") and wait_for(master, "eosh> "))
        consume()

        # 4. Down past the newest restores the stashed fresh line.
        send(master, "echo stash")          # type without Enter
        time.sleep(0.3); consume()
        send(master, "\x1b[A")               # browse away
        time.sleep(0.3); consume()
        send(master, "\x1b[B")               # and back
        check("down restored the stash", wait_for(master, "echo stash", timeout=15))
        consume()
        send(master, " restored\r")
        check("restored line ran", wait_for(master, "stash restored") and wait_for(master, "eosh> "))
        consume()

        # 5. Left/Right/Home/End/Delete are consumed silently (no [C garbage).
        send(master, "\x1b[C\x1b[D\x1b[H\x1b[F\x1b[3~")
        time.sleep(0.3)
        send(master, "echo clean line\r")
        ok = wait_for(master, "clean line") and wait_for(master, "eosh> ")
        text = consume().decode(errors="replace")
        check("other CSI finals consumed silently", ok and "[C" not in text and "[D" not in text and "[3~" not in text)

        # 6. Unpaced paste burst still lands losslessly (the regression guard).
        ok_all = True
        for i in range(10):
            send(master, f"time.frozen --now-seconds 0 --monotonic-ns 0 $ hello --name burst{i} --excited true\r")
            if not (wait_for(master, f"burst{i}") and wait_for(master, "eosh> ")):
                ok_all = False
                break
            consume()
        check("10/10 unpaced 53+ char bursts", ok_all)

        # 7. Ctrl-C at the prompt stays harmless; the console lives.
        send(master, "\x03")
        time.sleep(0.3); consume()
        send(master, "echo after ctrl-c\r")
        check("console alive after Ctrl-C", wait_for(master, "after ctrl-c") and wait_for(master, "eosh> "))
        consume()

        send(master, "poweroff\r")
        wait_for(master, "", timeout=30)
    finally:
        time.sleep(2)
        if proc.poll() is None:
            proc.kill()
        failed = [name for name, ok in results if not ok]
        print(f"RESULT: {len(results) - len(failed)}/{len(results)} passed"
              + (f"; FAILED: {failed}" if failed else ""), flush=True)
        sys.exit(1 if failed else 0)

if __name__ == "__main__":
    main()
