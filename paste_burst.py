import subprocess, sys, time, os, select
proc = subprocess.Popen(["cargo","xtask","qemu","aarch64"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, bufsize=0)
buf=b""
def wait(pats, timeout):
    global buf
    deadline=time.time()+timeout
    while time.time()<deadline:
        for i,p in enumerate(pats):
            if p in buf: buf=buf[buf.find(p)+len(p):]; return i
        r,_,_=select.select([proc.stdout],[],[],0.5)
        if r:
            c=os.read(proc.stdout.fileno(),65536)
            if not c: return -2
            buf+=c
    return -1
assert wait([b"eosh> "],900)>=0
ok=0
line=b"time.frozen --now-seconds 0 --monotonic-ns 0 $ hello\n"
for i in range(30):
    proc.stdin.write(line); proc.stdin.flush()
    if wait([b"[0.000000000]"],120)>=0 and wait([b"eosh> "],60)>=0: ok+=1
proc.stdin.write(b"poweroff\n"); proc.stdin.flush(); wait([b"SYSTEM_OFF"],60)
try: proc.wait(timeout=20)
except: proc.kill()
print(f"PASTE: {ok}/30")
