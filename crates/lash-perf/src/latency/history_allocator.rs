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

/// Set once, before the process starts another thread, then only read.
/// A plain static rather than an atomic: its read is one inline load in
/// every build, where an atomic's generic load can stay an out-of-line call.
static mut PLAIN: bool = false;

/// Sends every later allocation straight to `System`, past the
/// executable's diagnostic layers (allocation counts, history
/// attribution, stats_alloc counters). A command that reads none of those
/// figures calls this first, before it starts a thread, so its instruction
/// counts carry no counter work. The layers' memory is `System` memory, so
/// either path frees it.
///
/// # Safety
/// No other thread may be running.
pub unsafe fn plain_allocation() {
    // SAFETY: the caller is the process's only thread.
    unsafe { PLAIN = true };
}

/// The executable's diagnostic allocator stack `A`, until
/// [`plain_allocation`] turns it off.
pub struct Diagnosed<A>(pub A);
// SAFETY: both paths end in `System`, which every layer of `A` forwards to
// with the same pointer and layout; `PLAIN` changes only while one thread runs.
unsafe impl<A: GlobalAlloc> GlobalAlloc for Diagnosed<A> {
    #[inline(always)]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if unsafe { PLAIN } {
            unsafe { std::alloc::System.alloc(layout) }
        } else {
            unsafe { self.0.alloc(layout) }
        }
    }
    #[inline(always)]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if unsafe { PLAIN } {
            unsafe { std::alloc::System.alloc_zeroed(layout) }
        } else {
            unsafe { self.0.alloc_zeroed(layout) }
        }
    }
    #[inline(always)]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if unsafe { PLAIN } {
            unsafe { std::alloc::System.dealloc(ptr, layout) }
        } else {
            unsafe { self.0.dealloc(ptr, layout) }
        }
    }
    #[inline(always)]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if unsafe { PLAIN } {
            unsafe { std::alloc::System.realloc(ptr, layout, size) }
        } else {
            unsafe { self.0.realloc(ptr, layout, size) }
        }
    }
}
