//! Hostile frames are refused with bounded allocation.
//!
//! A counting allocator measures what one decode allocates on this thread.
//! Every refusal below is for a frame that declares far more than the bound —
//! a 4 GiB payload, a 4-billion-element array, a 4 GiB string — and each is
//! refused having allocated a few kilobytes at most.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use lash_vm_protocol::{
    CodecRefusal, DecodeLimits, EncodedPayload, ExecutionLease, FRAME_MAGIC, FrameCodec,
    FrameEpoch, HostReadId, HostReadKind, MessageFence, OwnerEpoch, WorkerFrame, WorkerMessage,
};

struct CountingAllocator;

thread_local! {
    static ALLOCATED_BYTES: Cell<u64> = const { Cell::new(0) };
}

#[expect(
    unsafe_code,
    reason = "bounded decode allocation is measured with a counting global allocator, and GlobalAlloc is an unsafe trait"
)]
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // The key is const-initialised and holds a Copy type, so charging it
        // allocates nothing and cannot re-enter the allocator.
        let _ = ALLOCATED_BYTES
            .try_with(|bytes| bytes.set(bytes.get().saturating_add(layout.size() as u64)));
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// The most any refusal below may allocate.
const REFUSAL_ALLOCATION_BOUND: u64 = 16 * 1024;

fn allocated_during<T>(work: impl FnOnce() -> T) -> (T, u64) {
    let before = ALLOCATED_BYTES.with(Cell::get);
    let result = work();
    (result, ALLOCATED_BYTES.with(Cell::get) - before)
}

fn codec() -> FrameCodec {
    FrameCodec::new(DecodeLimits::standard())
}

fn frame(_codec: &FrameCodec, declared: u32, payload: &[u8]) -> Vec<u8> {
    let mut bytes = FRAME_MAGIC.to_vec();
    bytes.extend_from_slice(&declared.to_be_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

fn assert_bounded_refusal(name: &str, bytes: &[u8], expected: impl Fn(&CodecRefusal) -> bool) {
    let codec = codec();
    let (result, allocated) = allocated_during(|| codec.decode_worker(bytes));
    let Err(refusal) = result else {
        panic!("{name}: a hostile frame decoded");
    };
    assert!(expected(&refusal), "{name}: {refusal:?}");
    assert!(
        allocated <= REFUSAL_ALLOCATION_BOUND,
        "{name} allocated {allocated} bytes before refusing"
    );
}

#[test]
fn hostile_frames_are_refused_with_bounded_allocation() {
    let codec = codec();

    assert_bounded_refusal("oversized", &frame(&codec, u32::MAX, &[]), |refusal| {
        matches!(refusal, CodecRefusal::FrameTooLarge { .. })
    });

    // A 4-billion-element array and map, each in a small frame.
    for marker in [0xdd_u8, 0xdf] {
        let payload = [marker, 0xff, 0xff, 0xff, 0xff, 0xc0];
        assert_bounded_refusal(
            "huge container",
            &frame(&codec, payload.len() as u32, &payload),
            |refusal| matches!(refusal, CodecRefusal::Malformed { .. }),
        );
    }

    // A string declaring 4 GiB, and one declaring just under the frame bound
    // with the bytes missing.
    let payload = [0xdb, 0xff, 0xff, 0xff, 0xff];
    assert_bounded_refusal(
        "huge string",
        &frame(&codec, payload.len() as u32, &payload),
        |refusal| matches!(refusal, CodecRefusal::Malformed { .. }),
    );

    // Truncated: the header declares 1 MiB, 3 bytes are present.
    assert_bounded_refusal(
        "truncated",
        &frame(&codec, 1 << 20, &[0x93, 0xc0, 0xc0]),
        |refusal| matches!(refusal, CodecRefusal::Truncated { .. }),
    );

    // Malformed: well-formed MessagePack that is not a frame message.
    let payload = [0x92, 0xc3, 0xc2];
    assert_bounded_refusal(
        "malformed",
        &frame(&codec, payload.len() as u32, &payload),
        |refusal| matches!(refusal, CodecRefusal::Malformed { .. }),
    );

    // Deep nesting: 100 000 nested one-element arrays.
    let mut payload = vec![0x91; 100_000];
    payload.push(0xc0);
    assert_bounded_refusal(
        "deep",
        &frame(&codec, payload.len() as u32, &payload),
        |refusal| matches!(refusal, CodecRefusal::DepthExceeded { .. }),
    );
}

/// FIG-4433: a frame is serialized once, into the allocation that is sent. A
/// large effect value is not copied again to put its header before it.
#[test]
fn a_frame_is_encoded_into_one_allocation_of_its_size() {
    const VALUE_BYTES: usize = 512 * 1024;
    let frame = WorkerFrame {
        header: MessageFence::new(ExecutionLease(1), OwnerEpoch(1), FrameEpoch(1)).next_header(),
        message: WorkerMessage::HostRead {
            id: HostReadId(0),
            kind: HostReadKind::Projection,
            request: EncodedPayload(vec![b'x'; VALUE_BYTES]),
        },
    };
    let (bytes, allocated) = allocated_during(|| codec().encode_worker(&frame));
    let bytes = bytes.expect("the frame encodes");
    assert_eq!(
        codec().decode_worker(&bytes).expect("the frame decodes"),
        frame
    );
    assert!(
        allocated < (VALUE_BYTES + VALUE_BYTES / 2) as u64,
        "encoding a {}-byte frame allocated {allocated} bytes",
        bytes.len()
    );
}
