//! Every field of every head kind, forged alone in a chunk and inline
//! (review astra-l65d item 2).

use super::*;

/// The root tuple of a [`Substitute::InlineHead`] case: two of the
/// immutable `member`, so its row is inline and holds the member's head.
fn inline_of(heap: &mut SlotHeap, member: Value) -> [Value; 3] {
    let outer = make(heap, NewObject::Tuple(vec![member; 2]));
    [outer, member, member]
}

/// `key` changed in one byte.
fn forged_key(key: &mut lash_kernel_doc::heap::NormalKey) {
    match key {
        lash_kernel_doc::heap::NormalKey::Inline(tokens) => match tokens.last_mut() {
            Some(last) => *last ^= 1,
            None => tokens.push(0),
        },
        lash_kernel_doc::heap::NormalKey::Digest(digest) => digest[0] ^= 1,
    }
}

/// A function value naming one callable.
fn function(heap: &mut SlotHeap) -> Value {
    make(
        heap,
        NewObject::Function(lash_kernel_value::CallableId {
            link: lash_kernel_value::LinkId::from_bytes([4; 32]),
            function: lash_kernel_value::FunctionId::from_bytes([5; 32]),
        }),
    )
}

/// One field of one head kind: the member that has the head, the read
/// that consumes it, and the forgery of that field alone (`false` when
/// the head is of another kind).
type HeadField = (
    &'static str,
    Build,
    Read,
    fn(&mut lash_kernel_state::paged::run::objects::Head) -> bool,
);

/// Every field of every head kind (`paged/run/objects.rs`, `Head`) a
/// page can carry: a tuple's key is not one, its page writing the root
/// alone, from which the key is derived.
const HEAD_FIELDS: &[HeadField] = {
    use lash_kernel_state::paged::run::objects::Head;
    &[
        (
            "a tuple, its length forged to zero (the nested-length attack, review astra-l65c)",
            |heap| filled(heap, 10),
            Read::Nested,
            |head| match head {
                Head::Tuple { len, .. } => {
                    *len = 0;
                    true
                }
                _ => false,
            },
        ),
        (
            "a tuple, its length forged one short",
            |heap| filled(heap, 10),
            Read::Nested,
            |head| match head {
                Head::Tuple { len, .. } => {
                    *len -= 1;
                    true
                }
                _ => false,
            },
        ),
        (
            "a tuple, its shape's depth forged",
            |heap| filled(heap, 10),
            Read::Nested,
            |head| match head {
                Head::Tuple { shape, .. } => {
                    shape.depth += 1;
                    true
                }
                _ => false,
            },
        ),
        (
            "a tuple, its shape's task edges forged",
            |heap| filled(heap, 10),
            Read::Nested,
            |head| match head {
                Head::Tuple { shape, .. } => {
                    shape.task_edges += 1;
                    true
                }
                _ => false,
            },
        ),
        (
            "a tuple, its shape's edge flag forged",
            |heap| filled(heap, 10),
            Read::Nested,
            |head| match head {
                Head::Tuple { shape, .. } => {
                    shape.has_edges = !shape.has_edges;
                    true
                }
                _ => false,
            },
        ),
        (
            "a tuple, its key's form forged: no key",
            |heap| filled(heap, 10),
            Read::Nested,
            |head| match head {
                Head::Tuple { normal, .. } if normal.is_some() => {
                    *normal = None;
                    true
                }
                _ => false,
            },
        ),
        (
            "a tuple, its root forged, its row kept",
            |heap| filled(heap, 10),
            Read::Nested,
            |head| match head {
                Head::Tuple { root, .. } => {
                    root[0] ^= 1;
                    true
                }
                _ => false,
            },
        ),
        (
            "an error, its shape's depth forged",
            |heap| {
                heap.error("kind", "message", Value::Null)
                    .expect("an error")
            },
            Read::Item,
            |head| match head {
                Head::Error { shape, .. } => {
                    shape.depth += 1;
                    true
                }
                _ => false,
            },
        ),
        (
            "an error, its shape's task edges forged",
            |heap| {
                heap.error("kind", "message", Value::Null)
                    .expect("an error")
            },
            Read::Item,
            |head| match head {
                Head::Error { shape, .. } => {
                    shape.task_edges += 1;
                    true
                }
                _ => false,
            },
        ),
        (
            "an error, its shape's edge flag forged",
            |heap| {
                heap.error("kind", "message", Value::Null)
                    .expect("an error")
            },
            Read::Item,
            |head| match head {
                Head::Error { shape, .. } => {
                    shape.has_edges = !shape.has_edges;
                    true
                }
                _ => false,
            },
        ),
        (
            "an error, its content forged",
            |heap| {
                heap.error("kind", "message", Value::Null)
                    .expect("an error")
            },
            Read::Item,
            |head| match head {
                Head::Error { content, .. } => {
                    content[0] ^= 1;
                    true
                }
                _ => false,
            },
        ),
        (
            "a big integer, its bit count forged",
            |heap| make(heap, NewObject::BigInt(big(20_000))),
            Read::Item,
            |head| match head {
                Head::BigInt { summary, .. } => {
                    summary.bits += 1;
                    true
                }
                _ => false,
            },
        ),
        (
            "a big integer, its sign forged",
            |heap| make(heap, NewObject::BigInt(big(20_000))),
            Read::Item,
            |head| match head {
                Head::BigInt { summary, .. } => {
                    summary.negative = !summary.negative;
                    true
                }
                _ => false,
            },
        ),
        (
            "a big integer, its key forged",
            |heap| make(heap, NewObject::BigInt(big(20_000))),
            Read::Item,
            |head| match head {
                Head::BigInt { normal, .. } => {
                    forged_key(normal);
                    true
                }
                _ => false,
            },
        ),
        (
            "a timestamp, its bit count forged",
            |heap| make(heap, NewObject::Timestamp(big(20_000))),
            Read::Item,
            |head| match head {
                Head::Timestamp { summary, .. } => {
                    summary.bits -= 1;
                    true
                }
                _ => false,
            },
        ),
        (
            "a timestamp, its sign forged",
            |heap| make(heap, NewObject::Timestamp(big(20_000))),
            Read::Item,
            |head| match head {
                Head::Timestamp { summary, .. } => {
                    summary.negative = !summary.negative;
                    true
                }
                _ => false,
            },
        ),
        (
            "a timestamp, its key forged",
            |heap| make(heap, NewObject::Timestamp(big(20_000))),
            Read::Item,
            |head| match head {
                Head::Timestamp { normal, .. } => {
                    forged_key(normal);
                    true
                }
                _ => false,
            },
        ),
        (
            "a text, its byte count forged",
            |heap| text(heap, 'a'),
            Read::Item,
            |head| match head {
                Head::LongText { summary, .. } => {
                    summary.utf8_bytes += 1;
                    true
                }
                _ => false,
            },
        ),
        (
            "a text, its character count forged",
            |heap| text(heap, 'a'),
            Read::Item,
            |head| match head {
                Head::LongText { summary, .. } => {
                    summary.chars -= 1;
                    true
                }
                _ => false,
            },
        ),
        (
            "a text, its UTF-16 count forged",
            |heap| text(heap, 'a'),
            Read::Item,
            |head| match head {
                Head::LongText { summary, .. } => {
                    summary.utf16_units += 1;
                    true
                }
                _ => false,
            },
        ),
        (
            "a text, its key forged",
            |heap| text(heap, 'a'),
            Read::Item,
            |head| match head {
                Head::LongText { normal, .. } => {
                    forged_key(normal);
                    true
                }
                _ => false,
            },
        ),
        (
            "bytes, their length forged",
            |heap| make(heap, NewObject::Bytes(vec![7; 9_000].into())),
            Read::Item,
            |head| match head {
                Head::Bytes { len, .. } => {
                    *len -= 1;
                    true
                }
                _ => false,
            },
        ),
        (
            "bytes, their key forged",
            |heap| make(heap, NewObject::Bytes(vec![7; 9_000].into())),
            Read::Item,
            |head| match head {
                Head::Bytes { normal, .. } => {
                    forged_key(normal);
                    true
                }
                _ => false,
            },
        ),
        (
            "a handle, its kind length forged",
            |heap| host_handle(heap, "h".repeat(9_000)),
            Read::Item,
            |head| match head {
                Head::Handle { kind_len, .. } => {
                    *kind_len += 1;
                    true
                }
                _ => false,
            },
        ),
        (
            "a handle, its id length forged",
            |heap| host_handle(heap, "h".repeat(9_000)),
            Read::Item,
            |head| match head {
                Head::Handle { id_len, .. } => {
                    *id_len -= 1;
                    true
                }
                _ => false,
            },
        ),
        (
            "a handle, its content forged",
            |heap| host_handle(heap, "h".repeat(9_000)),
            Read::Item,
            |head| match head {
                Head::Handle { content, .. } => {
                    forged_key(content);
                    true
                }
                _ => false,
            },
        ),
        (
            "a function, its link forged",
            function,
            Read::Item,
            |head| match head {
                Head::Function(callable) => {
                    callable.link = lash_kernel_value::LinkId::from_bytes([6; 32]);
                    true
                }
                _ => false,
            },
        ),
        (
            "a function, its function forged",
            function,
            Read::Item,
            |head| match head {
                Head::Function(callable) => {
                    callable.function = lash_kernel_value::FunctionId::from_bytes([6; 32]);
                    true
                }
                _ => false,
            },
        ),
    ]
};

