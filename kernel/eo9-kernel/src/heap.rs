//! Global heap allocator (no_std + alloc).
//!
//! The heap spans from the end of the kernel image (`__heap_start`, linker script) to the
//! top of the 512 MiB of RAM that xtask's QEMU invocation provides. A free-list allocator
//! (`linked_list_allocator`) backs `#[global_allocator]`; it is small, reclaims frees, and
//! is entirely adequate for the single-core spike — wasmtime's runtime allocates and frees
//! continuously, so a bump allocator would not do.

use linked_list_allocator::LockedHeap;

/// Start of RAM on the QEMU `virt` machine.
const RAM_BASE: usize = 0x4000_0000;
/// RAM size the kernel assumes; must match the `-m` value in xtask's QEMU invocation.
const RAM_SIZE: usize = 512 * 1024 * 1024;
/// First byte past the heap.
const HEAP_END: usize = RAM_BASE + RAM_SIZE;

#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

unsafe extern "C" {
    /// Provided by the linker script: first free byte after the kernel image and stack.
    static __heap_start: u8;
}

/// Hand every byte between the kernel image and the top of RAM to the allocator.
pub fn init() {
    let start = (&raw const __heap_start).addr();
    let size = HEAP_END
        .checked_sub(start)
        .expect("kernel image overflows RAM");
    // SAFETY: the region [start, HEAP_END) is normal RAM, unused by the image, the stack,
    // or the DTB, and `init` is called exactly once before any allocation.
    unsafe { ALLOCATOR.lock().init(start as *mut u8, size) };
    crate::kprintln!(
        "heap: {} MiB at {:#010x}..{:#010x}",
        size / (1024 * 1024),
        start,
        HEAP_END
    );
}

/// Exercise alloc/free through the global allocator and report over serial. Panics (and
/// therefore powers off with a message) if anything is inconsistent.
pub fn self_test() {
    use alloc::boxed::Box;
    use alloc::string::String;
    use alloc::vec::Vec;

    // Boxing and reading back.
    let boxed = Box::new(0xE09_u64);
    assert_eq!(*boxed, 0xE09);

    // A growing vector with a checkable sum.
    let mut numbers: Vec<u64> = Vec::new();
    for value in 0..10_000_u64 {
        numbers.push(value);
    }
    assert_eq!(numbers.iter().sum::<u64>(), 49_995_000);

    // A larger allocation, written and verified, then freed.
    let mut big = alloc::vec![0xA5_u8; 4 * 1024 * 1024];
    let last = big.len() - 1;
    big[last] = 0x5A;
    assert_eq!(big[0], 0xA5);
    assert_eq!(big[last], 0x5A);
    drop(big);

    // Heap-backed formatting (what kprintln! of dynamic data ultimately relies on).
    let mut message = String::from("heap self-test passed");
    message.push_str(" (box, 10k-element vec, 4 MiB buffer, string)");

    let used = ALLOCATOR.lock().used();
    let free = ALLOCATOR.lock().free();
    crate::kprintln!(
        "{message}; {used} bytes in use, {} MiB free",
        free / (1024 * 1024)
    );
}
