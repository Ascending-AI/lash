//! The projection seam (seam D of `docs/design/kernel-end-state.md`): the
//! one place a run's arena values and their page form meet. Every
//! conversion between an [`ArenaHandle`] and an [`EntityId`] is here.
//!
//! [`project`] gives a value's page form, what a saved tree row or a
//! census root holds; [`admit`] gives the arena value of a page form,
//! interning a mutable object by its identity so the run sees one handle
//! for it. Both read and write pages only; the heap's entity heads are
//! [`materialize`]'s (a proxy's contents, decoded outside any native call)
//! and [`flush`]'s (every dirty object, written back at a projection point
//! or before an exact refinement).
//!
//! A mutable object takes its durable id where it is first projected
//! (seam C). A projection point walks its roots in the canonical order
//! first (`KernelMachine::identify`), so every id it gives follows that
//! walk; a value saved outside one (a task's result, a session binding, a
//! continuation's role) gives its objects their ids as it is saved.
//!
//! [`ArenaHandle`]: lash_kernel_doc::arena::ArenaHandle

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use lash_kernel_doc::arena::ShortText;
use lash_kernel_doc::arena::{ArenaHandle, Body, Bytes, Chunk, ErrorBody, LongText, Sequence};
use lash_kernel_doc::heap::{
    EntityId, HeapFault, Identity, PageReader as _, PageValue, Payload, TextRef,
};
use lash_kernel_doc::heap_values::text_summary;
use lash_kernel_doc::{Integer, Site, Timestamp, Value};
use lash_kernel_state::arena::{
    Arena, CARD, ColdChunk, PayloadPart, ProxyKind, Summary, cold_text,
};
use lash_kernel_state::kernel_internals::payload;
use lash_kernel_state::kernel_internals::tree::Access;
use lash_kernel_state::kernel_internals::{
    PagedHeap, StoredCards, StoredHead, StoredObject, page_reads, stored_pages,
};
use lash_kernel_state::paged::{HeapWriteError, NameRef, ObjectKind, SiteRef};

use crate::compile::{CodeId, Executable};

/// The run's arena, shared by the machine and the frames that convert
/// their saved rows through this seam.
pub(crate) type SharedArena = Rc<RefCell<Arena>>;

fn malformed(problem: &str) -> HeapFault {
    HeapFault::Malformed(problem.to_string())
}

fn arena_fault(error: lash_kernel_doc::NativeError) -> HeapFault {
    HeapFault::Malformed(error.to_string())
}

fn entity(arena: &mut Arena, handle: ArenaHandle) -> Result<EntityId, HeapFault> {
    arena.identify(handle).map_err(arena_fault)
}

/// The page form of `value`, built through `pages`. An immutable's is
/// built once and kept with it; a mutable object's is its identity, given
/// now if it has none.
///
/// # Errors
///
/// The page port's faults, and a value the arena does not hold.
pub(crate) fn project(
    arena: &mut Arena,
    pages: &Access<'_>,
    value: &Value,
) -> Result<PageValue, HeapFault> {
    #[cfg(any(test, feature = "perf-witness"))]
    crate::perf_witness::record(|work| work.page_forms += 1);
    Ok(match *value {
        Value::Null => PageValue::Null,
        Value::Absent => PageValue::Absent,
        Value::Bool(value) => PageValue::Bool(value),
        Value::Int(word) => PageValue::Int(word.into()),
        Value::Float(float) => PageValue::Float(float),
        Value::Text(short) => PageValue::Text(resident_text(short.as_str())),
        Value::Function(short) => PageValue::Function(resident_text(short.as_str())),
        Value::Timestamp(word) => PageValue::Timestamp(Timestamp {
            nanoseconds: Integer::from(word),
        }),
        Value::Task(task) => PageValue::Task(task),
        Value::TaskRef(task) => PageValue::Ref(Identity::Task(task)),
        Value::List(handle) => PageValue::List(entity(arena, handle)?),
        Value::Map(handle) => PageValue::Map(entity(arena, handle)?),
        Value::Set(handle) => PageValue::Set(entity(arena, handle)?),
        Value::Record(handle) => PageValue::Record(entity(arena, handle)?),
        Value::Closure(handle) => PageValue::Closure(entity(arena, handle)?),
        Value::ObjectRef(handle) => PageValue::Ref(Identity::Object(entity(arena, handle)?)),
        Value::BigInt(handle)
        | Value::LongText(handle)
        | Value::Bytes(handle)
        | Value::BigTimestamp(handle)
        | Value::Tuple(handle)
        | Value::Error(handle)
        | Value::LongFunction(handle)
        | Value::Handle(handle) => {
            let facts = arena.facts(handle).map_err(arena_fault)?;
            if let Some(page) = facts.page.get() {
                return Ok(page.clone());
            }
            let page = immutable(arena, pages, handle)?;
            arena.keep_page(handle, page.clone()).map_err(arena_fault)?;
            page
        }
    })
}

