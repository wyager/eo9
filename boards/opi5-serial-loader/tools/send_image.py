#!/usr/bin/env python3
"""Send a kernel image to the serial-loader stub and watch it boot.

    python3 send_image.py kernel/target/eo9-opi5plus-min.img \
        --load-addr 0x00200000 --x0 0xeb9f6c38

Protocol (must match src/lib.rs, pinned by --selftest):
  "EO9L" + <Q load_addr> + <Q length> + <Q x0_value> + payload + <I crc32(payload)>
The stub answers 'k' per 64 KiB (progress), then 'K' (verified, jumping) or 'E'
(CRC mismatch — it re-arms, just re-run) or 'T' (3 s stall — re-run).

Why streaming without flow control is safe: the stub services one byte in well under
1 µs (a couple of MMIO polls) against the line's 6.7 µs/byte at 1.5 Mbaud, so the
UART's 32-byte RX FIFO can never fill. The 'k' bytes are progress truth (macOS buffers
serial writes deeply, so local write counts mean little).

x0_value: pass U-Boot's ${fdtcontroladdr} (`printenv fdtcontroladdr` on the board).
With the booti launch you may pass --x0 0 — the stub then forwards its own entry x0,
which booti already set to the device tree.

After 'K' the stub jumps; this script switches to dumb console mode and tails the
board's output (Ctrl-C to detach; the port is then free for the planner's console).
"""
import argparse
import binascii
import struct
import sys
import time

MAGIC = b"EO9L"
ACK_INTERVAL = 64 * 1024
PORT = "/dev/cu.usbserial-AC009X7K"
BAUD = 1_500_000
CHUNK = 4096


def selftest() -> None:
    assert binascii.crc32(b"123456789") & 0xFFFFFFFF == 0xCBF43926
    hdr = MAGIC + struct.pack("<QQQ", 1, 2, 3)
    assert len(hdr) == 28, len(hdr)
    print("selftest ok: crc vector + 28-byte header")


def drain_acks(s, seen: int) -> tuple[int, bytes]:
    """Consume any pending stub bytes; count 'k's, return (count, terminal byte or b'')."""
    while True:
        chunk = s.read(64)
        if not chunk:
            return seen, b""
        for b in chunk:
            ch = bytes([b])
            if ch == b"k":
                seen += 1
            elif ch in (b"K", b"E", b"T"):
                return seen, ch
            # anything else is line noise; ignore


def console(s) -> None:
    print("--- console (Ctrl-C to detach) ---")
    try:
        while True:
            data = s.read(4096)
            if data:
                sys.stdout.write(data.decode("utf-8", errors="replace"))
                sys.stdout.flush()
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
    ap.add_argument("--selftest", action="store_true")
    args = ap.parse_args()
    if args.selftest:
        selftest()
        return
    if not args.image:
        print("image required (or --selftest)", file=sys.stderr)
        sys.exit(2)

    import serial

    payload = open(args.image, "rb").read()
    crc = binascii.crc32(payload) & 0xFFFFFFFF
    expected_acks = len(payload) // ACK_INTERVAL
    print(
        f"{args.image}: {len(payload)} bytes -> 0x{args.load_addr:08x}, "
        f"x0=0x{args.x0:x}, crc {crc:08x}, ~{len(payload) / 150_000:.0f}s at 1.5 Mbaud"
    )

    s = serial.Serial(args.port, args.baud, timeout=0.05)
    try:
        s.reset_input_buffer()
        s.write(MAGIC + struct.pack("<QQQ", args.load_addr, len(payload), args.x0))

        acks, start = 0, time.time()
        for off in range(0, len(payload), CHUNK):
            s.write(payload[off : off + CHUNK])
            acks, term = drain_acks(s, acks)
            if term:
                print(f"\nstub answered {term.decode()!r} mid-transfer", file=sys.stderr)
                sys.exit(1)
            done = acks * ACK_INTERVAL
            sys.stdout.write(
                f"\racked {done}/{len(payload)} bytes "
                f"({100 * done / len(payload):.0f}%) {time.time() - start:.0f}s"
            )
            sys.stdout.flush()
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
                console(s)
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
