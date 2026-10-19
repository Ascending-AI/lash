//! Objects in and out of the heap: the heap's side of the arena's
//! projection (seam D). The machine opens a proxy's head with
//! [`PagedHeap::read_head_of`], decodes one chunk of a large list or table
//! with [`PagedHeap::read_list_rows`] or [`PagedHeap::read_table_rows`],
//! and writes back at projection points and before an exact refinement: a
//! fresh object whole with [`PagedHeap::store_object`], an object the heap
//! already holds by its dirty cards with [`PagedHeap::store_cards`], so a
//! point write re-encodes one leaf and its index path.
//!
//! A store prices nothing: the machine's capacity meter counted the
//! arena's growth when it was made (FIG-6791). A store counts a fresh
//! entity into the census.

use super::directory::{Body, Entry, ObjectHead, Ordinal, ordinal_key};
use super::heap_impl::{PagedHeap, census_fault, normal_bytes};
use super::tree::{Cell, Tree};
use super::working::Taken;
use super::{HeapWriteError, NameRef, ObjectKind, SiteRef};
use lash_kernel_doc::heap::{EntityId, FamilyId, HeapFault, NormalKey, PageValue};

/// The values one card of a list holds, and the insertion sequences one
/// card of a table spans: the granularity of the arena's dirty worklist.
pub const CARD: u64 = 64;

/// A fresh object's whole contents, in page form.
#[derive(Clone, Debug)]
pub enum StoredObject {
    List(Vec<PageValue>),
    /// A map's, set's or record's entries in insertion order, each with
    /// its insertion sequence (`K-ITER-003`), and the sequence the next new
    /// entry takes. A set's values are null; a record's keys are texts.
    Table {
        kind: ObjectKind,
        entries: Vec<(u64, PageValue, PageValue)>,
        next: u64,
    },
    Closure {
        site: Box<SiteRef>,
        captures: Vec<(NameRef, EntityId)>,
    },
    Variable(PageValue),
}

/// What an object's head says before any of its rows are read: what
/// opening a proxy reads.
#[derive(Clone, Debug)]
pub enum StoredHead {
    List {
        len: u64,
    },
    Table {
        kind: ObjectKind,
        len: u64,
        next: u64,
    },
    /// A closure or a shared variable, whose head is its whole contents.
    Small(StoredObject),
}

/// A stored table entry: its sequence, key and value.
pub type StoredEntry = (u64, PageValue, PageValue);

/// The dirty cards of an object the heap already holds, in page form: each
/// card number with the contents it holds now.
#[derive(Clone, Debug)]
pub enum StoredCards {
    /// A list of `len` values; card `c` holds positions `c * CARD ..`, up
    /// to the length.
    List {
        len: u64,
        cards: Vec<(u64, Vec<PageValue>)>,
    },
    /// A table of `len` entries whose next entry takes sequence `next`;
    /// card `c` holds the entries whose sequences are `c * CARD ..`.
    Table {
        len: u64,
        next: u64,
        cards: Vec<(u64, Vec<StoredEntry>)>,
    },
}

fn malformed(problem: &str) -> HeapFault {
    HeapFault::Malformed(problem.into())
}

impl PagedHeap {
    /// The head of `entity` without its rows, or `None` when the heap
    /// holds no such entity.
    ///
    /// # Errors
    ///
    /// The page port's faults.
    pub fn read_head_of(&self, entity: EntityId) -> Result<Option<StoredHead>, HeapFault> {
        let Some(head) = self.current_head(entity)? else {
            return Ok(None);
        };
        let access = self.pages();
        Ok(Some(match &head.body {
            Body::List { rows, .. } => StoredHead::List { len: rows.len() },
            Body::Table {
                kind, rows, next, ..
            } => StoredHead::Table {
                kind: *kind,
                len: rows.len(),
                next: *next,
            },
            Body::Closure { site, captures } => {
                let mut held = Vec::with_capacity(captures.len() as usize);
                captures.visit(access, &mut |_, capture| {
                    held.push(capture.clone());
                    true
                })?;
                StoredHead::Small(StoredObject::Closure {
                    site: Box::new(site.clone()),
                    captures: held,
                })
            }
            Body::Variable(value) => StoredHead::Small(StoredObject::Variable(value.clone())),
        }))
    }