fn resident_text(text: &str) -> TextRef {
    TextRef {
        normal: Default::default(),
        summary: text_summary(text),
        body: Payload::Resident(Arc::from(text)),
    }
}

/// The text a long text's one resident chunk holds.
fn whole_text(text: &LongText) -> Result<&str, HeapFault> {
    match &*text.chunks {
        [Chunk::Resident(text)] => Ok(text),
        [] => Ok(""),
        _ => Err(malformed("a long text with no page form is not resident")),
    }
}

fn text_of(arena: &mut Arena, pages: &Access<'_>, value: &Value) -> Result<TextRef, HeapFault> {
    match project(arena, pages, value)? {
        PageValue::Text(text) => Ok(text),
        _ => Err(malformed("an error's kind or message is not a text")),
    }
}

fn immutable(
    arena: &mut Arena,
    pages: &Access<'_>,
    handle: ArenaHandle,
) -> Result<PageValue, HeapFault> {
    // A tuple's members and an error's parts are projected through the
    // arena, which may give an object among them its id.
    let members = match arena.body(handle).map_err(arena_fault)? {
        Body::Tuple(sequence) => {
            let mut members = Vec::with_capacity(sequence.len as usize);
            for chunk in &sequence.chunks {
                let Chunk::Resident(values) = chunk else {
                    return Err(malformed("a tuple with no page form is not resident"));
                };
                members.extend_from_slice(values);
            }
            Some(Ok(members))
        }
        Body::Error(ErrorBody {
            kind,
            message,
            data,
        }) => Some(Err((*kind, *message, *data))),
        _ => None,
    };
    match members {
        Some(Ok(members)) => {
            let members = members
                .iter()
                .map(|value| project(arena, pages, value))
                .collect::<Result<Vec<_>, _>>()?;
            return Ok(PageValue::Tuple(stored_pages::tuple(pages, members)?));
        }
        Some(Err((kind, message, data))) => {
            let kind = text_of(arena, pages, &kind)?;
            let message = text_of(arena, pages, &message)?;
            let data = project(arena, pages, &data)?;
            return Ok(PageValue::Error(stored_pages::error(
                pages, kind, message, data,
            )?));
        }
        None => {}
    }
    Ok(match arena.body(handle).map_err(arena_fault)? {
        Body::BigInt(int) => PageValue::Int(stored_pages::int(pages, int.clone())?),
        Body::Timestamp(nanoseconds) => PageValue::Timestamp(Timestamp {
            nanoseconds: nanoseconds.clone(),
        }),
        Body::LongText(text) => PageValue::Text(stored_pages::text(pages, whole_text(text)?)?),
        Body::LongFunction(text) => {
            PageValue::Function(stored_pages::text(pages, whole_text(text)?)?)
        }
        Body::Bytes(bytes) => match &*bytes.chunks {
            [Chunk::Resident(bytes)] => PageValue::Bytes(stored_pages::bytes(pages, bytes)?),
            [] => PageValue::Bytes(stored_pages::bytes(pages, &[])?),
            _ => return Err(malformed("bytes with no page form are not resident")),
        },
        Body::Handle(handle) => PageValue::Handle(Arc::new(handle.clone())),
        _ => return Err(malformed("an immutable handle names a mutable body")),
    })
}

