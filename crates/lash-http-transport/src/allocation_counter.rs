//! Test-thread allocation requests, excluding prebuilt transport chunks.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Counts {
    pub(crate) allocated: usize,
    pub(crate) largest: usize,
}

thread_local! {
    static COUNTS: Cell<Option<Counts>> = const { Cell::new(None) };
}

fn charge(bytes: usize) {
    let _ = COUNTS.try_with(|cell| {
        if let Some(mut counts) = cell.get() {
            counts.allocated += bytes;
            counts.largest = counts.largest.max(bytes);
            cell.set(Some(counts));
        }
    });
}

struct Counter;

#[global_allocator]
static ALLOCATOR: Counter = Counter;

#[expect(
    unsafe_code,
    reason = "test allocator forwards the unchanged GlobalAlloc contract to System"
)]
unsafe impl GlobalAlloc for Counter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        charge(layout.size());
        // SAFETY: The caller supplies a valid layout; System receives it unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        charge(layout.size());
        // SAFETY: The caller supplies a valid layout; System receives it unchanged.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: Allocations came from System with the same layout.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        charge(new_size);
        // SAFETY: The pointer, layout and new size keep the caller's contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

pub(crate) struct Measurement;

impl Measurement {
    pub(crate) fn start() -> Self {
        COUNTS.with(|cell| {
            assert!(cell.get().is_none(), "allocation measurements cannot nest");
            cell.set(Some(Counts::default()));
        });
        Self
    }

    pub(crate) fn finish(self) -> Counts {
        COUNTS.with(|cell| cell.take().expect("active allocation measurement"))
    }
}

impl Drop for Measurement {
    fn drop(&mut self) {
        COUNTS.with(|cell| cell.set(None));
    }
}
