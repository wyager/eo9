import subprocess, sys, time, os, select
def boot(cmds, expect):
    proc = subprocess.Popen(["cargo","xtask","qemu","aarch64","storedisk"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, bufsize=0)
    buf=b""; seen=[]
    def wait(pats, timeout):
        nonlocal buf
        deadline=time.time()+timeout
        while time.time()<deadline:
            for i,p in enumerate(pats):
                if p in buf: buf=buf[buf.find(p)+len(p):]; return i
            r,_,_=select.select([proc.stdout],[],[],0.5)
            if r:
                c=os.read(proc.stdout.fileno(),65536)
                if not c: return -2
                buf+=c; seen.append(c)
        return -1
    assert wait([b"eosh> "],900)>=0, "no prompt"
    for cmd in cmds:
        proc.stdin.write(cmd.encode()+b"\n"); proc.stdin.flush()
        wait([b"eosh> "],900)
    proc.stdin.write(b"poweroff\n"); proc.stdin.flush()
    wait([b"SYSTEM_OFF"],120)
    try: proc.wait(timeout=20)
    except: proc.kill()
    out=b"".join(seen).decode(errors="replace")
    for e in expect:
        print(("FOUND " if e in out else "MISSING ")+e)
boot(["time.frozen --now-seconds 5 --monotonic-ns 0 $ hello"], ["codegen: compiling", "eofs mounted", "[5.000000000]"])
boot(["time.frozen --now-seconds 5 --monotonic-ns 0 $ hello"], ["compile cache hit", "[5.000000000]"])