/// The arena value of page form `value`. A mutable object is the one
/// handle the run has for its identity, or a cold proxy of it; an
/// immutable is copied in when its payload is in place and stays cold on
/// its pages otherwise, except an error's data and an integer's digits,
/// which are read at once.
///
/// # Errors
///
/// The page port's faults.
pub(crate) fn admit(
    arena: &mut Arena,
    pages: &Access<'_>,
    value: &PageValue,
) -> Result<Value, HeapFault> {
    Ok(match value {
        PageValue::Null => Value::Null,
        PageValue::Absent => Value::Absent,
        PageValue::Bool(value) => Value::Bool(*value),
        PageValue::Float(float) => Value::Float(*float),
        PageValue::Int(int) => {
            let whole = page_reads::int(pages, int)?;
            match whole.word() {
                Some(word) => Value::Int(word),
                None => Value::BigInt(arena.immutable(
                    Body::BigInt(whole),
                    Summary::default(),
                    Some(value.clone()),
                )),
            }
        }
        PageValue::Timestamp(timestamp) => match timestamp.nanoseconds.word() {
            Some(word) => Value::Timestamp(word),
            None => Value::BigTimestamp(arena.immutable(
                Body::Timestamp(timestamp.nanoseconds.clone()),
                Summary::default(),
                Some(value.clone()),
            )),
        },
        PageValue::Text(text) => admit_text(arena, text, value)?,
        PageValue::Function(text) => {
            let name = page_reads::text(pages, text)?;
            match ShortText::new(&name) {
                Some(short) => Value::Function(short),
                None => Value::LongFunction(arena.immutable(
                    Body::LongFunction(LongText {
                        summary: text.summary,
                        chunks: Box::new([Chunk::Resident(Box::from(&*name))]),
                    }),
                    Summary::default(),
                    Some(value.clone()),
                )),
            }
        }
        PageValue::Bytes(bytes) => {
            let chunk = match &bytes.body {
                Payload::Resident(held) => Chunk::Resident(Box::from(&**held)),
                Payload::Paged(page) => Chunk::Cold {
                    page: **page,
                    len: u32::try_from(bytes.len).unwrap_or(u32::MAX),
                },
            };
            Value::Bytes(arena.immutable(
                Body::Bytes(Bytes {
                    len: bytes.len,
                    chunks: Box::new([chunk]),
                }),
                Summary::default(),
                Some(value.clone()),
            ))
        }
        PageValue::Tuple(tuple) => {
            let summary = Summary {
                cost: tuple.summary.cost.0,
                depth: tuple.summary.depth,
                task_edges: tuple.summary.task_edges,
                has_edges: tuple.summary.has_edges,
            };
            let chunk = match &tuple.body {
                Payload::Resident(members) => Chunk::Resident(
                    members
                        .iter()
                        .map(|member| admit(arena, pages, member))
                        .collect::<Result<Vec<_>, _>>()?,
                ),
                Payload::Paged(page) => Chunk::Cold {
                    page: **page,
                    len: u32::try_from(tuple.summary.len).unwrap_or(u32::MAX),
                },
            };
            Value::Tuple(arena.immutable(
                Body::Tuple(Sequence {
                    len: tuple.summary.len,
                    chunks: vec![chunk],
                }),
                summary,
                Some(value.clone()),
            ))
        }
        PageValue::Error(error) => {
            let kind = admit_text(arena, &error.kind, &PageValue::Text(error.kind.clone()))?;
            let message = admit_text(
                arena,
                &error.message,
                &PageValue::Text(error.message.clone()),
            )?;
            let data = page_reads::error_data(pages, error)?;
            let data = admit(arena, pages, &data)?;
            Value::Error(arena.immutable(
                Body::Error(ErrorBody {
                    kind,
                    message,
                    data,
                }),
                Summary {
                    cost: error.cost.0,
                    depth: error.depth,
                    task_edges: error.task_edges,
                    has_edges: error.has_edges,
                },
                Some(value.clone()),
            ))
        }
        PageValue::Handle(handle) => Value::Handle(arena.immutable(
            Body::Handle(handle.as_ref().clone()),
            Summary::default(),
            Some(value.clone()),
        )),
        PageValue::Task(task) => Value::Task(*task),
        PageValue::Ref(Identity::Task(task)) => Value::TaskRef(*task),
        PageValue::Ref(Identity::Object(id)) => {
            Value::ObjectRef(arena.proxy(*id, ProxyKind::Unknown))
        }
        PageValue::List(id) => Value::List(arena.proxy(*id, ProxyKind::List)),
        PageValue::Map(id) => Value::Map(arena.proxy(*id, ProxyKind::Map)),
        PageValue::Set(id) => Value::Set(arena.proxy(*id, ProxyKind::Set)),
        PageValue::Record(id) => Value::Record(arena.proxy(*id, ProxyKind::Record)),
        PageValue::Closure(id) => Value::Closure(arena.proxy(*id, ProxyKind::Closure)),
    })
}

