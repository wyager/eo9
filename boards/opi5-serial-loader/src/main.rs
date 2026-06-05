//! The bare-metal serial-loader stub for the Orange Pi 5 Plus (RK3588).
//!
//! Launched from the vendor U-Boot either via `booti 0x04000000 - ${fdtcontroladdr}`
//! (preferred: U-Boot flushes caches and enters with the MMU and D-cache OFF, x0 = the
//! device tree — exactly the Linux arm64 boot protocol; the Image header below carries
//! text_offset 0x03E0_0000 so the vendor relocation lands the stub exactly where `mm`
//! typed it, dram_base 0x0020_0000 + 0x03E0_0000 == 0x0400_0000, a no-op move) or via
//! `go 0x04000000` (fallback: MMU and caches stay ON as U-Boot runs them, x0 = argc —
//! pass the real device-tree address in the protocol header's `x0_value` instead; the
//! pre-jump `dc cvau` sweep makes the freshly written payload fetchable either way).
//!
//! The stub never reconfigures the UART — the line stays exactly as U-Boot programmed
//! it (1.5 Mbaud, FIFOs on). It only reads LSR and RBR and writes THR.

#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]

#[cfg(not(target_os = "none"))]
fn main() {
    eprintln!("the serial-loader stub only runs on the board; see tools/ for the host side");
}

#[cfg(target_os = "none")]
mod bare {
    use core::arch::{asm, global_asm};
    use core::ptr::{read_volatile, write_volatile};
    use opi5_serial_loader::{
        crc32_update, ACK_INTERVAL, HEADER_LEN, MAGIC, MAX_LENGTH, STUB_BASE, STUB_GUARD_LEN,
    };

    // The arm64 Image header (64 bytes) + the entry trampoline. `code0` branches over
    // the header; `mm`-typed and `booti`/`go`-launched alike enter at offset 0.
    global_asm!(
        r#"
        .section .text.head, "ax"
        .globl _head
    _head:
        b       _start              // code0
        .word   0                   // code1
        .quad   0x03E00000          // text_offset: dram_base 0x200000 + this == 0x04000000
        .quad   0x8000              // image_size: code + bss + stack reservation
        .quad   0                   // flags: LE, 4K
        .quad   0                   // res2
        .quad   0                   // res3
        .quad   0                   // res4
        .ascii  "ARM\x64"           // magic
        .word   0                   // res5

        .globl _start
    _start:
        mov     x19, x0             // preserve the entry x0 (booti: the device tree)
        // Zero .bss (includes the stack reservation).
        ldr     x1, =__bss_start
        ldr     x2, =__bss_end
    1:  cmp     x1, x2
        b.hs    2f
        str     xzr, [x1], #8
        b       1b
    2:  ldr     x3, =_stack_top
        mov     sp, x3
        mov     x0, x19
        bl      stub_main
    3:  wfe
        b       3b
        "#
    );

    #[panic_handler]
    fn panic(_: &core::panic::PanicInfo) -> ! {
        loop {
            unsafe { asm!("wfe") };
        }
    }

    // ---- DW-APB UART2, exactly as the kernel board profile drives it ----------------
    // (kernel/eo9-kernel/src/arch/aarch64/uart.rs `board-opi5plus` arm: base 0xfeb5_0000,
    // reg stride 4, io width 4; RBR/THR at +0x00, LSR at +0x14, DR bit0, THRE bit5.)
    const UART_BASE: usize = 0xfeb5_0000;
    const DW_RBR_THR: usize = 0x00;
    const DW_LSR: usize = 0x14;
    const DW_LSR_DR: u32 = 1 << 0;
    const DW_LSR_THRE: u32 = 1 << 5;

    #[inline]
    fn mmio_read(off: usize) -> u32 {
        // Real EL2 silicon — no hypervisor syndrome decoding to appease; a plain
        // volatile is the whole contract here.
        unsafe { read_volatile((UART_BASE + off) as *const u32) }
    }

    #[inline]
    fn mmio_write(off: usize, v: u32) {
        unsafe { write_volatile((UART_BASE + off) as *mut u32, v) }
    }

    #[inline]
    fn tx(byte: u8) {
        while mmio_read(DW_LSR) & DW_LSR_THRE == 0 {}
        mmio_write(DW_RBR_THR, u32::from(byte));
    }

    #[inline]
    fn rx_ready() -> bool {
        mmio_read(DW_LSR) & DW_LSR_DR != 0
    }

    #[inline]
    fn rx_pop() -> u8 {
        (mmio_read(DW_RBR_THR) & 0xFF) as u8
    }

    // ---- Generic timer (for the mid-transfer stall timeout) -------------------------