    /// Values `start ..` of the list `entity`, at most `count`: one chunk,
    /// read by rank through its index path.
    ///
    /// # Errors
    ///
    /// The page port's faults, and an entity that is no list.
    pub fn read_list_rows(
        &self,
        entity: EntityId,
        start: u64,
        count: u64,
    ) -> Result<Vec<PageValue>, HeapFault> {
        self.read_head(entity, |head| {
            let Some(Body::List { rows, .. }) = head.map(|head| &head.body) else {
                return Err(malformed("a list chunk names no list"));
            };
            let end = start.saturating_add(count).min(rows.len());
            let mut values = Vec::with_capacity(end.saturating_sub(start) as usize);
            for rank in start..end {
                let (_, value) = rows
                    .rank_get(rank, self.pages())?
                    .ok_or_else(|| malformed("a list rank is missing"))?;
                values.push(value);
            }
            Ok(values)
        })
    }

    /// The entries of the table `entity` whose insertion sequences are in
    /// `from .. to`, in order, each with its sequence: one chunk, read
    /// through its index path.
    ///
    /// # Errors
    ///
    /// The page port's faults, and an entity that is no table.
    pub fn read_table_rows(
        &self,
        entity: EntityId,
        from: u64,
        to: u64,
    ) -> Result<Vec<StoredEntry>, HeapFault> {
        self.read_head(entity, |head| {
            let Some(Body::Table { rows, .. }) = head.map(|head| &head.body) else {
                return Err(malformed("a table chunk names no table"));
            };
            table_rows(rows, self.pages(), from, to)
        })
    }

    /// The insertion sequence of the entry the table `entity` keys by
    /// `normal`, if it has one: one path of its key index.
    ///
    /// # Errors
    ///
    /// The page port's faults, and an entity that is no table.
    pub fn table_lookup(
        &self,
        entity: EntityId,
        normal: &NormalKey,
    ) -> Result<Option<u64>, HeapFault> {
        self.read_head(entity, |head| {
            let Some(Body::Table { lookup, .. }) = head.map(|head| &head.body) else {
                return Err(malformed("a key lookup names no table"));
            };
            Ok(lookup
                .get(&normal_bytes(normal.clone()), self.pages())?
                .map(|Ordinal(seq)| seq))
        })
    }

    /// Installs the fresh object `entity` whole, or replaces the contents
    /// of a closure or a shared variable.
    ///
    /// # Errors
    ///
    /// The page port's faults, and a census fault.
    pub fn store_object(
        &mut self,
        entity: EntityId,
        object: StoredObject,
    ) -> Result<(), HeapWriteError> {
        let head = self.stored_head(entity, object)?;
        let taken = self.take_working(entity)?;
        let (closure, tasks) = (head.kind() == ObjectKind::Closure, head.task_edges());
        if taken.is_some() {
            self.edit_working(entity, |current, _| {
                *current = head;
                Ok(())
            })?;
        } else {
            self.insert_working(entity, head)?;
        }
        self.settle(entity, taken, closure, tasks)
    }

