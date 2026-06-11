#!/usr/bin/env python3
"""Send a kernel image to the serial-loader stub (UART), to oskexec (TCP kexec), or
to stickflash (TCP boot-stick rewrite).

    python3 send_image.py kernel/target/eo9-opi5plus-min.img \
        --load-addr 0x00200000 --x0 0xeb9f6c38

    python3 send_image.py kernel/target/eo9-opi5plus-min.img \
        --tcp 10.20.3.70:9909 --secret "$EO9_KEXEC_SECRET"

    python3 send_image.py kernel/target/eo9-opi5plus-min.img \
        --stick 10.20.3.70:9910 --secret "$EO9_KEXEC_SECRET"

--stick speaks the SAME frames as --tcp (eo9-flashwire pins them) to the stickflash
service: the image is zero-padded host-side to the stick's fixed EO9.IMG slot
(--slot-mib, default 56 — must match `cargo xtask build-stick`), the CRC covers the
padded slot, and 'K' arrives only after the board wrote the slot AND read it back
verified — so the verdict wait is minutes at full-speed USB, not seconds. On 'K' the
stick is done; reset the board to boot it (stickflash never auto-resets).

Serial protocol (must match src/lib.rs, pinned by --selftest):
  "EO9L" + <Q load_addr> + <Q length> + <Q x0_value> + payload + <I crc32(payload)>
The stub answers 'k' per 64 KiB (progress), then 'K' (verified, jumping) or 'E'
(CRC mismatch — it re-arms, just re-run) or 'T' (3 s stall — re-run).

TCP protocol (must match guest/examples/oskexec — the serial framing plus an
authentication frame and a commit go-ahead):
  "EO9L" + <H len(secret)> + secret        -> 'A' (authenticated) or 'E' (refused)
  <Q load_addr> + <Q length> + <Q x0>      (load_addr/x0 carried for parity, ignored)
  payload                                  <- 'k' per 64 KiB
  <I crc32(payload)>                       -> 'K' (verified) or 'E' (mismatch)
  "G"                                      go-ahead: only after our 'K' arrived does
                                           oskexec commit (so the verdict can't be
                                           lost when the machine jumps)
The secret comes from --secret or the EO9_KEXEC_SECRET environment variable (the env
form keeps it out of bench command lines); >= 16 bytes, REQUIRED for --tcp. It travels
CLEARTEXT — trusted-LAN/bench tool only (see guest/examples/oskexec/wit/world.wit).
After 'G' the connection simply dies with the old kernel; the new kernel's banner
appears on the board's serial console (the planner's console, not this script).

Why streaming is safe AND fast: the stub services one byte in well under 1 µs (a
couple of MMIO polls) against the line's 6.7 µs/byte at 1.5 Mbaud, so the UART's
32-byte RX FIFO can never fill — the board never needs byte-level back-pressure. The
per-64-KiB 'k' acks are the flow control: the host streams at line rate, polling acks
without blocking, and only stops to wait when it gets more than WINDOW bytes ahead of
the stub's last ack. That keeps the wire the pacer (~87 s for the 12.4 MiB minimal
image) while still catching a dead/derailed stub within seconds instead of at the end.
(macOS buffers serial writes deeply, so local write counts mean little; the acks are
the progress truth.)

x0_value: pass the control FDT address from `bdinfo` on the board (the `fdt_blob`
line — the `fdtcontroladdr` environment variable is UNSET on this vendor U-Boot, so
`printenv fdtcontroladdr` yields nothing).
With the booti launch you may pass --x0 0 — the stub then forwards its own entry x0,
which booti already set to the device tree.

After 'K' the stub jumps; this script switches to dumb console mode and tails the
board's output. --tail-seconds N (default 20) detaches after N seconds of console
SILENCE — a stall alarm for unattended runs that also frees the port for the planner's
console; 0 tails forever (Ctrl-C to detach).
"""
import argparse
import binascii
import struct
import sys
import time

