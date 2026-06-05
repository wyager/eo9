#!/usr/bin/env python3
"""Flatten the loader ELF into the raw binary `mm` types into RAM.

Same idea as xtask's PT_LOAD flattener: concatenate every PT_LOAD segment at its
physical-address offset from the image base, zero-padding gaps. Only filesz bytes are
emitted — .bss (and the stack reservation) exist solely in memsz and are zeroed by the
stub's own entry code.

Usage: flatten.py <elf> <out.bin>
"""
import struct
import sys


def main() -> None:
    elf_path, out_path = sys.argv[1], sys.argv[2]
    data = open(elf_path, "rb").read()
    assert data[:4] == b"\x7fELF" and data[4] == 2 and data[5] == 1, "not a 64-bit LE ELF"
    (e_phoff,) = struct.unpack_from("<Q", data, 0x20)
    (e_phentsize, e_phnum) = struct.unpack_from("<HH", data, 0x36)

    loads = []
    for i in range(e_phnum):
        off = e_phoff + i * e_phentsize
        p_type, _flags = struct.unpack_from("<II", data, off)
        p_offset, _vaddr, p_paddr, p_filesz, _memsz = struct.unpack_from(
            "<QQQQQ", data, off + 8
        )
        if p_type == 1 and p_filesz > 0:  # PT_LOAD
            loads.append((p_paddr, data[p_offset : p_offset + p_filesz]))

    assert loads, "no PT_LOAD segments"
    loads.sort()
    base = loads[0][0]
    end = max(addr + len(seg) for addr, seg in loads)
    image = bytearray(end - base)
    for addr, seg in loads:
        image[addr - base : addr - base + len(seg)] = seg

    open(out_path, "wb").write(bytes(image))
    print(f"flattened {len(image)} bytes, base 0x{base:08x}")


if __name__ == "__main__":
    main()