    /// Writes the dirty cards of a list or a table the heap holds into its
    /// rows: each card's rows are rewritten, and rows a card no longer
    /// holds are removed, so only the leaves the cards address and their
    /// index paths change. A table's key index changes only for entries
    /// added or removed.
    ///
    /// # Errors
    ///
    /// The page port's faults, a census fault, and cards that disagree
    /// with the object's length.
    pub fn store_cards(
        &mut self,
        entity: EntityId,
        cards: StoredCards,
    ) -> Result<(), HeapWriteError> {
        let Some(base) = self.current_head(entity)? else {
            return Err(malformed("stored cards name no object").into());
        };
        let edit = match cards {
            StoredCards::List { len, cards } => {
                let Body::List { rows, next } = &base.body else {
                    return Err(malformed("list cards name another object").into());
                };
                if *next != rows.len() {
                    return Err(malformed("a list's rows are not positional").into());
                }
                let mut writes = Vec::new();
                let mut removes = Vec::new();
                for (card, values) in cards {
                    let start = card * CARD;
                    let held = values.len() as u64;
                    for (at, value) in values.into_iter().enumerate() {
                        writes.push((start + at as u64, self.canonical(value)?));
                    }
                    for position in (start + held).max(len)..(start + CARD).min(rows.len()) {
                        removes.push(position);
                    }
                }
                Edit::List {
                    len,
                    writes,
                    removes,
                }
            }
            StoredCards::Table { len, next, cards } => {
                let Body::Table {
                    rows,
                    next: base_next,
                    ..
                } = &base.body
                else {
                    return Err(malformed("table cards name another object").into());
                };
                let mut writes = Vec::new();
                let mut removes = Vec::new();
                let mut inserts = Vec::new();
                for (card, entries) in cards {
                    let held: Vec<u64> = entries.iter().map(|(seq, _, _)| *seq).collect();
                    let base_rows = table_rows(rows, self.pages(), card * CARD, (card + 1) * CARD)?;
                    for (seq, key, _) in base_rows {
                        if held.binary_search(&seq).is_err() {
                            removes.push((seq, self.key(&key)?));
                        }
                    }
                    for (seq, key, value) in entries {
                        let key = self.canonical(key)?;
                        if seq >= *base_next {
                            inserts.push((seq, self.key(&key)?));
                        }
                        writes.push((seq, key, self.canonical(value)?));
                    }
                }
                Edit::Table {
                    len,
                    next,
                    writes,
                    removes,
                    inserts,
                }
            }
        };
        drop(base);
        let taken = self.take_working(entity)?;
        let tasks = self.edit_working(entity, |head, access| {
            match (&mut head.body, edit) {
                (
                    Body::List { rows, next },
                    Edit::List {
                        len,
                        writes,
                        removes,
                    },
                ) => {
                    for position in removes {
                        rows.remove(&ordinal_key(position), access)?;
                    }
                    for (position, value) in writes {
                        rows.write(ordinal_key(position), value, access)?;
                    }
                    if rows.len() != len {
                        return Err(malformed("a list's dirty cards miss a change"));
                    }
                    *next = len;
                }
                (
                    Body::Table {
                        lookup, rows, next, ..
                    },
                    Edit::Table {
                        len,
                        next: arena_next,
                        writes,
                        removes,
                        inserts,
                    },
                ) => {
                    for (seq, normal) in removes {
                        rows.remove(&ordinal_key(seq), access)?;
                        lookup.remove(&normal, access)?;
                    }
                    for (seq, key, value) in writes {
                        rows.write(ordinal_key(seq), Entry { key, value }, access)?;
                    }
                    for (seq, normal) in inserts {
                        lookup.write(normal, Ordinal(seq), access)?;
                    }
                    if rows.len() != len || lookup.len() != len {
                        return Err(malformed("a table's dirty cards miss a change"));
                    }
                    *next = arena_next;
                }
                _ => return Err(malformed("stored cards changed kind")),
            }
            Ok(head.task_edges())
        })?;
        self.settle(entity, taken, false, tasks)
    }

    /// Settles the census and the populations after the head of `entity`
    /// replaced the one `taken` describes.
    fn settle(
        &mut self,
        entity: EntityId,
        taken: Option<Taken>,
        closure: bool,
        tasks: u64,
    ) -> Result<(), HeapWriteError> {
        let added = taken.is_none();
        let (old_closures, old_tasks) = taken
            .as_ref()
            .and_then(|taken| taken.owned)
            .map_or((0, 0), |(closure, tasks)| (u64::from(closure), tasks));
        self.population.closure_entities = self
            .population
            .closure_entities
            .checked_sub(old_closures)
            .and_then(|v| v.checked_add(u64::from(closure)))
            .ok_or_else(|| malformed("closure population overflow"))?;
        self.population.task_edges = self
            .population
            .task_edges
            .checked_sub(old_tasks)
            .and_then(|v| v.checked_add(tasks))
            .ok_or_else(|| malformed("task-edge population overflow"))?;
        self.header = None;
        if added && let Some(census) = &mut self.census {
            census.added(1).map_err(census_fault)?;
            self.accounting = census.certificate();
        }
        if entity.family == FamilyId::PRIMARY && entity.ordinal >= self.next_ordinal() {
            self.set_next_ordinal(entity.ordinal + 1);
        }
        Ok(())
    }