fn admit_text(arena: &mut Arena, text: &TextRef, page: &PageValue) -> Result<Value, HeapFault> {
    Ok(match &text.body {
        Payload::Resident(held) => match ShortText::new(held) {
            Some(short) => Value::Text(short),
            None => Value::LongText(arena.immutable(
                Body::LongText(LongText {
                    summary: text.summary,
                    chunks: Box::new([Chunk::Resident(Box::from(&**held))]),
                }),
                Summary::default(),
                Some(page.clone()),
            )),
        },
        Payload::Paged(at) => Value::LongText(arena.immutable(
            Body::LongText(cold_text(text.summary, **at)),
            Summary::default(),
            Some(page.clone()),
        )),
    })
}

/// The arena handle of the variable `cell` a frame or a closure shares.
pub(crate) fn admit_cell(arena: &mut Arena, cell: EntityId) -> ArenaHandle {
    arena.proxy(cell, ProxyKind::Cell)
}

/// The identity of the shared variable `cell`.
///
/// # Errors
///
/// A handle that names no durable object.
pub(crate) fn project_cell(arena: &mut Arena, cell: ArenaHandle) -> Result<EntityId, HeapFault> {
    entity(arena, cell)
}

/// The code a closure of `site` runs: its body is the site's first child.
pub(crate) fn closure_code(exe: &Executable, site: &Site) -> Result<CodeId, HeapFault> {
    exe.code_at(&site.child(0))
        .ok_or_else(|| malformed("a closure names no admitted code"))
}

/// Decodes what a read of chunk `chunk` of the cold object `handle`
/// needs: a proxy's head (a small object whole, a large list or table as
/// cold chunks), else that chunk and, doubling with each decode of the
/// object since the last statement boundary, the cold chunks after it. An
/// object whose chunks are then all resident is joined into one chunk.
/// Members are admitted, as proxies where they are objects.
///
/// # Errors
///
/// The page port's faults, and a proxy the heap holds no object for.
pub(crate) fn materialize(
    arena: &mut Arena,
    heap: &PagedHeap,
    exe: &Executable,
    handle: ArenaHandle,
    chunk: u32,
) -> Result<(), HeapFault> {
    let facts = arena.facts(handle).map_err(arena_fault)?;
    let Some(entity) = facts.entity else {
        let page = facts.page.get().cloned();
        let count = arena.decode_count(handle).map_err(arena_fault)?;
        return match page {
            Some(page) => warm_immutable(arena, heap.pages(), handle, &page, chunk, count),
            None if arena.needs_decode(handle).map_err(arena_fault)? => {
                Err(malformed("a cold immutable has no page form"))
            }
            None => Ok(()),
        };
    };
    if facts.cold {
        return open(arena, heap, exe, handle, entity);
    }
    let pages = heap.pages();
    let count = arena.decode_count(handle).map_err(arena_fault)?;
    for (at, cold) in arena
        .cold_chunks(handle, chunk, count)
        .map_err(arena_fault)?
    {
        match cold {
            ColdChunk::List { start, count } => {
                let values = heap
                    .read_list_rows(entity, start, count)?
                    .iter()
                    .map(|value| admit(arena, pages, value))
                    .collect::<Result<Vec<_>, _>>()?;
                arena
                    .decoded_list(handle, at, values)
                    .map_err(arena_fault)?;
            }
            ColdChunk::Table { span } => {
                let set = matches!(arena.body(handle).map_err(arena_fault)?, Body::Set(_));
                let mut entries = Vec::new();
                for (seq, key, value) in heap.read_table_rows(entity, span.start, span.end)? {
                    let normal = heap
                        .normal_key(&key)?
                        .ok_or_else(|| malformed("a stored entry's key is not a legal key"))?;
                    let key = admit(arena, pages, &key)?;
                    let value = match set {
                        true => Value::Absent,
                        false => admit(arena, pages, &value)?,
                    };
                    entries.push((seq, key, value, normal));
                }
                arena
                    .decoded_table(handle, at, entries)
                    .map_err(arena_fault)?;
            }
        }
    }
    arena.join_chunks(handle).map_err(arena_fault)?;
    Ok(())
}