# Line-buffer stdout even when piped: a killed process must never take the board's
# console output with it (the 2026-06-04 first-jump output was lost exactly this way).
sys.stdout.reconfigure(line_buffering=True)

MAGIC = b"EO9L"
ACK_INTERVAL = 64 * 1024
PORT = "/dev/cu.usbserial-AC009X7K"
BAUD = 1_500_000
CHUNK = 4096
# Flow-control window: how far (in payload bytes) the host may run ahead of the stub's
# last 'k' ack before it blocks waiting for acks. 512 KiB ≈ 3.5 s of line time: deep
# enough that ack latency never starves the wire, shallow enough that a stub that stops
# acking (or answers 'E'/'T') is noticed within a few seconds.
WINDOW = 8 * ACK_INTERVAL
# Mid-transfer alarm: abort if at least one ack is overdue and none arrives for this
# long (the stub's own stall timeout is ~3 s, so 10 s of nothing means it is gone).
# The alarm is checked on EVERY pass of EVERY loop — outer streaming loop included —
# against the time of the last ack progress, not per-loop-entry deadlines: the
# 2026-06-07 incident (a host sleep mid-transfer; the sender repainted 77% for 80+
# minutes) was a deadline that kept being re-armed each time control re-entered the
# window-block loop, so no single loop pass ever saw 10 quiet seconds.
ACK_STALL_SECONDS = 10.0
# A serial write that blocks longer than this means the OS driver wedged (post-sleep
# USB-serial is the known case) — surface it as the same stall alarm instead of
# hanging forever in write() where no alarm can run.
WRITE_TIMEOUT_SECONDS = 15.0


def selftest() -> None:
    assert binascii.crc32(b"123456789") & 0xFFFFFFFF == 0xCBF43926
    hdr = MAGIC + struct.pack("<QQQ", 1, 2, 3)
    assert len(hdr) == 28, len(hdr)
    print("selftest ok: crc vector + 28-byte header")


def scan_stub_bytes(data: bytes, seen: int) -> tuple[int, bytes]:
    """Count 'k' acks in `data`; return (count, terminal byte or b'')."""
    for b in data:
        ch = bytes([b])
        if ch == b"k":
            seen += 1
        elif ch in (b"K", b"E", b"T"):
            return seen, ch
        # anything else is line noise; ignore
    return seen, b""


def drain_acks(s, seen: int, block: bool = True) -> tuple[int, bytes]:
    """Consume pending stub bytes; count 'k's, return (count, terminal byte or b'').

    block=False reads only what the OS already buffered (in_waiting) and never sleeps
    in the port timeout — the streaming path uses this so the wire, not host polling,
    sets the transfer pace (a blocking 50 ms drain per 4 KiB chunk is what stretched
    the first 13 MB send to 182 s against the 87 s line rate)."""
    while True:
        pending = s.in_waiting
        if pending:
            seen, term = scan_stub_bytes(s.read(pending), seen)
        elif block:
            chunk = s.read(64)  # blocks up to the port timeout
            if not chunk:
                return seen, b""
            seen, term = scan_stub_bytes(chunk, seen)
        else:
            return seen, b""
        if term:
            return seen, term


def console(s, tail_seconds: float) -> None:
    if tail_seconds > 0:
        print(f"--- console (exits after {tail_seconds:g}s of silence; Ctrl-C to detach) ---")
    else:
        print("--- console (Ctrl-C to detach) ---")
    last = time.time()
    try:
        while tail_seconds <= 0 or time.time() - last < tail_seconds:
            data = s.read(4096)
            if data:
                sys.stdout.write(data.decode("utf-8", errors="replace"))
                sys.stdout.flush()
                last = time.time()
        print(
            f"\n--- console quiet for {tail_seconds:g}s; detaching "
            "(board state: whatever you last saw) ---"
        )
    except KeyboardInterrupt:
        print("\n--- detached ---")