    fn stored_head(
        &mut self,
        entity: EntityId,
        object: StoredObject,
    ) -> Result<ObjectHead, HeapWriteError> {
        let body = match object {
            StoredObject::List(values) => {
                let next = values.len() as u64;
                let rows = values
                    .into_iter()
                    .enumerate()
                    .map(|(at, value)| Ok((ordinal_key(at as u64), self.canonical(value)?)))
                    .collect::<Result<Vec<_>, HeapWriteError>>()?;
                let rows = Tree::nested_from_rows(rows, true, self.pages())?;
                Body::List { rows, next }
            }
            StoredObject::Table {
                kind,
                entries,
                next,
            } => {
                let mut lookup = Vec::with_capacity(entries.len());
                let mut rows = Vec::with_capacity(entries.len());
                for (seq, key, value) in entries {
                    let key = self.canonical(key)?;
                    let value = self.canonical(value)?;
                    let normal = self
                        .normal_key(&key)?
                        .ok_or_else(|| malformed("a stored entry's key is not a legal key"))?;
                    lookup.push((normal_bytes(normal), Ordinal(seq)));
                    rows.push((ordinal_key(seq), Entry { key, value }));
                }
                let lookup: Tree<Ordinal> = Tree::nested_from_rows(lookup, false, self.pages())?;
                let rows = Tree::nested_from_rows(rows, true, self.pages())?;
                Body::Table {
                    kind,
                    lookup,
                    rows,
                    next,
                }
            }
            StoredObject::Closure { site, captures } => {
                let mut rows = Tree::new(false);
                for (name, id) in captures {
                    let key = name.key(self.pages())?;
                    rows.write(key, (name, id), self.pages())?;
                }
                Body::Closure {
                    site: *site,
                    captures: rows,
                }
            }
            StoredObject::Variable(value) => Body::Variable(self.canonical(value)?),
        };
        Ok(ObjectHead {
            body,
            family: entity.family,
        })
    }
}

/// What [`PagedHeap::store_cards`] changes in a head, with every value in
/// page form and every key normalized before the head is edited.
enum Edit {
    List {
        len: u64,
        writes: Vec<(u64, PageValue)>,
        removes: Vec<u64>,
    },
    Table {
        len: u64,
        next: u64,
        writes: Vec<StoredEntry>,
        removes: Vec<(u64, Vec<u8>)>,
        inserts: Vec<(u64, Vec<u8>)>,
    },
}

/// The rows of a table whose sequences are in `from .. to`.
fn table_rows(
    rows: &Tree<Entry>,
    access: &super::tree::Access<'_>,
    from: u64,
    to: u64,
) -> Result<Vec<StoredEntry>, HeapFault> {
    let mut out = Vec::new();
    let mut after = from.checked_sub(1).map(ordinal_key);
    while let Some((key, entry)) = rows.after(after.as_deref(), access)? {
        let seq = u64::from_be_bytes(
            <[u8; 8]>::try_from(key.as_slice())
                .map_err(|_| malformed("a table row key is no ordinal"))?,
        );
        if seq >= to {
            break;
        }
        out.push((seq, entry.key, entry.value));
        after = Some(key);
    }
    Ok(out)
}

use lash_kernel_doc::heap::PageReader as _;

/// Reads of page-form immutables through page access alone: what admission
/// and materialization decode a cold payload with.
pub mod reads {
    use std::sync::Arc;

    use super::super::directory::tuple_tree;
    use super::super::heap_impl::binary_summary;
    use super::super::payload;
    use super::super::tree::{Access, Tree};
    use lash_kernel_doc::Integer;
    use lash_kernel_doc::heap::{
        BytesRef, ErrorRef, HeapFault, IntRef, LogicalBytes, PageValue, Payload, TextRef, TupleRef,
    };
    use lash_kernel_doc::heap_values::integer_from_digits;

    /// A text's UTF-8, whole.
    ///
    /// # Errors
    ///
    /// The page port's faults.
    pub fn text(access: &Access<'_>, text: &TextRef) -> Result<Arc<str>, HeapFault> {
        match &text.body {
            Payload::Resident(text) => Ok(Arc::clone(text)),
            Payload::Paged(page) => {
                let bytes = payload::copy(access, **page, text.summary, true)?;
                String::from_utf8(bytes)
                    .map(Arc::from)
                    .map_err(|error| HeapFault::Malformed(error.to_string()))
            }
        }
    }

