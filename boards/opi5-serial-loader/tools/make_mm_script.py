#!/usr/bin/env python3
"""Bootstrap the stub into board RAM through the vendor U-Boot's interactive `mm.l`.

Two modes:

  --emit PLAN.txt     write the word plan (address: value per line) for inspection
  --send              drive the serial port live, prompt-paced (the robust default:
                      each word is sent only after U-Boot's "? " prompt arrives), then
                      `q`, then `crc32 <addr> <len>` and a local-CRC comparison.

The planner owns the console; run --send only while nothing else holds the port.

    python3 make_mm_script.py target/loader.bin --send
    python3 make_mm_script.py target/loader.bin --emit plan.txt

U-Boot 2017 `mm.l` semantics (cmd/mem.c, mod_mem): prints "<addr>: <cur> ? ", reads a
line; a hex value writes-and-advances; `q` exits. The "? " token is the pacing signal,
so no blind delays are needed; --delay adds an extra safety pause per word anyway.
"""
import argparse
import binascii
import pathlib
import sys
import time

LOAD_ADDR = 0x0400_0000
PORT = "/dev/cu.usbserial-AC009X7K"
BAUD = 1_500_000


def words_of(blob: bytes) -> list[int]:
    pad = (-len(blob)) % 4
    blob += b"\x00" * pad
    return [int.from_bytes(blob[i : i + 4], "little") for i in range(0, len(blob), 4)]


def emit(blob: bytes, out: pathlib.Path) -> None:
    lines = [f"mm.l {LOAD_ADDR:08x}"]
    for i, w in enumerate(words_of(blob)):
        lines.append(f"{LOAD_ADDR + 4 * i:08x}: {w:08x}")
    lines.append("q")
    lines.append(f"crc32 {LOAD_ADDR:08x} {len(blob):x}")
    lines.append(f"# local crc32: {binascii.crc32(blob) & 0xFFFFFFFF:08x}")
    out.write_text("\n".join(lines) + "\n")
    print(f"wrote {out} ({len(words_of(blob))} words)")


def read_until(s, token: bytes, timeout: float) -> bytes:
    buf = b""
    deadline = time.time() + timeout
    while time.time() < deadline:
        chunk = s.read(256)
        if chunk:
            buf += chunk
            if token in buf:
                return buf
    raise TimeoutError(f"no {token!r} within {timeout}s; got: {buf[-200:]!r}")


def send(blob: bytes, port: str, baud: int, delay: float) -> None:
    import serial  # pyserial

    ws = words_of(blob)
    crc_local = binascii.crc32(blob) & 0xFFFFFFFF
    s = serial.Serial(port, baud, timeout=0.2)
    try:
        s.reset_input_buffer()
        s.write(b"\n")
        read_until(s, b"opi#", 5)
        s.write(f"mm.l {LOAD_ADDR:08x}\n".encode())
        start = time.time()
        for i, w in enumerate(ws):
            read_until(s, b"? ", 10)
            s.write(f"{w:08x}\n".encode())
            if delay:
                time.sleep(delay)
            if (i + 1) % 64 == 0:
                pct = 100 * (i + 1) / len(ws)
                sys.stdout.write(f"\r{i + 1}/{len(ws)} words ({pct:.0f}%)")
                sys.stdout.flush()
        read_until(s, b"? ", 10)
        s.write(b"q\n")
        read_until(s, b"opi#", 5)
        print(f"\ntyped {len(ws)} words in {time.time() - start:.1f}s")

        s.write(f"crc32 {LOAD_ADDR:08x} {len(blob):x}\n".encode())
        out = read_until(s, b"opi#", 10).decode(errors="replace")
        print(out)
        if f"{crc_local:08x}" in out.lower():
            print(f"CRC MATCH ({crc_local:08x}) — stub is in RAM, ready to launch:")
            print("  booti 0x04000000 - ${fdtcontroladdr}   (preferred)")
            print("  go 0x04000000                           (fallback; pass x0 via --x0)")
        else:
            print(f"CRC MISMATCH (expected {crc_local:08x}) — re-run --send")
            sys.exit(1)
    finally:
        s.close()


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("binary", type=pathlib.Path)
    ap.add_argument("--emit", type=pathlib.Path)
    ap.add_argument("--send", action="store_true")
    ap.add_argument("--port", default=PORT)
    ap.add_argument("--baud", type=int, default=BAUD)
    ap.add_argument("--delay", type=float, default=0.0, help="extra per-word pause (s)")
    args = ap.parse_args()
    blob = args.binary.read_bytes()
    if args.emit:
        emit(blob, args.emit)
    if args.send:
        send(blob, args.port, args.baud, args.delay)
    if not args.emit and not args.send:
        print("nothing to do: pass --emit and/or --send", file=sys.stderr)
        sys.exit(2)


if __name__ == "__main__":
    main()
