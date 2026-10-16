//! TEXT-PROJECTION-ONE-BUFFER: certified text needs one host output buffer.
use super::embedder::Source;
use crate::data::text_copy;
use lash_kernel_doc::NativeHeap;
use lash_kernel_doc::heap::*;
use lash_kernel_state::paged::{
    EditEpoch, PageAnswer, PageBody, PageKind, PageSource, PagedHeap, SemanticHeap,
};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::RefCell;
use std::ops::ControlFlow;

#[derive(Clone, Copy, Default)]
struct Allocation {
    pointer: usize,
    previous: usize,
    bytes: usize,
}

// Fixed storage keeps the observer out of the allocation stream. Only the
// test thread is observed; page decoding allocations are excluded by tracing
// the returned String's allocation lineage backwards.
struct Window {
    events: [Allocation; 4096],
    len: usize,
}
thread_local! {
    static WINDOW: RefCell<Option<Window>> = const { RefCell::new(None) };
}
fn record(pointer: *mut u8, previous: usize, bytes: usize) {
    let _ = WINDOW.try_with(|window| {
        if let Some(window) = window.borrow_mut().as_mut() {
            let index = window.len;
            if let Some(event) = window.events.get_mut(index) {
                *event = Allocation {
                    pointer: pointer as usize,
                    previous,
                    bytes,
                };
            }
            window.len += 1;
        }
    });
}
struct Counter;
#[global_allocator]
static ALLOCATOR: Counter = Counter;
#[expect(
    unsafe_code,
    reason = "test allocator forwards the GlobalAlloc contract to System"
)]
unsafe impl GlobalAlloc for Counter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: The caller supplies System's layout unchanged.
        let pointer = unsafe { System.alloc(layout) };
        record(pointer, 0, layout.size());
        pointer
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: The caller supplies System's layout unchanged.
        let pointer = unsafe { System.alloc_zeroed(layout) };
        record(pointer, 0, layout.size());
        pointer
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, bytes: usize) -> *mut u8 {
        let previous = pointer as usize;
        // SAFETY: The pointer, layout and size retain the caller's contract.
        let pointer = unsafe { System.realloc(pointer, layout, bytes) };
        record(pointer, previous, bytes);
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: The allocation came from System with the same layout.
        unsafe { System.dealloc(pointer, layout) }
    }
}
struct Measurement;
impl Measurement {
    fn start() -> Self {
        WINDOW.with(|window| {
            assert!(window.borrow().is_none());
            *window.borrow_mut() = Some(Window {
                events: [Allocation::default(); 4096],
                len: 0,
            });
        });
        Self
    }
    fn finish(self, output: &str) -> (usize, usize) {
        let window = WINDOW.with(|window| window.borrow_mut().take().unwrap());
        assert!(
            window.len <= window.events.len(),
            "allocation observer overflow"
        );
        let mut pointer = output.as_ptr() as usize;
        let mut calls = 0;
        let mut bytes = 0;
        for event in window.events[..window.len].iter().rev() {
            if event.pointer == pointer {
                calls += 1;
                bytes += event.bytes;
                pointer = event.previous;
                if pointer == 0 {
                    break;
                }
            }
        }
        assert_eq!(pointer, 0, "output's initial allocation was observed");
        (calls, bytes)
    }
}
impl Drop for Measurement {
    fn drop(&mut self) {
        WINDOW.with(|window| *window.borrow_mut() = None);
    }
}

struct FaultSource {
    source: Source,
    page: PageId,
    fault: HeapFault,
}
impl PageSource for FaultSource {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<PageAnswer>, HeapFault> {
        if pages.iter().any(|page| page.id == self.page) {
            return Err(self.fault.clone());
        }
        self.source.read(pages)
    }
    fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
        self.source.seal(body)
    }
}

#[test]
fn text_projection_one_buffer() {
    let source = Source::default();
    let mut heap = PagedHeap::fresh(Box::new(source.clone()), EditEpoch(1));
    // Different final leaf, with multibyte scalars at leaf boundaries.
    let expected = format!("{}終", "aé🦀".repeat(3001));
    let text = heap.text(&expected).unwrap();
    assert!(matches!(text.body, Payload::Paged(_)));
    let mut chunks = 0;
    heap.payload_visit(
        &lash_kernel_doc::Value::Text(text.clone()),
        0..text.summary.utf8_bytes,
        &mut |_| {
            chunks += 1;
            ControlFlow::Continue(())
        },
    )
    .unwrap();
    assert!(chunks > 2);
    let measurement = Measurement::start();
    let output = text_copy(&heap, &text).unwrap();
    let counts = measurement.finish(&output);
    assert_eq!(output, expected);

    // The final leaf faults after earlier chunks have already been copied.
    let page = source
        .0
        .borrow()
        .values()
        .find(|page| {
            page.kind == PageKind::PAYLOAD_LEAF
                && page
                    .tokens
                    .windows("終".len())
                    .any(|bytes| bytes == "終".as_bytes())
        })
        .unwrap()
        .page
        .id;
    for fault in [
        HeapFault::Missing { page },
        HeapFault::Unauthorized { page },
        HeapFault::Corrupt {
            page,
            reason: "damaged leaf".into(),
        },
        HeapFault::LeaseLost,
        HeapFault::Transport("offline".into()),
        HeapFault::Exhausted {
            resource: HostResource::ReadBatch,
        },
    ] {
        let heap = PagedHeap::fresh(
            Box::new(FaultSource {
                source: source.clone(),
                page,
                fault: fault.clone(),
            }),
            EditEpoch(2),
        );
        assert_eq!(text_copy(&heap, &text).unwrap_err(), fault);
    }
    for input in ["", "é🦀終"] {
        let text = heap.text(input).unwrap();
        assert_eq!(text_copy(&heap, &text).unwrap(), input);
    }
    let mut oversized = heap.text("é").unwrap();
    oversized.summary.utf8_bytes = u64::MAX;
    assert!(matches!(
        text_copy(&heap, &oversized),
        Err(HeapFault::Malformed(_))
    ));
    assert_eq!(
        counts,
        (1, expected.len()),
        "TEXT-PROJECTION-ONE-BUFFER: output allocation calls and requested bytes"
    );
}
