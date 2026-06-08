#!/usr/bin/env python3
"""Send a kernel image to the serial-loader stub and watch it boot.

    python3 send_image.py kernel/target/eo9-opi5plus-min.img \
        --load-addr 0x00200000 --x0 0xeb9f6c38

Protocol (must match src/lib.rs, pinned by --selftest):
  "EO9L" + <Q load_addr> + <Q length> + <Q x0_value> + payload + <I crc32(payload)>
The stub answers 'k' per 64 KiB (progress), then 'K' (verified, jumping) or 'E'
(CRC mismatch — it re-arms, just re-run) or 'T' (3 s stall — re-run).

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


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("image", nargs="?")
    ap.add_argument("--load-addr", type=lambda v: int(v, 0), default=0x0020_0000)
    ap.add_argument("--x0", type=lambda v: int(v, 0), default=0)
    ap.add_argument("--port", default=PORT)
    ap.add_argument("--baud", type=int, default=BAUD)
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

    import serial

    payload = open(args.image, "rb").read()
    crc = binascii.crc32(payload) & 0xFFFFFFFF
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