    /// Bytes, whole.
    ///
    /// # Errors
    ///
    /// The page port's faults.
    pub fn bytes(access: &Access<'_>, bytes: &BytesRef) -> Result<Arc<[u8]>, HeapFault> {
        match &bytes.body {
            Payload::Resident(bytes) => Ok(Arc::clone(bytes)),
            Payload::Paged(page) => {
                Ok(payload::copy(access, **page, binary_summary(bytes.len), false)?.into())
            }
        }
    }

    /// An integer, whole.
    ///
    /// # Errors
    ///
    /// The page port's faults, and a summary its digits contradict.
    pub fn int(access: &Access<'_>, int: &IntRef) -> Result<Integer, HeapFault> {
        match int {
            IntRef::Inline(int) => Ok(int.clone()),
            IntRef::Paged {
                summary, digits, ..
            } => {
                let len = summary.bits.div_ceil(8);
                let out = payload::copy(access, **digits, binary_summary(len), false)?;
                let int = integer_from_digits(summary.negative, &out);
                if int.bits() != summary.bits || int.is_negative() != summary.negative {
                    return Err(HeapFault::Corrupt {
                        page: digits.id,
                        reason: "integer summary mismatch".into(),
                    });
                }
                Ok(int)
            }
        }
    }

    /// An error's data.
    ///
    /// # Errors
    ///
    /// The page port's faults.
    pub fn error_data(access: &Access<'_>, error: &ErrorRef) -> Result<PageValue, HeapFault> {
        match &error.data {
            Payload::Resident(value) => Ok((**value).clone()),
            Payload::Paged(page) => {
                let cost = error
                    .cost
                    .0
                    .checked_sub(error.kind.summary.utf8_bytes)
                    .and_then(|v| v.checked_sub(error.message.summary.utf8_bytes))
                    .ok_or_else(|| HeapFault::Malformed("error cost summary mismatch".into()))?;
                let tree = Tree::<PageValue>::open(Some(**page), 1, LogicalBytes(cost), true)
                    .with_task_edges(error.task_edges);
                let tree = match error.family {
                    Some(family) => tree.in_family(family),
                    None => tree,
                };
                tree.rank_get(0, access)?
                    .map(|(_, value)| value)
                    .ok_or_else(|| HeapFault::Malformed("error data page is empty".into()))
            }
        }
    }

    /// A tuple's members, in order.
    ///
    /// # Errors
    ///
    /// The page port's faults.
    pub fn tuple(access: &Access<'_>, tuple: &TupleRef) -> Result<Vec<PageValue>, HeapFault> {
        match &tuple.body {
            Payload::Resident(values) => Ok(values.to_vec()),
            Payload::Paged(page) => {
                let mut values = Vec::with_capacity(tuple.summary.len as usize);
                tuple_tree(tuple, **page).visit(access, &mut |_, value| {
                    values.push(value.clone());
                    true
                })?;
                Ok(values)
            }
        }
    }
}

/// Page forms built through page access alone: what projection writes an
/// arena immutable as. Each payload past the inline bound goes to payload
/// pages, as the heap's own builders place it.
pub mod pages {
    use std::sync::Arc;

    use super::super::directory::{ordinal_key, value_cost, value_has_edges};
    use super::super::heap_impl::value_depth;
    use super::super::tree::{Access, SealWork, Tree, checked_sum};
    use super::super::{SealBudget, payload};
    use lash_kernel_doc::Integer;
    use lash_kernel_doc::graph_tokens::encode_body;
    use lash_kernel_doc::heap::{
        BytesRef, ErrorRef, HeapFault, IMMUTABLE_INLINE_MAX, IntRef, IntSummary, LogicalBytes,
        PageValue, Payload, TextRef, TupleRef, TupleSummary,
    };
    use lash_kernel_doc::heap_values::{integer_digits, task_count, text_summary};

    /// A text: inline when short, else payload pages.
    ///
    /// # Errors
    ///
    /// The page port's faults.
    pub fn text(access: &Access<'_>, text: &str) -> Result<TextRef, HeapFault> {
        Ok(TextRef {
            normal: Default::default(),
            summary: text_summary(text),
            body: if text.len() as u64 <= IMMUTABLE_INLINE_MAX {
                Payload::Resident(Arc::from(text))
            } else {
                Payload::paged(payload::build(access, text.as_bytes(), true)?)
            },
        })
    }

