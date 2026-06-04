#!/usr/bin/env python3
import subprocess, sys, time, os, select
arch = sys.argv[1]; xargs = sys.argv[2].split() if len(sys.argv)>2 and sys.argv[2] else []
cmds = sys.argv[3:]
proc = subprocess.Popen(["cargo","xtask","qemu",arch]+xargs, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, bufsize=0)
buf=b""
def wait(pats, timeout):
    global buf
    deadline=time.time()+timeout
    while time.time()<deadline:
        for i,p in enumerate(pats):
            if p in buf:
                buf=buf[buf.find(p)+len(p):]; return i
        r,_,_=select.select([proc.stdout],[],[],0.5)
        if r:
            c=os.read(proc.stdout.fileno(),65536)
            if not c: return -2
            buf+=c; sys.stdout.write(c.decode(errors="replace")); sys.stdout.flush()
    return -1
assert wait([b"eosh> "],900)>=0, "no prompt"
ok=True
for cmd in cmds:
    proc.stdin.write(cmd.encode()+b"\n"); proc.stdin.flush()
    r=wait([b"ok: ",b"error"],900)
    if r!=0: ok=False; print(f"\n!!! {cmd!r} -> {'error' if r==1 else 'timeout'}")
    wait([b"eosh> "],120)
proc.stdin.write(b"poweroff\n"); proc.stdin.flush()
wait([b"SYSTEM_OFF",b"system off"],120)
try: proc.wait(timeout=30)
except: proc.kill()
print("\nBATTERY:", "PASS" if ok else "FAIL")