/// Review astra-l65d item 2, executed (l65e item 8): every field of every
/// head kind, forged alone in the member's head that the root tuple holds,
/// in its first chunk and in its inline row, the member's key, root and
/// row and every other page kept, fails the first read that consumes it
/// with `Store(Corrupt)`.
#[test]
fn every_head_field_forged_alone_fails_the_first_read_in_a_chunk_and_inline() {
    use crate::heap::LoadError;

    let mut failed = Vec::new();
    for (field, member, read, forge) in HEAD_FIELDS {
        for (placement, of, substitute) in [
            (
                "in a chunk",
                nested_of as fn(&mut SlotHeap, Value) -> [Value; 3],
                Substitute::NestedHead(*forge),
            ),
            ("inline", inline_of, Substitute::InlineHead(*forge)),
        ] {
            let case = format!("{field}, {placement}");
            let store = Counting::default();
            let mut heap = fresh(&store);
            let built = member(&mut heap);
            let roots = of(&mut heap, built);
            match nested(&store, &mut heap, &roots, substitute) {
                Ok(root) => {
                    let (mut heap, cold) = reopened(&store, &root);
                    let built = member(&mut heap);
                    let again = of(&mut heap, built)[0];
                    match first_read(&mut heap, &case, *read, [cold, again]) {
                        Ok(Some(LoadError::Store(lash_kernel_doc::heap::HeapFault::Corrupt {
                            ..
                        }))) => {}
                        Err(error) => failed.push(error),
                        other => failed.push(format!("{case}: the first read answered {other:?}")),
                    }
                }
                Err(error) => failed.push(format!("{case}: {error}")),
            }
        }
    }
    assert!(failed.is_empty(), "{failed:#?}");
}