    #[inline]
    fn cnt_frq() -> u64 {
        let f: u64;
        unsafe { asm!("mrs {0}, cntfrq_el0", out(reg) f) };
        f
    }

    #[inline]
    fn cnt_now() -> u64 {
        let t: u64;
        unsafe { asm!("isb", "mrs {0}, cntvct_el0", out(reg) t) };
        t
    }

    /// Blocking byte read. With `deadline_ticks == 0` waits forever (idle state);
    /// otherwise gives up after that many timer ticks of silence.
    fn rx_byte(deadline_ticks: u64) -> Option<u8> {
        if deadline_ticks == 0 {
            while !rx_ready() {}
            return Some(rx_pop());
        }
        let start = cnt_now();
        while !rx_ready() {
            if cnt_now().wrapping_sub(start) > deadline_ticks {
                return None;
            }
        }
        Some(rx_pop())
    }

    // ---- The pre-jump cache sweep ----------------------------------------------------
    // Under `go` the payload was written through the live D-cache: clean it to the point
    // of unification so instruction fetch sees it. Under `booti` the D-cache is off and
    // every `dc cvau` is a cheap no-op on clean lines. Either way, drop the whole
    // I-cache (one instruction) rather than 200k `ic ivau`s.
    unsafe fn make_executable(start: u64, len: u64) {
        let mut line = start & !63;
        let end = start.wrapping_add(len);
        while line < end {
            asm!("dc cvau, {0}", in(reg) line);
            line += 64;
        }
        asm!("dsb ish");
        asm!("ic iallu");
        asm!("dsb ish");
        asm!("isb");
    }

    /// Jump to the loaded image with the Linux boot register contract.
    unsafe fn jump(addr: u64, x0: u64) -> ! {
        asm!(
            "mov x1, xzr",
            "mov x2, xzr",
            "mov x3, xzr",
            "br  {addr}",
            addr = in(reg) addr,
            in("x0") x0,
            options(noreturn)
        );
    }

    fn read_u64_le(timeout: u64) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(8) {
            v |= u64::from(rx_byte(timeout)?) << shift;
        }
        Some(v)
    }

    #[no_mangle]
    extern "C" fn stub_main(entry_x0: u64) -> ! {
        // ~3 s of mid-transfer silence aborts the frame.
        let stall = cnt_frq().saturating_mul(3);

        'idle: loop {
            // Hunt for the magic byte sequence; garbage (console noise, a re-opened
            // port) just slides through the shift window.
            let mut matched = 0usize;
            while matched < MAGIC.len() {
                let b = rx_byte(0).unwrap(); // idle wait is infinite, never None
                if b == MAGIC[matched] {
                    matched += 1;
                } else {
                    matched = usize::from(b == MAGIC[0]);
                }
            }

            // Header: load_addr, length, x0_value.
            let (load, len, x0v) =
                match (read_u64_le(stall), read_u64_le(stall), read_u64_le(stall)) {
                    (Some(a), Some(l), Some(x)) => (a, l, x),
                    _ => {
                        tx(b'T');
                        continue 'idle;
                    }
                };
            let _ = HEADER_LEN; // layout pinned by the lib tests

            // Refuse obvious corruption and anything that would overwrite the stub.
            let overlaps_stub =
                load < STUB_BASE + STUB_GUARD_LEN && load.wrapping_add(len) > STUB_BASE;
            if len == 0 || len > MAX_LENGTH || overlaps_stub {
                tx(b'E');
                continue 'idle;
            }

            // Payload: store + CRC in one pass, a `k` per 64 KiB.
            let mut crc = 0xFFFF_FFFFu32;
            let mut i = 0u64;
            while i < len {
                let Some(b) = rx_byte(stall) else {
                    tx(b'T');
                    continue 'idle;
                };
                unsafe { write_volatile((load + i) as *mut u8, b) };
                crc = crc32_update(crc, b);
                i += 1;
                if i.is_multiple_of(ACK_INTERVAL) {
                    tx(b'k');
                }
            }
            let crc = !crc;

            let Some(wire) = ({
                let mut v = 0u32;
                let mut ok = true;
                for shift in (0..32).step_by(8) {
                    match rx_byte(stall) {
                        Some(b) => v |= u32::from(b) << shift,
                        None => {
                            ok = false;
                            break;
                        }
                    }
                }
                ok.then_some(v)
            }) else {
                tx(b'T');
                continue 'idle;
            };

            if wire != crc {
                tx(b'E');
                continue 'idle;
            }

            tx(b'K');
            unsafe {
                make_executable(load, len);
                jump(load, if x0v == 0 { entry_x0 } else { x0v });
            }
        }
    }
}
