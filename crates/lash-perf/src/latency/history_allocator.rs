//! Allocation attribution exists only in this developer executable. The inner
//! allocator still owns the established process-wide stats_alloc counters.
#![expect(
    unsafe_code,
    reason = "GlobalAlloc forwards identical pointers and layouts"
)]
use lash_core_ids::perf_witness::history::allocation;
use std::alloc::{GlobalAlloc, Layout};

pub struct HistoryAllocator<A>(pub A);
// SAFETY: every operation delegates to the inner allocator with its original
// arguments; the observation hook does not allocate, lock, or touch the pointer.
unsafe impl<A: GlobalAlloc> GlobalAlloc for HistoryAllocator<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { self.0.alloc(layout) };
        if !ptr.is_null() {
            allocation(1, layout.size());
        }
        ptr
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { self.0.alloc_zeroed(layout) };
        if !ptr.is_null() {
            allocation(1, layout.size());
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { self.0.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let result = unsafe { self.0.realloc(ptr, layout, size) };
        if !result.is_null() {
            allocation(0, size.saturating_sub(layout.size()));
        }
        result
    }
}