def tcp_recv_scan(sock, seen: int, block: bool) -> tuple[int, bytes]:
    """Read whatever the socket has (one bounded recv); count 'k's, return any verdict."""
    import socket as socketlib

    try:
        sock.settimeout(0.05 if block else 0.0)
        data = sock.recv(4096)
    except (BlockingIOError, socketlib.timeout, TimeoutError):
        return seen, b""
    if not data:
        print("\nconnection closed by the receiver mid-transfer — re-run", file=sys.stderr)
        sys.exit(1)
    return scan_stub_bytes(data, seen)


def tcp_wait_byte(sock, wanted: bytes, deadline_seconds: float, what: str) -> bytes:
    """Block until one byte of `wanted` arrives (other bytes are protocol errors)."""
    import socket as socketlib

    deadline = time.time() + deadline_seconds
    while time.time() < deadline:
        try:
            sock.settimeout(0.2)
            data = sock.recv(1)
        except (socketlib.timeout, TimeoutError):
            continue
        if not data:
            print(f"\nconnection closed waiting for {what}", file=sys.stderr)
            sys.exit(1)
        if data in wanted:
            return data
        print(f"\nunexpected byte {data!r} waiting for {what}", file=sys.stderr)
        sys.exit(1)
    print(f"\ntimed out waiting for {what}", file=sys.stderr)
    sys.exit(1)


# TCP transport tuning: bigger writes (no UART pacing to respect), and a wider stall
# window than the serial path's 10 s — the guest staging loop under QEMU TCG can
# legitimately take seconds per 64 KiB ack late in a large stream, and this alarm only
# needs to catch a DEAD peer, not a slow one.
TCP_CHUNK = 64 * 1024
TCP_ACK_STALL_SECONDS = 60.0


