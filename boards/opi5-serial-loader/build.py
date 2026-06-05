#!/usr/bin/env python3
"""Build the stub: cargo (aarch64-unknown-none, release) -> flatten -> size report.

Run from the crate directory (all compilation stays inside the repo tree):

    python3 build.py

Emits target/loader.bin and prints the size + the first instructions (sanity).
"""
import pathlib
import subprocess
import sys

HERE = pathlib.Path(__file__).resolve().parent
ELF = HERE / "target" / "aarch64-unknown-none" / "release" / "opi5-serial-loader"
OUT = HERE / "target" / "loader.bin"


def run(*cmd: str) -> None:
    print("+", " ".join(cmd))
    subprocess.run(cmd, cwd=HERE, check=True)


def main() -> None:
    run("cargo", "build", "--release")
    run(sys.executable, "flatten.py", str(ELF), str(OUT))
    size = OUT.stat().st_size
    print(f"loader.bin: {size} bytes ({size / 1024:.1f} KiB), {(size + 3) // 4} mm words")
    if size > 4096:
        print("WARNING: stub exceeds the 4 KiB target")
    # First instructions, for the bench log (macOS objdump is llvm-objdump: reads ELF).
    subprocess.run(
        ["objdump", "-d", "--section=.text", str(ELF)], cwd=HERE, check=False
    )


if __name__ == "__main__":
    main()
