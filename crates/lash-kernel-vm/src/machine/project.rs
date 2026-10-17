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
//! [`ArenaHandle`]: lash_kernel_doc::arena::ArenaHandle

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use lash_kernel_doc::arena::ShortText;
use lash_kernel_doc::arena::{ArenaHandle, Body, Bytes, Chunk, ErrorBody, LongText, Sequence};
use lash_kernel_doc::heap::{EntityId, HeapFault, Identity, PageValue, Payload, TextRef};
use lash_kernel_doc::heap_values::text_summary;
use lash_kernel_doc::{Integer, Site, Timestamp, Value};
use lash_kernel_state::arena::{Arena, ProxyKind, Summary, cold_text};
use lash_kernel_state::kernel_internals::tree::Access;
use lash_kernel_state::kernel_internals::{PagedHeap, StoredObject, page_reads, stored_pages};
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

fn entity(arena: &Arena, handle: ArenaHandle) -> Result<EntityId, HeapFault> {
    arena.entity_of(handle).map_err(arena_fault)
}

/// The page form of `value`, built through `pages`. An immutable's is
/// built once and kept with it; a mutable object's is its identity.
///
/// # Errors
///
/// The page port's faults, and a value the arena does not hold.
pub(crate) fn project(
    arena: &Arena,
    pages: &Access<'_>,
    value: &Value,
) -> Result<PageValue, HeapFault> {
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

fn text_of(arena: &Arena, pages: &Access<'_>, value: &Value) -> Result<TextRef, HeapFault> {
    match project(arena, pages, value)? {
        PageValue::Text(text) => Ok(text),
        _ => Err(malformed("an error's kind or message is not a text")),
    }
}

fn immutable(
    arena: &Arena,
    pages: &Access<'_>,
    handle: ArenaHandle,
) -> Result<PageValue, HeapFault> {
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
        Body::Tuple(sequence) => {
            let mut members = Vec::with_capacity(sequence.len as usize);
            for chunk in &sequence.chunks {
                let Chunk::Resident(values) = chunk else {
                    return Err(malformed("a tuple with no page form is not resident"));
                };
                for value in values {
                    members.push(project(arena, pages, value)?);
                }
            }
            PageValue::Tuple(stored_pages::tuple(pages, members)?)
        }
        Body::Error(ErrorBody {
            kind,
            message,
            data,
        }) => PageValue::Error(stored_pages::error(
            pages,
            text_of(arena, pages, kind)?,
            text_of(arena, pages, message)?,
            project(arena, pages, data)?,
        )?),
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
pub(crate) fn project_cell(arena: &Arena, cell: ArenaHandle) -> Result<EntityId, HeapFault> {
    entity(arena, cell)
}

/// The code a closure of `site` runs: its body is the site's first child.
pub(crate) fn closure_code(exe: &Executable, site: &Site) -> Result<CodeId, HeapFault> {
    exe.code_at(&site.child(0))
        .ok_or_else(|| malformed("a closure names no admitted code"))
}

/// Decodes the cold object `handle` whole: a proxy's contents from the
/// heap by its identity, or an immutable's chunks from its pages. Its
/// members are admitted, as proxies where they are objects.
///
/// # Errors
///
/// The page port's faults, and a proxy the heap holds no object for.
pub(crate) fn materialize(
    arena: &mut Arena,
    heap: &PagedHeap,
    exe: &Executable,
    handle: ArenaHandle,
) -> Result<(), HeapFault> {
    if !arena.needs_decode(handle).map_err(arena_fault)? {
        return Ok(());
    }
    let pages = heap.pages();
    let facts = arena.facts(handle).map_err(arena_fault)?;
    let Some(entity) = facts.entity else {
        let page = facts
            .page
            .get()
            .cloned()
            .ok_or_else(|| malformed("a cold immutable has no page form"))?;
        return warm_immutable(arena, pages, handle, &page);
    };
    let object = heap
        .read_object(entity)?
        .ok_or_else(|| malformed("a proxy names no object of the heap"))?;
    let (body, sequences) = match object {
        StoredObject::List(values) => {
            let values = values
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
        StoredObject::Table {
            kind,
            entries,
            next,
        } => {
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
        StoredObject::Closure { site, captures } => {
            let site = site.resolve(pages)?.as_ref().clone();
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
                },
                None,
            )
        }
        StoredObject::Variable(value) => (Body::Cell(admit(arena, pages, &value)?), None),
    };
    arena.materialized(handle, body).map_err(arena_fault)?;
    if let Some((seqs, next)) = sequences {
        arena
            .set_sequences(handle, seqs, next)
            .map_err(arena_fault)?;
    }
    Ok(())
}

fn warm_immutable(
    arena: &mut Arena,
    pages: &Access<'_>,
    handle: ArenaHandle,
    page: &PageValue,
) -> Result<(), HeapFault> {
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
    arena.materialized(handle, body).map_err(arena_fault)
}

/// Writes every dirty object of the arena to the heap, in identity order,
/// and marks it clean: what a projection point and an exact refinement
/// read.
///
/// # Errors
///
/// The page port's faults, and a census fault.
pub(crate) fn flush(
    arena: &mut Arena,
    heap: &mut PagedHeap,
    exe: &Executable,
) -> Result<(), HeapWriteError> {
    for (entity, handle) in arena.dirty_objects() {
        let object = stored(arena, heap.pages(), exe, handle)?;
        heap.store_object(entity, object)?;
        arena.clean(handle).map_err(arena_fault)?;
    }
    Ok(())
}

fn stored(
    arena: &Arena,
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
    Ok(match arena.body(handle).map_err(arena_fault)? {
        Body::List(sequence) => StoredObject::List(
            resident(&sequence.chunks)?
                .iter()
                .map(|value| project(arena, pages, value))
                .collect::<Result<_, _>>()?,
        ),
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
            let pairs = resident(&table.chunks)?;
            let mut entries = Vec::with_capacity(pairs.len() / 2);
            for (pair, seq) in pairs.as_chunks::<2>().0.iter().zip(index.sequences()) {
                let value = match kind {
                    ObjectKind::Set => PageValue::Null,
                    _ => project(arena, pages, &pair[1])?,
                };
                entries.push((*seq, project(arena, pages, &pair[0])?, value));
            }
            StoredObject::Table {
                kind,
                entries,
                next: index.next(),
            }
        }
        Body::Closure { site, captures } => {
            let code = exe.code(closure_code(exe, site)?);
            let mut held = Vec::with_capacity(captures.len());
            for (capture, cell) in code.captures.iter().zip(captures.iter()) {
                let name = &code.slots[capture.inner as usize].name;
                held.push((NameRef::new(name, pages)?, project_cell(arena, *cell)?));
            }
            StoredObject::Closure {
                site: Box::new(SiteRef::new(site, pages)?),
                captures: held,
            }
        }
        Body::Cell(value) => StoredObject::Variable(project(arena, pages, value)?),
        _ => return Err(malformed("a dirty handle names an immutable")),
    })
}
