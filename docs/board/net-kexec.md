# Network kexec — flash the next image over TCP

Lane 21 (area/21-kexec). Receive a new Eo9 image over TCP into reserved RAM and jump
into it, so board iteration runs at ethernet speed; the UART serial loader
(boards/opi5-serial-loader) demotes to recovery-only. QEMU gate:
`cargo xtask check-kexec`.

## Pieces

- **wit/kexec** — `eo9:kexec`: `stage(offset, chunk)` + `commit(len, crc32, bootargs)`.
  TOTAL AUTHORITY (the staged image becomes the machine); never linked by default.
- **`kexec` boot token** — the `pci`/`platform`/`gfx` grant grammar
  (kernel `runner::boot`); the grant is announced loudly at boot. aarch64 only.
- **Staging region** — the top 64 MiB of the DRAM window, carved out of the heap in the
  memory map itself (kernel `mmu.rs`, like the bootargs page):
  board stub `0x1D00_0000` + staging `0x1D01_0000..0x2100_0000`;
  QEMU virt stub `0x5C00_0000` + staging `0x5C01_0000..0x6000_0000`.
  Outside the allocator → unreachable by every other capability.
- **Staged-bootargs page** — `0x0010_0000` (board; QEMU `0x4010_0000`), one printable
  NUL/newline-terminated line (usb-boot-demo-plan Part A, Option 1). The board
  `fdt::bootargs` falls back to it LAST — a valid x0 FDT always wins, so the serial
  path is byte-for-byte unchanged.
- **oskexec** (guest/examples/oskexec) — TCP listener :9909, serial-loader framing plus
  a mandatory preshared-secret frame; one-shot; typed refusals.
- **send_image.py --tcp** (boards/opi5-serial-loader/tools) — same file/CRC handling as
  the serial path, TCP transport, ack-driven progress, the wall-clock stall alarm,
  `--secret` / `EO9_KEXEC_SECRET`.

## The dance (commit, after the CRC verifies)

1. `kexec: jumping to the staged image (N bytes, crc ok)` — the dying kernel's last line.
2. Bus mastering cleared on every enumerated PCI function (machine-wide quiesce; the
   per-task teardown discipline, applied to everything because the machine is ending).
3. Bootargs written to the staged page; watchdog patted.
4. Relocation stub (~14 instructions, position-independent) copied to its slot; staged
   image + stub + bootargs page swept to PoC while caches are on.
5. Final asm block (registers only — no stack after this point): DAIF masked, the
   **target window swept to PoC** (without this, the dying kernel's dirty cache lines
   over its own image/heap would write back OVER the freshly copied bytes when the new
   kernel's entry sweep cleans them), `ic iallu`, MMU/D/I off, branch to the stub.
6. Stub copies staging → link address with uncached 16-byte moves (stores land at PoC
   by construction), drops the I-cache, jumps with **x0 = 0** (deliberate junk → the
   staged-bootargs fallback; QEMU's RAM-base DTB survives and wins there).

First-instructions hazard (usb-boot plan §A.2): with SCTLR.{M,C,I}=0 the copy's stores
go straight to DRAM, so nothing remains above the PoC; the only stale state is old
I-cache lines, removed by `ic iallu` twice (before and after the copy). The board
kernel's entry self-sweep is the belt-and-braces second layer.

**EL1-entry check (lane precondition)**: the board trampoline in
`kernel/eo9-kernel/src/arch/aarch64/boot.rs` already tolerates EL1 entry —
`cmp x1, #2; b.ne 9f` skips the EL2 drop and bangs the `'b'` beacon. A kexec'd kernel
enters at EL1 (the running kernel's level) and takes exactly that path; U-Boot's EL2
entry is untouched. No fix was needed.

## Watchdog across the jump

The receive keeps the drive loop alive (the guest pump pats as normal). From `commit`'s
verification to the new kernel's `wdt::arm_and_report` ('G' milestone): a PoC sweep of
≤64 MiB + ≤62 MiB (sub-second each), an uncached 16-byte-stride copy of ≤62 MiB
(~1 s-class), and the new kernel's entry+MMU+heap path — comfortably inside the 22.4 s
timeout, and `commit` pats once just before the final block.

## Loop safety (board)

A wild jump after a good CRC (an image that is whole but wrong) hangs silently → the
watchdog fires at ~22 s → reset → U-Boot → the serial loader recovery path. The
network path never makes the bench less recoverable than the serial wire it replaces.
The 62 MiB image cap also keeps the staged copy below the serial stub home
(`0x0400_0000`).

## Security posture

- The `kexec` **token** gates which *programs* can reach stage/commit (and the
  provider is never linked by default).
- The **preshared secret** gates which *network peer* can drive oskexec: mandatory,
  ≥ 16 bytes, full-length compare, one retry then the program exits, one-shot service
  (the listener exists only while the operator is flashing).
- **Residual, stated loudly**: the secret travels CLEARTEXT on the LAN (the telnet
  posture precedent, but with a real gate because the authority is total). A passive
  sniffer who also wins the race inside the one-shot window could replay it.
  Trusted-LAN/bench tool only. Upgrade path: challenge-response (needs a real hash in
  the guest world) — recorded follow-up, deliberately not hand-rolled.
- CRC-32 is integrity, not authenticity: the sender supplies both bytes and CRC.

## Bench protocol (the board round)

Serial console (planner-held), after the serial-loaded kernel boots with bootargs
carrying the grants, e.g.:

    program=eosh pci kexec        # or the full composition boot; `kexec` is the grant

At the eosh prompt (static LAN addressing; DHCP works too):

    net.rtl8125 $ (net.l4.over-l2 --address 10.20.3.70 --prefix-length 24
      --gateway 10.20.3.1) $ oskexec --secret <16+ bytes> --bootargs "pci kexec"

oskexec narrates `listening on :9909`. From the bench host:

    EO9_KEXEC_SECRET=<same secret> python3 boards/opi5-serial-loader/tools/send_image.py \
        kernel/target/eo9-opi5plus.img --tcp 10.20.3.70:9909

Watch the serial console: `kexec: jumping…` → beacons `A b C H` (note `b`, EL1-direct)
→ the new banner. The new kernel's bootargs are whatever `--bootargs` carried (the
staged page); include `kexec` in them to keep the loop flashable. Recovery at any
point: watchdog → U-Boot → serial loader (`go 0x04000000` + send_image.py over UART).

## QEMU gate

`cargo xtask check-kexec`: kernel A (plain) boots with `pci kexec` and a derived-port
slirp forward to :9909; kernel B (same tree, banner-stamped `kexec-B`) is flattened and
sent with `send_image.py --tcp`; the gate asserts, on one serial stream: the grant
line → oskexec listening → the transfer (sender ack-alarmed, guest narrating per
4 MiB) → `kexec: jumping` → `build stamp: kexec-B` → a live prompt → clean exit.
Under TCG the transfer paces at the guest recv loop (~16 min for a full image) — the
board runs native and is bounded by the wire instead.
