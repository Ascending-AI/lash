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

/// Every field of every head kind but a tuple's (`paged/run/objects.rs`,
/// `Head`; a tuple's are [`TUPLE_FIELDS`]).
const HEAD_FIELDS: &[HeadField] = {
    use lash_kernel_state::paged::run::objects::Head;
    &[
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

/// One field of a tuple's head: its name, whether the member is a tuple
/// that is no key (one holding a list), and the forgery of that field
/// alone.
type TupleField = (
    &'static str,
    bool,
    fn(&mut lash_kernel_state::paged::run::objects::Head) -> bool,
);

/// Every field of a tuple's head a page can carry. A page writes a
/// tuple's root or, an inline tuple that is a key, its flat key, and
/// derives the other, so its identity is forged in the one it writes.
const TUPLE_FIELDS: &[TupleField] = {
    use lash_kernel_state::paged::run::objects::Head;
    &[
        (
            "its length forged to zero (the nested-length attack, review astra-l65c)",
            false,
            |head| match head {
                Head::Tuple { len, .. } => {
                    *len = 0;
                    true
                }
                _ => false,
            },
        ),
        ("its length forged one short", false, |head| match head {
            Head::Tuple { len, .. } => {
                *len -= 1;
                true
            }
            _ => false,
        }),
        ("its shape's depth forged", false, |head| match head {
            Head::Tuple { shape, .. } => {
                shape.depth += 1;
                true
            }
            _ => false,
        }),
        ("its shape's task edges forged", false, |head| match head {
            Head::Tuple { shape, .. } => {
                shape.task_edges += 1;
                true
            }
            _ => false,
        }),
        ("its shape's edge flag forged", false, |head| match head {
            Head::Tuple { shape, .. } => {
                shape.has_edges = !shape.has_edges;
                true
            }
            _ => false,
        }),
        ("its key's form forged: no key", false, |head| match head {
            Head::Tuple { normal, .. } if normal.is_some() => {
                *normal = None;
                true
            }
            _ => false,
        }),
        (
            "no key, its key's form forged: a key",
            true,
            |head| match head {
                Head::Tuple { normal, .. } if normal.is_none() => {
                    *normal = Some(lash_kernel_doc::heap::NormalKey::Digest([0; 32]));
                    true
                }
                _ => false,
            },
        ),
        (
            "its identity forged, its row kept",
            false,
            |head| match head {
                Head::Tuple {
                    normal: Some(normal @ lash_kernel_doc::heap::NormalKey::Inline(_)),
                    ..
                } => {
                    forged_key(normal);
                    true
                }
                Head::Tuple { root, .. } => {
                    root[0] ^= 1;
                    true
                }
                _ => false,
            },
        ),
    ]
};

/// A tuple of `len` members: integers, the last a list when it is no key.
fn tuple(heap: &mut SlotHeap, len: i64, unkeyable: bool) -> Value {
    let mut members: Vec<Value> = (0..len).map(Value::Int).collect();
    if unkeyable {
        members[0] = make(heap, NewObject::List(Vec::new()));
    }
    make(heap, NewObject::Tuple(members))
}

/// A record's field name.
fn field(i: i64) -> Value {
    Value::Text(lash_kernel_doc::arena::ShortText::new(&format!("k{i}")).expect("short"))
}

/// What holds a member's head: the holder of the member, chunked (past
/// [`LARGE`](lash_kernel_state::paged::run::chunked::LARGE) members) or
/// inline, and the read that reaches the member in it.
struct Holder {
    name: &'static str,
    chunked: bool,
    build: fn(&mut SlotHeap, Value) -> Value,
    reach: fn(
        &SlotHeap,
        lash_kernel_doc::arena::ArenaHandle,
    ) -> Result<Option<Value>, lash_kernel_doc::arena::native::NativeError>,
}

/// The first of a list or a tuple.
fn first(
    heap: &SlotHeap,
    holder: lash_kernel_doc::arena::ArenaHandle,
) -> Result<Option<Value>, lash_kernel_doc::arena::native::NativeError> {
    heap.arena().item(holder, 0)
}

/// Every holder of an immutable's head: each body kind that holds
/// members, chunked and inline, and a closure through its cell.
const HOLDERS: &[Holder] = &[
    Holder {
        name: "a tuple's chunk",
        chunked: true,
        build: |heap, member| make(heap, NewObject::Tuple(vec![member; 513])),
        reach: first,
    },
    Holder {
        name: "a tuple's row",
        chunked: false,
        build: |heap, member| make(heap, NewObject::Tuple(vec![member; 2])),
        reach: first,
    },
    Holder {
        name: "a list's chunk",
        chunked: true,
        build: |heap, member| make(heap, NewObject::List(vec![member; 513])),
        reach: first,
    },
    Holder {
        name: "a list's row",
        chunked: false,
        build: |heap, member| make(heap, NewObject::List(vec![member; 2])),
        reach: first,
    },
    Holder {
        name: "a map's chunk",
        chunked: true,
        build: |heap, member| {
            make(
                heap,
                NewObject::Map((0..513).map(|i| (Value::Int(i), member)).collect()),
            )
        },
        reach: |heap, holder| heap.arena().table_get(holder, &Value::Int(0)),
    },
    Holder {
        name: "a map's row",
        chunked: false,
        build: |heap, member| make(heap, NewObject::Map(vec![(Value::Int(0), member)])),
        reach: |heap, holder| heap.arena().table_get(holder, &Value::Int(0)),
    },
    Holder {
        name: "a record's chunk",
        chunked: true,
        build: |heap, member| {
            make(
                heap,
                NewObject::Record((0..513).map(|i| (field(i), member)).collect()),
            )
        },
        reach: |heap, holder| heap.arena().table_get(holder, &field(0)),
    },
    Holder {
        name: "a record's row",
        chunked: false,
        build: |heap, member| make(heap, NewObject::Record(vec![(field(0), member)])),
        reach: |heap, holder| heap.arena().table_get(holder, &field(0)),
    },
    Holder {
        name: "a closure's cell",
        chunked: false,
        build: |heap, member| {
            use lash_kernel_value::names::{CallableId, FunctionId};
            use lash_kernel_value::sites::{Site, Unit};
            let link = lash_kernel_value::content_ids::LinkId::from_bytes([4; 32]);
            let identity = std::sync::Arc::new(lash_kernel_doc::arena::ClosureIdentity {
                capture_names: vec![lash_kernel_value::Name::new("held".to_owned())],
                callable: CallableId {
                    link,
                    function: FunctionId::from_bytes([5; 32]),
                },
                site: lash_kernel_doc::linked_site::LinkedSite {
                    link,
                    site: Site::new(Unit::Main, [1]),
                },
            });
            let cell = heap.cell(member);
            Value::Closure(heap.closure(identity, vec![cell]))
        },
        reach: |heap, holder| {
            let (_, cells, _) = heap.arena().closure(holder)?;
            let cell =
                *cells
                    .first()
                    .ok_or(lash_kernel_doc::arena::native::NativeError::Malformed(
                        "a closure captures no cell",
                    ))?;
            heap.arena().cell(cell).map(Some)
        },
    },
];

/// The member `holder` holds in the cold `held`, reached, its first member
/// read and its key taken, each cold page loaded as the reads ask: the
/// member, or the first load's error.
fn reached(
    heap: &mut SlotHeap,
    holder: &Holder,
    held: Value,
) -> Result<Result<Value, crate::heap::LoadError>, String> {
    use lash_kernel_doc::arena::native::{NativeError, Retry};
    let handle = held.handle().ok_or("the holder is no object")?;
    for _ in 0..64 {
        let asked = (holder.reach)(heap, handle).and_then(|member| {
            let member = member.ok_or(NativeError::Malformed("the holder holds no member"))?;
            let inner = member
                .handle()
                .ok_or(NativeError::Malformed("the member is no object"))?;
            heap.arena().item(inner, 0)?;
            heap.arena().semantics().normal_key(&member)?;
            Ok(member)
        });
        match asked {
            Ok(member) => return Ok(Ok(member)),
            Err(NativeError::Retry(Retry::Cold { object, chunk })) => {
                if let Err(error) = heap.load(object, chunk, &mut |visit| {
                    [held].iter().for_each(&mut *visit)
                }) {
                    return Ok(Err(error));
                }
            }
            Err(other) => return Err(format!("a read failed: {other:?}")),
        }
    }
    Err("the reads asked for more than 64 loads".into())
}

/// Lead ruling, l65f item 2: every field of a tuple's head, forged alone
/// where its holder holds it, for a chunked tuple and an inline one,
/// under every holder ([`HOLDERS`]), the tuple's row and every other page
/// kept, fails the first read that reaches the tuple with
/// `Store(Corrupt)`: a holder's head is checked against the row it names,
/// whatever holds it.
#[test]
fn every_tuple_head_field_forged_alone_fails_the_first_read_under_every_holder() {
    use crate::heap::LoadError;

    let mut failed = Vec::new();
    for (field, unkeyable, forge) in TUPLE_FIELDS {
        for (size, len) in [("chunked", 513), ("inline", 10)] {
            for holder in HOLDERS {
                let case = format!("{size} tuple, {field}, in {}", holder.name);
                let store = Counting::default();
                let mut heap = fresh(&store);
                let member = tuple(&mut heap, len, *unkeyable);
                let held = (holder.build)(&mut heap, member);
                let substitute = if holder.chunked {
                    Substitute::NestedHead(*forge)
                } else {
                    Substitute::InlineHead(*forge)
                };
                let root = match nested(&store, &mut heap, &[held, member, member], substitute) {
                    Ok(root) => root,
                    Err(error) => {
                        failed.push(format!("{case}: {error}"));
                        continue;
                    }
                };
                let (mut heap, cold) = reopened(&store, &root);
                match reached(&mut heap, holder, cold) {
                    Ok(Err(LoadError::Store(lash_kernel_doc::heap::HeapFault::Corrupt {
                        ..
                    }))) => {}
                    Ok(Err(other)) => {
                        failed.push(format!(
                            "{case}: the first read failed otherwise: {other:?}"
                        ));
                    }
                    Ok(Ok(member)) => {
                        let head = member
                            .handle()
                            .and_then(|handle| heap.arena().facts(handle).ok())
                            .and_then(|facts| facts.head().cloned());
                        let key = heap.arena().semantics().normal_key(&member);
                        failed.push(format!(
                            "{case}: the read answered, the tuple's head {head:?}, its key {key:?}"
                        ));
                    }
                    Err(error) => failed.push(format!("{case}: {error}")),
                }
            }
        }
    }
    assert!(failed.is_empty(), "{failed:#?}");
}