/// Opens the head of the proxy `handle`, known as `entity`: a closure, a
/// shared variable, or a list or table of at most one chunk is decoded
/// whole; a larger list or table gets cold chunks.
fn open(
    arena: &mut Arena,
    heap: &PagedHeap,
    exe: &Executable,
    handle: ArenaHandle,
    entity: EntityId,
) -> Result<(), HeapFault> {
    let pages = heap.pages();
    let head = heap
        .read_head_of(entity)?
        .ok_or_else(|| malformed("a proxy names no object of the heap"))?;
    let chunk = Sequence::CHUNK as u64;
    let (body, sequences) = match head {
        StoredHead::List { len, cost } if len > chunk => {
            return arena.open_list(handle, len, cost).map_err(arena_fault);
        }
        StoredHead::List { len, .. } => {
            let values = heap
                .read_list_rows(entity, 0, len)?
                .iter()
                .map(|value| admit(arena, pages, value))
                .collect::<Result<Vec<_>, _>>()?;
            (
                Body::List(Sequence {
                    len: values.len() as u64,
                    chunks: vec![Chunk::Resident(values)],
                }),
                None,
            )
        }
        StoredHead::Table {
            kind,
            len,
            next,
            cost,
        } if len > chunk => {
            let kind = match kind {
                ObjectKind::Map => ProxyKind::Map,
                ObjectKind::Set => ProxyKind::Set,
                ObjectKind::Record => ProxyKind::Record,
                _ => return Err(malformed("a table of another kind")),
            };
            return arena
                .open_table(handle, kind, len, next, cost)
                .map_err(arena_fault);
        }
        StoredHead::Table {
            kind, len, next, ..
        } => {
            let entries = heap.read_table_rows(entity, 0, next)?;
            if entries.len() as u64 != len {
                return Err(malformed("a table's rows disagree with its length"));
            }
            let mut pairs = Vec::with_capacity(entries.len() * 2);
            let mut seqs = Vec::with_capacity(entries.len());
            for (seq, key, value) in &entries {
                pairs.push(admit(arena, pages, key)?);
                pairs.push(match kind {
                    ObjectKind::Set => Value::Absent,
                    _ => admit(arena, pages, value)?,
                });
                seqs.push(*seq);
            }
            let table = lash_kernel_doc::arena::Table {
                len: entries.len() as u64,
                chunks: vec![Chunk::Resident(pairs)],
            };
            (
                match kind {
                    ObjectKind::Map => Body::Map(table),
                    ObjectKind::Set => Body::Set(table),
                    ObjectKind::Record => Body::Record(table),
                    _ => return Err(malformed("a table of another kind")),
                },
                Some((seqs, next)),
            )
        }
        StoredHead::Small(StoredObject::Closure { site, captures }) => {
            let site = site.resolve(pages)?;
            let code = exe.code(closure_code(exe, &site)?);
            let mut named = Vec::with_capacity(captures.len());
            for (name, cell) in captures {
                named.push((name.resolve(pages)?, cell));
            }
            let mut cells = Vec::with_capacity(code.captures.len());
            for capture in &code.captures {
                let name = &code.slots[capture.inner as usize].name;
                let cell = named
                    .iter()
                    .find(|(held, _)| held == name)
                    .map(|(_, cell)| *cell)
                    .ok_or_else(|| malformed("a closure capture is missing"))?;
                cells.push(admit_cell(arena, cell));
            }
            (
                Body::Closure {
                    site,
                    captures: cells.into_boxed_slice(),
                    code: None,
                },
                None,
            )
        }
        StoredHead::Small(StoredObject::Variable(value)) => {
            (Body::Cell(admit(arena, pages, &value)?), None)
        }
        StoredHead::Small(_) => return Err(malformed("a small head of another kind")),
    };
    arena.materialized(handle, body).map_err(arena_fault)?;
    if let Some((seqs, next)) = sequences {
        arena
            .set_sequences(handle, seqs, next)
            .map_err(arena_fault)?;
    }
    Ok(())
}