def tcp_send(args, payload: bytes, crc: int, stick: bool = False) -> None:
    """The --tcp/--stick transport: authenticate, stream, verify, send the go-ahead.

    The frames are identical either way (eo9-flashwire pins them); the differences are
    the peer (oskexec :9909 vs stickflash :9910), the payload (--stick pre-pads to the
    EO9.IMG slot — see main), the verdict wait (stickflash writes AND read-back-verifies
    the whole slot between our CRC and its 'K', so the deadline scales with the slot at
    full-speed-USB rates), and what success means (a verified stick, not a jump)."""
    import socket

    flag, endpoint = ("--stick", args.stick) if stick else ("--tcp", args.tcp)
    peer = "stickflash" if stick else "oskexec"
    host, _, port_text = endpoint.rpartition(":")
    if not host or not port_text.isdigit():
        print(f"{flag} expects host:port (got {endpoint!r})", file=sys.stderr)
        sys.exit(2)
    secret = (args.secret or "").encode()
    if len(secret) < 16:
        print(
            f"{flag} needs a preshared secret of >= 16 bytes (--secret or the "
            "EO9_KEXEC_SECRET environment variable)",
            file=sys.stderr,
        )
        sys.exit(2)

    print(
        f"{args.image}: {len(payload)} bytes -> tcp {host}:{port_text} "
        f"({'stick flash' if stick else 'kexec'}), crc {crc:08x}"
    )
    sock = socket.create_connection((host, int(port_text)), timeout=30)
    try:
        sock.sendall(MAGIC + struct.pack("<H", len(secret)) + secret)
        verdict = tcp_wait_byte(sock, b"AE", 30, "the authentication verdict")
        if verdict == b"E":
            print(f"{peer} refused the secret — re-run (it allows ONE retry, then exits)",
                  file=sys.stderr)
            sys.exit(1)
        sock.sendall(struct.pack("<QQQ", args.load_addr, len(payload), args.x0))

        acks, start, last_paint = 0, time.time(), 0.0
        last_progress = time.time()

        def note_progress(before: int, now: int) -> None:
            nonlocal last_progress
            if now != before:
                last_progress = time.time()

        def check_stall(sent: int) -> None:
            # Same wall-clock alarm as the serial path (wider window — see
            # TCP_ACK_STALL_SECONDS): at least one ack overdue and nothing heard for
            # the window means the guest is gone (or the host slept).
            if (
                sent - acks * ACK_INTERVAL >= ACK_INTERVAL
                and time.time() - last_progress > TCP_ACK_STALL_SECONDS
            ):
                print(
                    f"\nno ack progress for {TCP_ACK_STALL_SECONDS:g}s mid-transfer "
                    f"(acked {acks * ACK_INTERVAL}/{len(payload)}) — {peer} gone? re-run",
                    file=sys.stderr,
                )
                sys.exit(1)

        def send_blocking(data: bytes) -> None:
            # tcp_recv_scan leaves the socket non-blocking (timeout 0.0) after its
            # opportunistic drains; sends must block — with a bound, so a wedged peer
            # surfaces as a timeout instead of a hang.
            sock.settimeout(TCP_ACK_STALL_SECONDS)
            sock.sendall(data)

        term = b""
        for off in range(0, len(payload), TCP_CHUNK):
            send_blocking(payload[off : off + TCP_CHUNK])
            sent = min(off + TCP_CHUNK, len(payload))
            before = acks
            acks, term = tcp_recv_scan(sock, acks, block=False)
            note_progress(before, acks)
            check_stall(sent)
            while not term and sent - acks * ACK_INTERVAL > WINDOW:
                before = acks
                acks, term = tcp_recv_scan(sock, acks, block=True)
                note_progress(before, acks)
                check_stall(sent)
            if term:
                print(f"\n{peer} answered {term.decode()!r} mid-transfer", file=sys.stderr)
                sys.exit(1)
            now = time.time()
            if now - last_paint >= 0.5 or sent == len(payload):
                done = acks * ACK_INTERVAL
                sys.stdout.write(
                    f"\racked {done}/{len(payload)} bytes "
                    f"({100 * done / len(payload):.0f}%) {now - start:.0f}s"
                )
                sys.stdout.flush()
                last_paint = now
        send_blocking(struct.pack("<I", crc))

        # Drain the remaining acks + the verdict. Generous on purpose: oskexec only
        # CRCs guest-side, but stickflash WRITES and READ-BACK-VERIFIES the whole slot
        # before its 'K' (write-then-verify before declaring success), at full-speed
        # USB rates on the board (~0.5-1 MiB/s each way) — so the stick deadline
        # scales with the slot.
        if stick:
            deadline = time.time() + 300 + len(payload) / 200_000
        else:
            deadline = time.time() + 120 + len(payload) / 1_000_000
        verdict = b""
        while time.time() < deadline:
            acks, term = tcp_recv_scan(sock, acks, block=True)
            if term:
                verdict = term
                break
        print()
        if verdict == b"K":
            send_blocking(b"G")
            if stick:
                print(
                    f"K: EO9.IMG rewritten and read-back-verified ({acks} acks) — "
                    "confirmation sent. Reset the board to boot the new image."
                )
            else:
                print(
                    f"K: verified ({acks} acks) — go-ahead sent; oskexec is committing. "
                    "Watch the board/QEMU serial console for the new kernel's banner."
                )
        elif verdict == b"E":
            if stick:
                print(
                    "E: stickflash refused or failed — read ITS console narration: a "
                    "refusal before any write leaves the stick untouched; a failure "
                    "after writes began means the stick is possibly torn (re-run; the "
                    "boot CRC gate catches a torn image)",
                    file=sys.stderr,
                )
            else:
                print("E: oskexec refused (crc/stage) — system untouched; re-run",
                      file=sys.stderr)
            sys.exit(1)
        else:
            print(f"no verdict (timeout) — check {peer} is still running; re-run",
                  file=sys.stderr)
            sys.exit(1)
    finally:
        sock.close()