    /// Bytes: inline when short, else payload pages.
    ///
    /// # Errors
    ///
    /// The page port's faults.
    pub fn bytes(access: &Access<'_>, bytes: &[u8]) -> Result<BytesRef, HeapFault> {
        Ok(BytesRef {
            normal: Default::default(),
            len: bytes.len() as u64,
            body: if bytes.len() as u64 <= IMMUTABLE_INLINE_MAX {
                Payload::Resident(Arc::from(bytes))
            } else {
                Payload::paged(payload::build(access, bytes, false)?)
            },
        })
    }

    /// An integer: inline when its magnitude is short, else digit pages.
    ///
    /// # Errors
    ///
    /// The page port's faults.
    pub fn int(access: &Access<'_>, int: Integer) -> Result<IntRef, HeapFault> {
        if int.bits().div_ceil(8) <= IMMUTABLE_INLINE_MAX {
            return Ok(IntRef::Inline(int));
        }
        let summary = IntSummary {
            bits: int.bits(),
            negative: int.is_negative(),
        };
        Ok(IntRef::Paged {
            normal: Default::default(),
            summary,
            digits: Arc::new(payload::build(access, &integer_digits(&int), false)?),
        })
    }

    fn sealed(tree: &mut Tree<PageValue>, access: &Access<'_>) -> Result<(), HeapFault> {
        tree.seal(
            access,
            &mut SealWork::new(SealBudget {
                pages: u64::MAX,
                bytes: u64::MAX,
            }),
        )?;
        Ok(())
    }

    /// A tuple of canonical `values`; its depth is the caller's to bound.
    ///
    /// # Errors
    ///
    /// The page port's faults.
    pub fn tuple(access: &Access<'_>, values: Vec<PageValue>) -> Result<Arc<TupleRef>, HeapFault> {
        let depth = values
            .iter()
            .map(|value| value_depth(value) + 1)
            .max()
            .unwrap_or(1);
        let has_edges = values.iter().any(value_has_edges);
        let task_edges = checked_sum(values.iter().map(task_count))?;
        let summary = TupleSummary {
            task_edges,
            len: values.len() as u64,
            cost: LogicalBytes(checked_sum(values.iter().map(value_cost))?),
            has_edges,
            depth,
        };
        let body = if encode_body(&values).len() <= IMMUTABLE_INLINE_MAX as usize {
            Payload::Resident(Arc::from(values))
        } else {
            let mut tree = Tree::new(true);
            for (i, value) in values.into_iter().enumerate() {
                tree.write(ordinal_key(i as u64), value, access)?;
            }
            sealed(&mut tree, access)?;
            Payload::paged(
                tree.page()
                    .ok_or_else(|| HeapFault::Malformed("nonempty tuple has no page".into()))?,
            )
        };
        Ok(Arc::new(TupleRef {
            normal: Default::default(),
            summary,
            body,
            family: None,
        }))
    }

    /// An error of `kind`, `message` and canonical `data`; its depth is the
    /// caller's to bound.
    ///
    /// # Errors
    ///
    /// The page port's faults.
    pub fn error(
        access: &Access<'_>,
        kind: TextRef,
        message: TextRef,
        data: PageValue,
    ) -> Result<Arc<ErrorRef>, HeapFault> {
        let cost = checked_sum(
            [
                kind.summary.utf8_bytes,
                message.summary.utf8_bytes,
                value_cost(&data),
            ]
            .into_iter(),
        )?;
        let depth = value_depth(&data) + 1;
        let task_edges = task_count(&data);
        let has_edges = value_has_edges(&data);
        let data = if encode_body(&data).len() <= IMMUTABLE_INLINE_MAX as usize {
            Payload::Resident(Arc::new(data))
        } else {
            let mut tree = Tree::new(true);
            tree.write(ordinal_key(0), data, access)?;
            sealed(&mut tree, access)?;
            Payload::paged(
                tree.page()
                    .ok_or_else(|| HeapFault::Malformed("error data has no page".into()))?,
            )
        };
        Ok(Arc::new(ErrorRef {
            family: None,
            task_edges,
            kind,
            message,
            data,
            depth,
            cost: LogicalBytes(cost),
            has_edges,
        }))
    }
}