/// Decodes what a read of chunk `chunk` of the cold immutable `handle`
/// needs from its page form `page`: a long text's or bytes' payload node by
/// node, `count` leaves from that chunk on, each decoded chunk an index
/// node's children or a leaf; anything else whole.
fn warm_immutable(
    arena: &mut Arena,
    pages: &Access<'_>,
    handle: ArenaHandle,
    page: &PageValue,
    chunk: u32,
    count: u32,
) -> Result<(), HeapFault> {
    let paged = match page {
        PageValue::Text(TextRef {
            summary,
            body: Payload::Paged(root),
            ..
        }) => Some((**root, *summary, true)),
        PageValue::Bytes(bytes) => match &bytes.body {
            Payload::Paged(root) => Some((
                **root,
                lash_kernel_doc::heap::TextSummary {
                    utf8_bytes: bytes.len,
                    chars: 0,
                    utf16_units: 0,
                },
                false,
            )),
            Payload::Resident(_) => None,
        },
        _ => None,
    };
    if let Some((root, summary, text)) = paged {
        let mut at = chunk;
        let mut leaves = 0u32;
        while leaves < count && (at as usize) < arena.chunk_count(handle).map_err(arena_fault)? {
            let Some(range) = arena.payload_chunk(handle, at).map_err(arena_fault)? else {
                at += 1;
                continue;
            };
            match payload::node_at(pages, root, summary, text, range)? {
                payload::PayloadNode::Leaf(bytes) => {
                    arena
                        .decoded_payload(handle, at, PayloadPart::Leaf(bytes))
                        .map_err(arena_fault)?;
                    leaves += 1;
                    at += 1;
                }
                // The chunk becomes its children: the first is decoded next.
                payload::PayloadNode::Index(children) => arena
                    .decoded_payload(handle, at, PayloadPart::Index(children))
                    .map_err(arena_fault)?,
            }
        }
        arena.join_chunks(handle).map_err(arena_fault)?;
        return Ok(());
    }
    let body = match (arena.body(handle).map_err(arena_fault)?, page) {
        (Body::LongText(text), PageValue::Text(paged)) => Body::LongText(LongText {
            summary: text.summary,
            chunks: Box::new([Chunk::Resident(Box::from(&*page_reads::text(
                pages, paged,
            )?))]),
        }),
        (Body::Bytes(bytes), PageValue::Bytes(paged)) => Body::Bytes(Bytes {
            len: bytes.len,
            chunks: Box::new([Chunk::Resident(Box::from(&*page_reads::bytes(
                pages, paged,
            )?))]),
        }),
        (Body::Tuple(sequence), PageValue::Tuple(paged)) => {
            let len = sequence.len;
            let members = page_reads::tuple(pages, paged)?
                .iter()
                .map(|member| admit(arena, pages, member))
                .collect::<Result<Vec<_>, _>>()?;
            if members.len() as u64 != len {
                return Err(malformed("a tuple's pages disagree with its length"));
            }
            Body::Tuple(Sequence {
                len,
                chunks: vec![Chunk::Resident(members)],
            })
        }
        _ => return Ok(()),
    };
    if !arena.needs_decode(handle).map_err(arena_fault)? {
        return Ok(());
    }
    arena.materialized(handle, body).map_err(arena_fault)
}

