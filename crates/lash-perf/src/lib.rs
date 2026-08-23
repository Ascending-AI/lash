//! Developer-only performance harness for the Lash runtime.
//!
//! This crate is never published or shipped. It owns the synthetic
//! non-inference runtime benchmark (`runtime_perf`, driven by the
//! `lash-perf` binary and `scripts/profile_runtime*.py`) plus its private
//! measurement helpers (`perf_support`). Host applications own their own UI
//! measurement support.

pub mod perf_support;
pub mod runtime_perf;

/// Allocation instrumentation used by the runtime performance harness.
#[cfg(feature = "dhat-heap")]
use std::alloc::{GlobalAlloc, Layout};
#[cfg(feature = "dhat-heap")]
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};

/// The allocator instrumentation mode recorded in runtime performance reports.
/// `dhat-heap+stats_alloc` counters include dhat's own bookkeeping allocations
/// and are not numerically comparable to `stats_alloc`-mode counters; compare
/// within a mode only.
#[cfg(not(feature = "dhat-heap"))]
pub const ALLOCATION_MODE: &str = "stats_alloc";

/// The allocator instrumentation mode recorded in runtime performance reports.
/// `dhat-heap+stats_alloc` counters include dhat's own bookkeeping allocations
/// and are not numerically comparable to `stats_alloc`-mode counters; compare
/// within a mode only.
#[cfg(feature = "dhat-heap")]
pub const ALLOCATION_MODE: &str = "dhat-heap+stats_alloc";

/// Allocation-counter view of the process allocator.
///
/// The `lash-perf` binary installs `stats_alloc::INSTRUMENTED_SYSTEM` as its
/// global allocator, so these counters are live there.
#[cfg(not(feature = "dhat-heap"))]
pub static GLOBAL_ALLOCATOR: &stats_alloc::StatsAlloc<std::alloc::System> =
    &stats_alloc::INSTRUMENTED_SYSTEM;

/// A dhat heap profiler allocator with stats-alloc-compatible counters.
#[cfg(feature = "dhat-heap")]
#[derive(Debug)]
pub struct DhatStatsAllocator {
    allocations: AtomicUsize,
    deallocations: AtomicUsize,
    reallocations: AtomicUsize,
    bytes_allocated: AtomicUsize,
    bytes_deallocated: AtomicUsize,
    bytes_reallocated: AtomicIsize,
    inner: dhat::Alloc,
}

#[cfg(feature = "dhat-heap")]
impl DhatStatsAllocator {
    const fn new() -> Self {
        Self {
            allocations: AtomicUsize::new(0),
            deallocations: AtomicUsize::new(0),
            reallocations: AtomicUsize::new(0),
            bytes_allocated: AtomicUsize::new(0),
            bytes_deallocated: AtomicUsize::new(0),
            bytes_reallocated: AtomicIsize::new(0),
            inner: dhat::Alloc,
        }
    }

    /// Snapshot counters using the same schema as the normal stats allocator.
    pub fn stats(&self) -> stats_alloc::Stats {
        stats_alloc::Stats {
            allocations: self.allocations.load(Ordering::SeqCst),
            deallocations: self.deallocations.load(Ordering::SeqCst),
            reallocations: self.reallocations.load(Ordering::SeqCst),
            bytes_allocated: self.bytes_allocated.load(Ordering::SeqCst),
            bytes_deallocated: self.bytes_deallocated.load(Ordering::SeqCst),
            bytes_reallocated: self.bytes_reallocated.load(Ordering::SeqCst),
        }
    }
}

#[cfg(feature = "dhat-heap")]
pub static GLOBAL_ALLOCATOR: DhatStatsAllocator = DhatStatsAllocator::new();

#[cfg(feature = "dhat-heap")]
unsafe impl GlobalAlloc for DhatStatsAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.allocations.fetch_add(1, Ordering::SeqCst);
        self.bytes_allocated
            .fetch_add(layout.size(), Ordering::SeqCst);
        // SAFETY: The caller upholds GlobalAlloc's allocation contract.
        unsafe { self.inner.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.deallocations.fetch_add(1, Ordering::SeqCst);
        self.bytes_deallocated
            .fetch_add(layout.size(), Ordering::SeqCst);
        // SAFETY: The caller upholds GlobalAlloc's deallocation contract.
        unsafe { self.inner.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        self.allocations.fetch_add(1, Ordering::SeqCst);
        self.bytes_allocated
            .fetch_add(layout.size(), Ordering::SeqCst);
        // SAFETY: The caller upholds GlobalAlloc's allocation contract.
        unsafe { self.inner.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        self.reallocations.fetch_add(1, Ordering::SeqCst);
        if new_size > layout.size() {
            self.bytes_allocated
                .fetch_add(new_size - layout.size(), Ordering::SeqCst);
        } else if new_size < layout.size() {
            self.bytes_deallocated
                .fetch_add(layout.size() - new_size, Ordering::SeqCst);
        }
        self.bytes_reallocated.fetch_add(
            new_size.wrapping_sub(layout.size()) as isize,
            Ordering::SeqCst,
        );
        // SAFETY: The caller upholds GlobalAlloc's reallocation contract.
        unsafe { self.inner.realloc(ptr, layout, new_size) }
    }
}

#[cfg(feature = "dhat-heap")]
unsafe impl GlobalAlloc for &DhatStatsAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: Forwarding the caller's GlobalAlloc contract.
        unsafe { (**self).alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: Forwarding the caller's GlobalAlloc contract.
        unsafe { (**self).dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: Forwarding the caller's GlobalAlloc contract.
        unsafe { (**self).alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: Forwarding the caller's GlobalAlloc contract.
        unsafe { (**self).realloc(ptr, layout, new_size) }
    }
}
