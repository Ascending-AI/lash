//! Mutate physical bodies and framing, then recompute the actual digest. Hash
//! authentication and inner grammar are separate oracles; neither is bypassed.
#![no_main]

mod support;

use lash_checkpoint_encoder::node::{frame_semantic, write_envelope};
use lash_checkpoint_encoder::{DecodeLimits, GraphRefusal, NodeId, NodeKind, check_envelope};
use lash_kernel_doc::heap::HeapFault;
use lash_kernel_state::paged::{Checkpoint, CheckpointRoot, PageAnswer, PageKind};
use lash_kernel_value::tokens::encode_body;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() < 5 || data.len() > 1024 {
        return;
    }
    let kind = NodeKind(PageKind::CHECKPOINT_ROOT.0);
    let logical = encode_body(&support::root(data[0] & 1 != 0));
    let original = frame_semantic(kind, &logical, 4096)
        .unwrap()
        .named()
        .into_node();
    let envelope = original.envelope().unwrap();
    let mut children: Vec<_> = envelope.children.iter().map(|id| id.0).collect();
    let mut body = envelope.body.to_vec();
    let offset = usize::from(u16::from_le_bytes([data[2], data[3]]));
    let mode = data[1] % 8;
    match mode {
        0 | 7 => {} // Valid control / independently test root-grant admission.
        1 => body.truncate(offset % body.len()),
        2 => {
            let at = offset % (body.len() + 1);
            body.splice(at..at, data[4..].iter().copied());
        }
        3 => {
            let at = offset % body.len();
            body[at] ^= data[4];
        }
        4 => body = data[4..].to_vec(),
        5 => {
            if data[4] & 1 == 0 {
                children.push([255; 32]); // Sorted but unreferenced child.
            } else {
                children.remove(offset % children.len()); // Out-of-range slot.
            }
        }
        6 => {} // Framing mutation below.
        _ => unreachable!(),
    }
    let mut bytes = write_envelope(kind, &children, &body);
    if mode == 6 {
        let at = offset % bytes.len();
        bytes[at] ^= data[4];
        if data[4] & 1 != 0 {
            bytes.truncate(at);
        }
    }
    let id = NodeId::of_envelope(&bytes);
    let limits = DecodeLimits::within_total(8192);
    // A wrong identity must fail before grammar admission, even when this
    // envelope's recomputed identity reaches an inner-body refusal below.
    let mut wrong = id;
    wrong.0[0] ^= 1;
    assert!(matches!(
        check_envelope(&bytes, wrong, &limits),
        Err(GraphRefusal::HashMismatch(_))
    ));
    let admitted = check_envelope(&bytes, id, &limits);
    if mode == 5 {
        assert!(matches!(admitted, Err(GraphRefusal::NonCanonical { .. })));
        return;
    }
    if mode == 0 || mode == 7 {
        assert!(admitted.is_ok(), "refused an unchanged valid envelope");
    }
    let Ok(checked) = admitted else { return };
    let tokens = checked
        .logical_tokens(0, id)
        .expect("checked physical slots expand");
    let answer = PageAnswer {
        page: lash_kernel_doc::heap::PageRef {
            id: lash_kernel_doc::heap::PageId(id.0),
            grant: lash_kernel_doc::heap::PageGrant::NONE,
        },
        kind: PageKind(checked.kind.0),
        tokens,
        children: checked
            .children
            .iter()
            .map(|id| lash_kernel_doc::heap::PageRef {
                id: lash_kernel_doc::heap::PageId(id.0),
                grant: lash_kernel_doc::heap::PageGrant([id.0[0]; 48]),
            })
            .collect(),
    };
    let root = CheckpointRoot::of_answer(&answer);
    let checkpoint = Checkpoint::of_answer(&answer);
    assert_eq!(root.is_ok(), checkpoint.is_ok());
    if mode == 1 {
        assert!(
            matches!(root, Err(HeapFault::Corrupt { .. })),
            "accepted a truncated root"
        );
    }
    if mode == 0 || mode == 7 {
        assert!(root.is_ok(), "refused an unchanged valid root");
    }
    if let Ok(root) = root {
        assert_eq!(encode_body(&root), answer.tokens.as_ref());
        assert_eq!(
            root.heap.grant,
            answer
                .children
                .iter()
                .find(|p| p.id == root.heap.id)
                .unwrap()
                .grant
        );
        if mode == 7 {
            // The authenticated adapter's declared-child grants are another
            // boundary: removing a required grant must produce HeapFault.
            let mut missing = answer.clone();
            let required: Vec<_> = answer
                .children
                .iter()
                .filter(|p| p.id != root.closure && p.id != root.owner)
                .collect();
            let child = required[offset % required.len()];
            missing.children.retain(|p| p.id != child.id);
            assert!(matches!(
                CheckpointRoot::of_answer(&missing),
                Err(HeapFault::Corrupt { .. })
            ));
            assert!(matches!(
                Checkpoint::of_answer(&missing),
                Err(HeapFault::Corrupt { .. })
            ));
        }
    }
});