/// Writes every dirty object of the arena to the heap, in identity order,
/// and marks it clean: a fresh object whole, a list or a table the heap
/// holds by its dirty cards. What a projection point and an exact
/// refinement read. An object a stored one names that had no id takes one
/// as it is projected, and is written in the next round.
///
/// # Errors
///
/// The page port's faults, and a census fault.
pub(crate) fn flush(
    arena: &mut Arena,
    heap: &mut PagedHeap,
    exe: &Executable,
) -> Result<(), HeapWriteError> {
    loop {
        let dirty = arena.take_dirty();
        if dirty.is_empty() {
            return Ok(());
        }
        for (entity, handle) in dirty {
            match cards(arena, heap.pages(), handle)? {
                Some(cards) => heap.store_cards(entity, cards)?,
                None => {
                    let object = stored(arena, heap.pages(), exe, handle)?;
                    heap.store_object(entity, object)?;
                }
            }
            arena.clean(handle).map_err(arena_fault)?;
        }
    }
}

/// The dirty cards of a list or a table the heap holds, in page form;
/// `None` for an object projected whole. A value among them that had no
/// id takes one as it is projected.
fn cards(
    arena: &mut Arena,
    pages: &Access<'_>,
    handle: ArenaHandle,
) -> Result<Option<StoredCards>, HeapFault> {
    /// A card number with its entries: sequence, key and value.
    type TableCard = (u64, Vec<(u64, Value, Value)>);
    enum Held {
        List(u64, Vec<(u64, Vec<Value>)>),
        Table(bool, u64, u64, Vec<TableCard>),
    }
    let facts = arena.facts(handle).map_err(arena_fault)?;
    if !facts.based {
        return Ok(None);
    }
    let held = match arena.body(handle).map_err(arena_fault)? {
        Body::List(sequence) => {
            let mut cards = Vec::with_capacity(facts.cards.len());
            for card in &facts.cards {
                let start = card * CARD;
                let end = (start + CARD).min(sequence.len);
                let mut values = Vec::with_capacity(end.saturating_sub(start) as usize);
                for position in start..end {
                    values.push(
                        arena
                            .item(handle, position)
                            .map_err(arena_fault)?
                            .ok_or_else(|| malformed("a list's dirty card misses a value"))?,
                    );
                }
                cards.push((*card, values));
            }
            Held::List(sequence.len, cards)
        }
        body @ (Body::Map(table) | Body::Set(table) | Body::Record(table)) => {
            let index = facts
                .index
                .as_deref()
                .ok_or_else(|| malformed("a table has no index"))?;
            let mut cards = Vec::with_capacity(facts.cards.len());
            for card in &facts.cards {
                let span = card * CARD..(card + 1) * CARD;
                let mut entries = Vec::new();
                for (at, chunk) in table.chunks.iter().enumerate() {
                    let held = index.span(at);
                    if held.end <= span.start || held.start >= span.end {
                        continue;
                    }
                    let Chunk::Resident(pairs) = chunk else {
                        return Err(malformed("a table's dirty card is not resident"));
                    };
                    for (pair, seq) in pairs.as_chunks::<2>().0.iter().zip(index.sequences(at)) {
                        if span.contains(seq) {
                            entries.push((*seq, pair[0], pair[1]));
                        }
                    }
                }
                cards.push((*card, entries));
            }
            Held::Table(matches!(body, Body::Set(_)), table.len, index.next(), cards)
        }
        _ => return Ok(None),
    };
    Ok(Some(match held {
        Held::List(len, held) => {
            let mut cards = Vec::with_capacity(held.len());
            for (card, values) in held {
                let values = values
                    .iter()
                    .map(|value| project(arena, pages, value))
                    .collect::<Result<_, _>>()?;
                cards.push((card, values));
            }
            StoredCards::List { len, cards }
        }
        Held::Table(set, len, next, held) => {
            let mut cards = Vec::with_capacity(held.len());
            for (card, entries) in held {
                let mut stored = Vec::with_capacity(entries.len());
                for (seq, key, value) in entries {
                    let value = match set {
                        true => PageValue::Null,
                        false => project(arena, pages, &value)?,
                    };
                    stored.push((seq, project(arena, pages, &key)?, value));
                }
                cards.push((card, stored));
            }
            StoredCards::Table { len, next, cards }
        }
    }))
}

