use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

pub struct CountingAllocator;

thread_local! {
    static ALLOCATED: Cell<u64> = const { Cell::new(0) };
}

fn charge(bytes: usize) {
    let _ = ALLOCATED.try_with(|counter| counter.set(counter.get().saturating_add(bytes as u64)));
}

pub fn total() -> u64 {
    ALLOCATED.try_with(Cell::get).unwrap_or(0)
}

// This executable only measures single-threaded corpus codecs. Each method
// preserves System's pointer/layout contract and never allocates in charge.
#[expect(
    unsafe_code,
    reason = "benchmark-only System allocator forwarding counts cumulative decode allocation"
)]
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            charge(layout.size());
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            charge(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let pointer = unsafe { System.realloc(pointer, layout, new_size) };
        // Count the entire requested allocation, including repeated buffers,
        // rather than only net growth. This is a cumulative admission budget.
        if !pointer.is_null() {
            charge(new_size);
        }
        pointer
    }
}
