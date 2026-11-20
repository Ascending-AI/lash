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

/// Fields of scalar heads (`paged/run/objects.rs`, `Head`).
const HEAD_FIELDS: &[HeadField] = {
    use lash_kernel_state::paged::run::objects::Head;
    &[
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

/// Review astra-l65d item 2, executed (l65e item 8): every scalar head
/// field, forged alone in the member's head that the root tuple holds,
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
/// that is no key (one holding a list), the sizes of member whose head
/// the page codec cannot hold forged so ([`DECODE_REFUSES`]), and the
/// forgery of that field alone.
type TupleField = (
    &'static str,
    bool,
    &'static [&'static str],
    fn(&mut lash_kernel_state::paged::run::objects::Head) -> bool,
);

/// Every field of a tuple's head a page can carry. A page writes a
/// tuple's root or, an inline tuple that is a key, its flat key, and
/// derives the other, so its identity is forged in the one it writes.
/// An inline tuple's flat key spells its length, and a head's task edges
/// must agree with its edge flag and depth, so the codec refuses those
/// forgeries before any store holds them.
const TUPLE_FIELDS: &[TupleField] = {
    use lash_kernel_state::paged::run::objects::Head;
    &[
        (
            "its length forged to zero (the nested-length attack, review astra-l65c)",
            false,
            &["inline"],
            |head| match head {
                Head::Tuple { len, .. } => {
                    *len = 0;
                    true
                }
                _ => false,
            },
        ),
        (
            "its length forged one short",
            false,
            &["inline"],
            |head| match head {
                Head::Tuple { len, .. } => {
                    *len -= 1;
                    true
                }
                _ => false,
            },
        ),
        ("its shape's depth forged", false, &[], |head| match head {
            Head::Tuple { shape, .. } => {
                shape.depth += 1;
                true
            }
            _ => false,
        }),
        (
            "its shape's task edges forged",
            false,
            &["chunked", "inline"],
            |head| match head {
                Head::Tuple { shape, .. } => {
                    shape.task_edges += 1;
                    true
                }
                _ => false,
            },
        ),
        (
            "its shape's edge flag forged",
            false,
            &[],
            |head| match head {
                Head::Tuple { shape, .. } => {
                    shape.has_edges = !shape.has_edges;
                    true
                }
                _ => false,
            },
        ),
        (
            "its key's form forged: no key",
            false,
            &[],
            |head| match head {
                Head::Tuple { normal, .. } if normal.is_some() => {
                    *normal = None;
                    true
                }
                _ => false,
            },
        ),
        (
            "no key, its key's form forged: a key",
            true,
            &[],
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
            &[],
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

/// Every field of an error head, plus the depth-zero encoding refusal.
const ERROR_FIELDS: &[(&str, bool, Forge)] = {
    use lash_kernel_state::paged::run::objects::Head;
    &[
        (
            "its shape's depth forged to zero",
            true,
            |head| match head {
                Head::Error { shape, .. } => {
                    shape.depth = 0;
                    true
                }
                _ => false,
            },
        ),
        ("its shape's depth forged", false, |head| match head {
            Head::Error { shape, .. } => {
                shape.depth += 1;
                true
            }
            _ => false,
        }),
        ("its shape's task edges forged", false, |head| match head {
            Head::Error { shape, .. } => {
                shape.task_edges += 1;
                true
            }
            _ => false,
        }),
        ("its shape's edge flag forged", false, |head| match head {
            Head::Error { shape, .. } => {
                shape.has_edges = !shape.has_edges;
                true
            }
            _ => false,
        }),
        ("its content forged", false, |head| match head {
            Head::Error { content, .. } => {
                content[0] ^= 1;
                true
            }
            _ => false,
        }),
    ]
};

/// One use of a tuple a run makes, first after its holder is reached: an
/// item read, or one of its head's facts alone (its key, its depth, its
/// length), which a check on item reads alone would not cover.
type Access = (
    &'static str,
    fn(&SlotHeap, Value) -> Result<(), lash_kernel_doc::arena::native::NativeError>,
);

const ACCESSES: &[Access] = &[
    ("an item read and its key", |heap, member| {
        let inner =
            member
                .handle()
                .ok_or(lash_kernel_doc::arena::native::NativeError::Malformed(
                    "the member is no object",
                ))?;
        heap.arena().item(inner, 0)?;
        heap.arena().semantics().normal_key(&member)?;
        Ok(())
    }),
    ("its key alone", |heap, member| {
        heap.arena().semantics().normal_key(&member).map(drop)
    }),
    ("its depth alone", |heap, member| {
        heap.arena().summary_of(&member).map(drop)
    }),
    ("its length alone", |heap, member| {
        let inner =
            member
                .handle()
                .ok_or(lash_kernel_doc::arena::native::NativeError::Malformed(
                    "the member is no object",
                ))?;
        heap.arena().len(inner).map(drop)
    }),
];

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

type Forge = fn(&mut lash_kernel_state::paged::run::objects::Head) -> bool;

/// A holder of a head, the placement of its copy, and the read that
/// reaches the member in it.
struct Holder {
    name: &'static str,
    substitute: fn(Forge) -> Substitute,
    /// It holds the member as a key, so only a tuple that is a key.
    key: bool,
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
        substitute: Substitute::NestedHead,
        key: false,
        build: |heap, member| make(heap, NewObject::Tuple(vec![member; 513])),
        reach: first,
    },
    Holder {
        name: "a tuple's row",
        substitute: Substitute::InlineHead,
        key: false,
        build: |heap, member| make(heap, NewObject::Tuple(vec![member; 2])),
        reach: first,
    },
    Holder {
        name: "a list's chunk",
        substitute: Substitute::NestedHead,
        key: false,
        build: |heap, member| make(heap, NewObject::List(vec![member; 513])),
        reach: first,
    },
    Holder {
        name: "a list's row",
        substitute: Substitute::InlineHead,
        key: false,
        build: |heap, member| make(heap, NewObject::List(vec![member; 2])),
        reach: first,
    },
    Holder {
        name: "a map's chunk",
        substitute: Substitute::NestedHead,
        key: false,
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
        substitute: Substitute::InlineHead,
        key: false,
        build: |heap, member| make(heap, NewObject::Map(vec![(Value::Int(0), member)])),
        reach: |heap, holder| heap.arena().table_get(holder, &Value::Int(0)),
    },
    Holder {
        name: "a record's chunk",
        substitute: Substitute::NestedHead,
        key: false,
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
        substitute: Substitute::InlineHead,
        key: false,
        build: |heap, member| make(heap, NewObject::Record(vec![(field(0), member)])),
        reach: |heap, holder| heap.arena().table_get(holder, &field(0)),
    },
    Holder {
        name: "a closure's cell",
        substitute: Substitute::InlineHead,
        key: false,
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
    Holder {
        name: "a set's chunk, as a key",
        substitute: Substitute::NestedHead,
        key: true,
        build: |heap, member| {
            let mut keys = vec![member];
            keys.extend((1..513).map(Value::Int));
            make(heap, NewObject::Set(keys))
        },
        reach: first_key,
    },
    Holder {
        name: "a set's row, as a key",
        substitute: Substitute::InlineHead,
        key: true,
        build: |heap, member| make(heap, NewObject::Set(vec![member])),
        reach: first_key,
    },
];

const ROOT_HOLDER: Holder = Holder {
    name: "the heap root",
    substitute: Substitute::RootHead,
    key: false,
    build: |_, member| member,
    reach: |_, handle| Ok(Some(Value::Error(handle))),
};

/// The first key of a set.
fn first_key(
    heap: &SlotHeap,
    holder: lash_kernel_doc::arena::ArenaHandle,
) -> Result<Option<Value>, lash_kernel_doc::arena::native::NativeError> {
    Ok(heap.arena().table_next(holder, None)?.map(|(_, key)| key))
}

/// The member `holder` holds in the cold `held`, reached and used as
/// `access` says, each cold page loaded as the reads ask: the member, or
/// the first load's error.
fn reached(
    heap: &mut SlotHeap,
    holder: &Holder,
    access: &Access,
    held: Value,
) -> Result<Result<Value, crate::heap::LoadError>, String> {
    use lash_kernel_doc::arena::native::{NativeError, Retry};
    let handle = held.handle().ok_or("the holder is no object")?;
    for _ in 0..64 {
        let asked = (holder.reach)(heap, handle).and_then(|member| {
            let member = member.ok_or(NativeError::Malformed("the holder holds no member"))?;
            (access.1)(heap, member)?;
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

/// Lead ruling, l65f item 2 (l65g): every field of a tuple's head, forged
/// alone where its holder holds it, for a chunked tuple and an inline one,
/// under every holder ([`HOLDERS`]), the tuple's row and every other page
/// kept, fails the first use of the tuple ([`ACCESSES`]: an item read, or
/// its key, depth or length alone) with `Store(Corrupt)`: every holder's
/// copy of a composite's head is checked whole against its row before
/// a fact of it is used. A forgery the page codec refuses
/// ([`TUPLE_FIELDS`]) never reaches a store, and must be refused so; a
/// tuple that is no key is never held as one. E1 extends this rule to
/// an error in each value holder and in the heap root. Errors have no
/// key or length. The VM must check the row before accepting or refusing
/// an enclosing tuple by depth.
#[test]
fn every_tuple_head_field_forged_alone_fails_the_first_read_under_every_holder() {
    use crate::heap::LoadError;

    let mut failed = Vec::new();
    let mut refused = 0;
    for (field, unkeyable, refusing, forge) in TUPLE_FIELDS {
        for (size, len) in [("chunked", 513), ("inline", 10)] {
            for holder in HOLDERS {
                if holder.key && *unkeyable {
                    continue;
                }
                let case = format!("{size} tuple, {field}, in {}", holder.name);
                let store = Counting::default();
                let mut heap = fresh(&store);
                let member = tuple(&mut heap, len, *unkeyable);
                let held = (holder.build)(&mut heap, member);
                let substitute = (holder.substitute)(*forge);
                let sealed = nested(&store, &mut heap, &[held, member, member], substitute);
                let root = match (sealed, refusing.contains(&size)) {
                    (Err(error), true) if error.starts_with(DECODE_REFUSES) => {
                        refused += 1;
                        continue;
                    }
                    (Ok(_), true) => {
                        failed.push(format!("{case}: the page codec holds the forgery"));
                        continue;
                    }
                    (Ok(root), false) => root,
                    (Err(error), _) => {
                        failed.push(format!("{case}: {error}"));
                        continue;
                    }
                };
                for access in ACCESSES {
                    let case = format!("{case}, {}", access.0);
                    let (mut heap, cold) = reopened(&store, &root);
                    match reached(&mut heap, holder, access, cold) {
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
                            failed.push(format!(
                                "{case}: the read answered, the tuple's head {head:?}"
                            ));
                        }
                        Err(error) => failed.push(format!("{case}: {error}")),
                    }
                }
            }
        }
    }
    // Inline length one short and zero, and task edges in both sizes,
    // under each of the eleven holders.
    if refused != 2 * HOLDERS.len() + 2 * HOLDERS.len() {
        failed.push(format!("the page codec refused {refused} forgeries"));
    }
    check_error_heads(&mut failed);
    assert!(failed.is_empty(), "{failed:#?}");
}

/// A use of a scalar that a head alone could answer, and its answer.
type ScalarUse = (
    &'static str,
    fn(&SlotHeap, Value) -> Result<String, lash_kernel_doc::arena::native::NativeError>,
);

/// What a scalar's head alone could answer: its key, the size a charge
/// reads (a text's or bytes' length, an integer's magnitude), and the
/// counts a native reads of a text or bytes.
const SCALAR_USES: &[ScalarUse] = &[
    ("its key", |heap, member| {
        Ok(format!(
            "{:?}",
            heap.arena().semantics().normal_key(&member)?
        ))
    }),
    ("its size", |heap, member| {
        Ok(crate::data::size(heap.arena(), &member)?.to_string())
    }),
    ("its counts", |heap, member| {
        let semantics = heap.arena().semantics();
        let args =
            lash_kernel_doc::arena::native::Args::new(&[], heap.arena().objects(), &semantics);
        Ok(match member {
            Value::LongText(_) => format!("{:?}", args.text(&member)?.map(|view| view.summary())),
            Value::Bytes(_) => format!("{:?}", args.bytes(&member)?.map(|view| view.len())),
            _ => String::new(),
        })
    }),
    ("its magnitude", |heap, member| {
        Ok(match member {
            Value::BigInt(handle) | Value::BigTimestamp(handle) => {
                crate::session_editor::preview::bits(heap.arena(), handle)?.to_string()
            }
            _ => String::new(),
        })
    }),
];

/// FIG-7735 K01: every field of a big integer's, timestamp's, text's or
/// bytes' head, forged alone where its holder holds it (every holder in
/// [`HOLDERS`], and the heap root), the scalar's own row and every other
/// page kept, never answers a use of the scalar (its key, size, counts
/// or magnitude): the use fails with `Store(Corrupt)` or gives the
/// owner's answer, the same use of the scalar as built. The owner's row is the oracle, read and
/// checked whole before any fact of the head is used. A function's and
/// a host handle's heads are whole values, not claims, and stay with
/// [`every_head_field_forged_alone_fails_the_first_read_in_a_chunk_and_inline`].
#[test]
fn every_scalar_head_field_forged_alone_never_answers_a_use_under_every_holder() {
    use crate::heap::LoadError;
    use lash_kernel_state::paged::run::objects::Head;

    let mut failed = Vec::new();
    for (field, member, _, forge) in HEAD_FIELDS {
        let store = Counting::default();
        let mut heap = fresh(&store);
        let built = member(&mut heap);
        let handle = built.handle().expect("an object");
        if !matches!(
            heap.arena().head(handle),
            Ok(Head::BigInt { .. }
                | Head::Timestamp { .. }
                | Head::LongText { .. }
                | Head::Bytes { .. })
        ) {
            continue;
        }
        let owner: Vec<_> = SCALAR_USES
            .iter()
            .map(|(name, used)| used(&heap, built).map_err(|error| format!("{name}: {error:?}")))
            .collect();
        for holder in HOLDERS.iter().chain([&ROOT_HOLDER]) {
            let case = format!("{field}, in {}", holder.name);
            let store = Counting::default();
            let mut heap = fresh(&store);
            let built = member(&mut heap);
            let held = (holder.build)(&mut heap, built);
            let root = match nested(
                &store,
                &mut heap,
                &[held, built, built],
                (holder.substitute)(*forge),
            ) {
                Ok(root) => root,
                Err(error) => {
                    failed.push(format!("{case}: {error}"));
                    continue;
                }
            };
            for (scalar_use, owner) in SCALAR_USES.iter().zip(&owner) {
                let case = format!("{case}, {}", scalar_use.0);
                let (mut heap, cold) = reopened(&store, &root);
                let reach = (holder.name != ROOT_HOLDER.name).then_some(holder);
                match used(&mut heap, reach, scalar_use, cold) {
                    Ok(Err(LoadError::Store(lash_kernel_doc::heap::HeapFault::Corrupt {
                        ..
                    }))) => {}
                    Ok(Err(other)) => {
                        failed.push(format!("{case}: the use failed otherwise: {other:?}"));
                    }
                    Ok(Ok(answer)) if Ok(&answer) == owner.as_ref() => {}
                    Ok(Ok(answer)) => failed.push(format!(
                        "{case}: the forged head answered {answer}, the owner {owner:?}"
                    )),
                    Err(error) => failed.push(format!("{case}: {error}")),
                }
            }
        }
    }
    assert!(failed.is_empty(), "{failed:#?}");
}

/// `scalar_use` of the member `holder` holds in the cold `held`, or of
/// `held` itself with no holder, each cold page loaded as the reads ask:
/// the answer, or the first load's error.
fn used(
    heap: &mut SlotHeap,
    holder: Option<&Holder>,
    scalar_use: &ScalarUse,
    held: Value,
) -> Result<Result<String, crate::heap::LoadError>, String> {
    use lash_kernel_doc::arena::native::{NativeError, Retry};
    for _ in 0..64 {
        let asked = match holder {
            Some(holder) => (holder.reach)(heap, held.handle().ok_or("the holder is no object")?)
                .and_then(|member| {
                    member.ok_or(NativeError::Malformed("the holder holds no member"))
                }),
            None => Ok(held),
        }
        .and_then(|member| (scalar_use.1)(heap, member));
        match asked {
            Ok(answer) => return Ok(Ok(answer)),
            Err(NativeError::Retry(Retry::Cold { object, chunk })) => {
                if let Err(error) = heap.load(object, chunk, &mut |visit| {
                    [held].iter().for_each(&mut *visit)
                }) {
                    return Ok(Err(error));
                }
            }
            Err(other) => return Err(format!("a use failed: {other:?}")),
        }
    }
    Err("the use asked for more than 64 loads".into())
}

fn build_error(heap: &mut SlotHeap) -> Value {
    heap.error("kind", "message", Value::Null)
        .expect("an error")
}

const ERROR_DEPTH: Access = ("its depth alone", |heap, member| {
    heap.arena().summary_of(&member).map(drop)
});

const ERROR_BODY: Access = ("its body", |heap, member| {
    let arena = heap.arena();
    let semantics = arena.semantics();
    lash_kernel_doc::Args::new(&[], arena.objects(), &semantics)
        .error(&member)
        .map(drop)
});

const REACH_ONLY: Access = ("reach the member", |_, _| Ok(()));

fn enclosing_tuple(
    heap: SlotHeap,
    member: Value,
    depth_limit: usize,
) -> Result<crate::Step, crate::MachineError> {
    use crate::laws::embedder::{Setup, Source, World, core_program};
    use crate::{CoreMachine, Machine};

    let program = core_program(
        r#"main {
            try { let tuple = (held,) } catch e { return e.kind }
            return "built"
        }"#,
        None,
    );
    let mut setup = Setup::default();
    setup.start.host.push((
        lash_kernel_value::Name::new("held"),
        lash_kernel_doc::Datum::Null,
    ));
    let mut machine = CoreMachine::start(
        program,
        setup.bounds,
        setup.start,
        Box::new(Source::default()),
    )
    .expect("the enclosing-tuple program starts");
    let vm = machine.call_law_vm();
    vm.heap = heap;
    vm.globals[0] = member;
    vm.value_depth = depth_limit;
    machine.run(&mut World::default(), u64::MAX)
}

fn check_error_heads(failed: &mut Vec<String>) {
    use crate::heap::LoadError;
    use lash_kernel_doc::heap::HeapFault;

    for (field, refuses, forge) in ERROR_FIELDS {
        for holder in HOLDERS
            .iter()
            .filter(|holder| !holder.key)
            .chain([&ROOT_HOLDER])
        {
            let case = format!("an error, {field}, in {}", holder.name);
            let store = Counting::default();
            let mut heap = fresh(&store);
            let member = build_error(&mut heap);
            let held = (holder.build)(&mut heap, member);
            let substitute = (holder.substitute)(*forge);
            let root = match (
                nested(&store, &mut heap, &[held, member, member], substitute),
                refuses,
            ) {
                (Err(error), true) if error.starts_with(DECODE_REFUSES) => continue,
                (Ok(_), true) => {
                    failed.push(format!("{case}: the page codec holds the forgery"));
                    continue;
                }
                (Ok(root), false) => root,
                (Err(error), _) => {
                    failed.push(format!("{case}: {error}"));
                    continue;
                }
            };

            let (mut heap, cold) = reopened(&store, &root);
            let depth = reached(&mut heap, holder, &ERROR_DEPTH, cold);
            if !matches!(depth, Ok(Err(LoadError::Store(HeapFault::Corrupt { .. })))) {
                failed.push(format!("{case}, {}: {depth:?}", ERROR_DEPTH.0));
            }

            let (mut heap, cold) = reopened(&store, &root);
            let body = reached(&mut heap, holder, &ERROR_BODY, cold);
            let Ok(Err(LoadError::Store(fault @ HeapFault::Corrupt { .. }))) = body else {
                failed.push(format!("{case}, {}: {body:?}", ERROR_BODY.0));
                continue;
            };

            let (mut heap, cold) = reopened(&store, &root);
            let member = match reached(&mut heap, holder, &REACH_ONLY, cold) {
                Ok(Ok(member)) => member,
                outcome => {
                    failed.push(format!("{case}, reaching the member: {outcome:?}"));
                    continue;
                }
            };
            let outcome = enclosing_tuple(heap, member, 2);
            let expected = crate::MachineError::Host(crate::HostFault::new(fault.to_string()));
            if outcome != Err(expected) {
                failed.push(format!(
                    "{case}, constructing the enclosing tuple: {outcome:?}"
                ));
            }
        }
    }

    let store = Counting::default();
    let mut heap = fresh(&store);
    let member = build_error(&mut heap);
    let root = heap.seal(&[member]).expect("the unaltered error seals");
    let (mut heap, error) = reopened(&store, &root);
    let (summary, first, _) =
        store.during(|| warm(&mut heap, &[error], |heap| heap.arena().summary_of(&error)));
    let (again, repeat, _) = store.during(|| heap.arena().summary_of(&error));
    eprintln!("error depth: {first} page reads on first use, {repeat} on repeat");
    assert_eq!(summary.depth, 1);
    assert_eq!(again.expect("a cached summary"), summary);
    assert_eq!(repeat, 0);
    for (depth_limit, expected) in [(2, "built"), (1, "too_deep")] {
        let (heap, error) = reopened(&store, &root);
        let outcome = enclosing_tuple(heap, error, depth_limit);
        assert!(
            matches!(
                &outcome,
                Ok(crate::Step::Ended(crate::End::Finished(finished)))
                    if finished.result == lash_kernel_doc::Datum::Text(expected.to_owned())
            ),
            "{outcome:?}"
        );
    }
}