fn stored(
    arena: &mut Arena,
    pages: &Access<'_>,
    exe: &Executable,
    handle: ArenaHandle,
) -> Result<StoredObject, HeapFault> {
    let resident = |chunks: &[Chunk<Vec<Value>>]| -> Result<Vec<Value>, HeapFault> {
        let mut values = Vec::new();
        for chunk in chunks {
            let Chunk::Resident(held) = chunk else {
                return Err(malformed("a dirty object is not resident"));
            };
            values.extend_from_slice(held);
        }
        Ok(values)
    };
    enum Held {
        List(Vec<Value>),
        Table(ObjectKind, Vec<Value>, Vec<u64>, u64),
        Closure(Arc<Site>, Vec<ArenaHandle>),
        Cell(Value),
    }
    let held = match arena.body(handle).map_err(arena_fault)? {
        Body::List(sequence) => Held::List(resident(&sequence.chunks)?),
        body @ (Body::Map(table) | Body::Set(table) | Body::Record(table)) => {
            let kind = match body {
                Body::Map(_) => ObjectKind::Map,
                Body::Set(_) => ObjectKind::Set,
                _ => ObjectKind::Record,
            };
            let index = arena
                .facts(handle)
                .map_err(arena_fault)?
                .index
                .as_deref()
                .ok_or_else(|| malformed("a resident table has no index"))?;
            Held::Table(
                kind,
                resident(&table.chunks)?,
                (0..table.chunks.len())
                    .flat_map(|chunk| index.sequences(chunk).iter().copied())
                    .collect(),
                index.next(),
            )
        }
        Body::Closure { site, captures, .. } => Held::Closure(site.clone(), captures.to_vec()),
        Body::Cell(value) => Held::Cell(*value),
        _ => return Err(malformed("a dirty handle names an immutable")),
    };
    Ok(match held {
        Held::List(values) => StoredObject::List(
            values
                .iter()
                .map(|value| project(arena, pages, value))
                .collect::<Result<_, _>>()?,
        ),
        Held::Table(kind, pairs, seqs, next) => {
            let mut entries = Vec::with_capacity(pairs.len() / 2);
            for (pair, seq) in pairs.as_chunks::<2>().0.iter().zip(seqs) {
                let value = match kind {
                    ObjectKind::Set => PageValue::Null,
                    _ => project(arena, pages, &pair[1])?,
                };
                entries.push((seq, project(arena, pages, &pair[0])?, value));
            }
            StoredObject::Table {
                kind,
                entries,
                next,
            }
        }
        Held::Closure(site, captures) => {
            let code = exe.code(closure_code(exe, &site)?);
            let mut held = Vec::with_capacity(captures.len());
            for (capture, cell) in code.captures.iter().zip(captures) {
                let name = &code.slots[capture.inner as usize].name;
                held.push((NameRef::new(name, pages)?, project_cell(arena, cell)?));
            }
            StoredObject::Closure {
                site: Box::new(SiteRef::new(&site, pages)?),
                captures: held,
            }
        }
        Held::Cell(value) => StoredObject::Variable(project(arena, pages, &value)?),
    })
}