def main() -> None:
    import os

    ap = argparse.ArgumentParser()
    ap.add_argument("image", nargs="?")
    ap.add_argument("--load-addr", type=lambda v: int(v, 0), default=0x0020_0000)
    ap.add_argument("--x0", type=lambda v: int(v, 0), default=0)
    ap.add_argument("--port", default=PORT)
    ap.add_argument("--baud", type=int, default=BAUD)
    ap.add_argument(
        "--tcp",
        default=None,
        metavar="HOST:PORT",
        help="send over TCP to a listening oskexec (network kexec) instead of the "
        "serial stub; requires --secret / EO9_KEXEC_SECRET",
    )
    ap.add_argument(
        "--stick",
        default=None,
        metavar="HOST:PORT",
        help="send over TCP to a listening stickflash (boot-stick rewrite, port 9910): "
        "the same frames as --tcp, but the image is first zero-padded to the EO9.IMG "
        "slot (--slot-mib) so the rewrite is the same-size in-place overwrite "
        "build-stick promises; requires --secret / EO9_KEXEC_SECRET",
    )
    ap.add_argument(
        "--slot-mib",
        type=int,
        default=56,
        help="the stick's fixed EO9.IMG slot size in MiB for --stick padding (must "
        "match what `cargo xtask build-stick` baked; default 56). stickflash refuses "
        "a length that differs from the slot on its stick.",
    )
    ap.add_argument(
        "--secret",
        default=os.environ.get("EO9_KEXEC_SECRET"),
        help="preshared secret for --tcp (>= 16 bytes; defaults to EO9_KEXEC_SECRET "
        "so bench scripts keep it out of command lines)",
    )
    ap.add_argument("--no-console", action="store_true")
    ap.add_argument(
        "--tail-seconds",
        type=float,
        default=20.0,
        help="exit the console tail after this many seconds of SILENCE "
        "(stall alarm; frees the port). 0 = tail forever",
    )
    ap.add_argument("--log", type=str, default=None,
                    help="tee everything (progress + console tail) to this file, line-flushed")
    ap.add_argument("--selftest", action="store_true")
    args = ap.parse_args()
    if args.selftest:
        selftest()
        return
    if not args.image:
        print("image required (or --selftest)", file=sys.stderr)
        sys.exit(2)
    if args.log:
        logf = open(args.log, "a", buffering=1)

        class _Tee:
            def __init__(self, *streams):
                self.streams = streams

            def write(self, data):
                for st in self.streams:
                    st.write(data)

            def flush(self):
                for st in self.streams:
                    st.flush()

        sys.stdout = _Tee(sys.stdout, logf)

    if args.tcp and args.stick:
        print("--tcp and --stick are mutually exclusive (different receivers)",
              file=sys.stderr)
        sys.exit(2)

    payload = open(args.image, "rb").read()

    if args.stick:
        # THE HOST PADS (the documented side — guest/examples/stickflash): zero-fill
        # to the fixed slot so the header length equals the slot, the CRC covers the
        # padded slot (the same value build-stick bakes), and the board-side rewrite
        # stays a same-size in-place cluster overwrite with zero FAT logic.
        slot = args.slot_mib * 1024 * 1024
        if len(payload) > slot:
            print(
                f"{args.image} is {len(payload)} bytes — past the {args.slot_mib} MiB "
                "EO9.IMG slot. Rebuild the stick with a bigger --slot-mib (and pass "
                "the same value here), or trim the image.",
                file=sys.stderr,
            )
            sys.exit(2)
        if len(payload) < slot:
            print(f"padding {len(payload)} bytes to the {args.slot_mib} MiB slot")
            payload = payload + b"\x00" * (slot - len(payload))
        crc = binascii.crc32(payload) & 0xFFFFFFFF
        tcp_send(args, payload, crc, stick=True)
        return

    crc = binascii.crc32(payload) & 0xFFFFFFFF

    if args.tcp:
        tcp_send(args, payload, crc)
        return

    import serial

    expected_acks = len(payload) // ACK_INTERVAL
    print(
        f"{args.image}: {len(payload)} bytes -> 0x{args.load_addr:08x}, "
        f"x0=0x{args.x0:x}, crc {crc:08x}, ~{len(payload) / 150_000:.0f}s at 1.5 Mbaud"
    )

    s = serial.Serial(
        args.port, args.baud, timeout=0.05, write_timeout=WRITE_TIMEOUT_SECONDS
    )
    try:
        s.reset_input_buffer()
        s.write(MAGIC + struct.pack("<QQQ", args.load_addr, len(payload), args.x0))

        acks, start, last_paint = 0, time.time(), 0.0
        # The single ack-progress clock the stall alarm reads: bumped ONLY when the
        # ack count advances, checked on every loop pass (outer and window-block),
        # so the alarm fires within ~10 s of the acks stopping no matter which loop
        # holds control. Wall clock on purpose: a host sleep advances it, so the
        # alarm fires immediately on wake instead of resuming a doomed transfer.
        last_progress = time.time()

        def note_progress(before: int, now: int) -> None:
            nonlocal last_progress
            if now != before:
                last_progress = time.time()

        def check_stall(sent: int) -> None:
            # At least one ack overdue (the stub owes one per 64 KiB delivered) and
            # nothing heard for the stall window: the stub is gone.
            if (
                sent - acks * ACK_INTERVAL >= ACK_INTERVAL
                and time.time() - last_progress > ACK_STALL_SECONDS
            ):
                print(
                    f"\nno ack progress for {ACK_STALL_SECONDS:g}s mid-transfer "
                    f"(acked {acks * ACK_INTERVAL}/{len(payload)}) — stub gone? re-run",
                    file=sys.stderr,
                )
                sys.exit(1)

        for off in range(0, len(payload), CHUNK):
            try:
                s.write(payload[off : off + CHUNK])
            except serial.SerialTimeoutException:
                print(
                    f"\nserial write blocked for {WRITE_TIMEOUT_SECONDS:g}s "
                    "— port driver wedged (host slept?); re-run",
                    file=sys.stderr,
                )
                sys.exit(1)
            sent = min(off + CHUNK, len(payload))
            # Poll acks without blocking; the wire sets the pace.
            before = acks
            acks, term = drain_acks(s, acks, block=False)
            note_progress(before, acks)
            check_stall(sent)
            # Window flow control: block for acks only when too far ahead of the stub.
            while not term and sent - acks * ACK_INTERVAL > WINDOW:
                before = acks
                acks, term = drain_acks(s, acks)  # blocking slice (port timeout)
                note_progress(before, acks)
                check_stall(sent)
            if term:
                print(f"\nstub answered {term.decode()!r} mid-transfer", file=sys.stderr)
                sys.exit(1)
            # Repaint at most twice a second (every chunk floods a tee'd log).
            now = time.time()
            if now - last_paint >= 0.5 or sent == len(payload):
                done = acks * ACK_INTERVAL
                sys.stdout.write(
                    f"\racked {done}/{len(payload)} bytes "
                    f"({100 * done / len(payload):.0f}%) {now - start:.0f}s"
                )
                sys.stdout.flush()
                last_paint = now
        s.write(struct.pack("<I", crc))

        # Wait for the remaining acks + the verdict.
        verdict = b""
        deadline = time.time() + 30 + len(payload) / 100_000
        while time.time() < deadline:
            acks, term = drain_acks(s, acks)
            if term:
                verdict = term
                break
        print()
        if verdict == b"K":
            print(f"K: verified ({acks}/{expected_acks} acks), stub is jumping")
            if not args.no_console:
                console(s, args.tail_seconds)
        elif verdict in (b"E", b"T"):
            print(f"{verdict.decode()}: transfer failed — stub re-armed, re-run")
            sys.exit(1)
        else:
            print("no verdict (timeout) — check the stub is running; re-run")
            sys.exit(1)
    finally:
        s.close()


if __name__ == "__main__":
    main()
