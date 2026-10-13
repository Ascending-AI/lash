//! S4 root grammar laws.
use super::super::*;
use lash_kernel_doc::DocumentId;
use lash_kernel_doc::graph_tokens::{decode_body, encode_body};
use lash_kernel_doc::heap::*;
fn empty() -> KeyedRoot {
    KeyedRoot {
        page: None,
        len: 0,
        cost: LogicalBytes(0),
    }
}
fn seq() -> SequenceRoot {
    SequenceRoot {
        page: None,
        len: 0,
        cost: LogicalBytes(0),
    }
}
#[test]
fn a_checkpoint_root_fits_its_root_bound() {
    let root = CheckpointRoot {
        kernel: 1,
        document: DocumentId::from_bytes([1; 32]),
        release: PageId([2; 32]),
        owner: PageId([3; 32]),
        status: RootStatus::Parked,
        heap: PageRef {
            id: PageId([4; 32]),
            grant: PageGrant::NONE,
        },
        tasks: empty(),
        ready: seq(),
        waits: empty(),
        joins: empty(),
        withdrawn: empty(),
        unreported: seq(),
        reads: empty(),
        session: empty(),
        meters: SavedMeters {
            charged: 0,
            waits_issued: 0,
            next_join: 0,
            live_tasks: 0,
        },
    };
    let body = encode_body(&root);
    assert!(body.len() + 256 < 4096);
    assert_eq!(
        encode_body(&decode_body::<CheckpointRoot>(&body).expect("canonical root")),
        body
    );
}
#[test]
fn a_kernel_whole_root_is_refused() {
    // A whole root is a many-field struct, while the heap-header is a
    // one-field tagged record. It cannot be interpreted by the new reader.
    let (mut heap, pages) = super::heap::fresh();
    super::heap::seal(&mut heap);
    let header = heap.sealed_root().expect("header");
    pages
        .borrow_mut()
        .pages
        .get_mut(&header.id)
        .expect("page")
        .kind = PageKind(16);
    assert!(
        PagedHeap::open_header(header, Box::new(super::heap::Source(pages)), EditEpoch(2)).is_err()
    );
}
