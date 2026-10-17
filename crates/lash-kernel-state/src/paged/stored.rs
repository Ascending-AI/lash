//! Whole objects in and out of the heap: the heap's side of the arena's
//! projection (seam D, swap 2.4(a)). The machine decodes a proxy's one
//! chunk with [`PagedHeap::read_object`] and writes a dirty arena object
//! back with [`PagedHeap::store_object`], at projection points and before
//! an exact refinement.
//!
//! A store replaces the entity's head whole. Its accounting is the heap's
//! own (`ObjectHead::cost`) and is not reserved again: the machine held the
//! arena's growth ([`PagedHeap::hold_growth`]) before it made it, and the
//! store spends that hold.

use super::directory::{Body, Entry, ObjectHead, Ordinal, ordinal_key};
use super::heap_impl::{PagedHeap, census_fault};
use super::tree::{Cell, Tree};
use super::{HeapWriteError, NameRef, ObjectKind, SiteRef};
use lash_kernel_doc::heap::{EntityId, FamilyId, HeapFault, LogicalBytes, PageValue};

/// One object's whole contents, in page form.
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

impl PagedHeap {
    /// Takes the next primary identity: what an object the arena allocates
    /// is known by (swap 2.4(a); ids move to projection in 2.4(c)).
    ///
    /// # Errors
    ///
    /// [`HeapFault::Malformed`] when ordinals overflow.
    pub fn take_ordinal(&mut self) -> Result<EntityId, HeapFault> {
        let ordinal = self.next_ordinal();
        self.set_next_ordinal(
            ordinal
                .checked_add(1)
                .ok_or_else(|| HeapFault::Malformed("identity ordinal overflow".into()))?,
        );
        Ok(EntityId {
            family: FamilyId::PRIMARY,
            ordinal,
        })
    }

    /// The whole contents of `entity`, or `None` when the heap holds no
    /// such entity.
    ///
    /// # Errors
    ///
    /// The page port's faults.
    pub fn read_object(&self, entity: EntityId) -> Result<Option<StoredObject>, HeapFault> {
        let Some(head) = self.head(entity)? else {
            return Ok(None);
        };
        let access = self.pages();
        Ok(Some(match head.body {
            Body::List { rows, .. } => {
                let mut values = Vec::with_capacity(rows.len() as usize);
                rows.visit(access, &mut |_, value| {
                    values.push(value.clone());
                    true
                })?;
                StoredObject::List(values)
            }
            Body::Table {
                kind, rows, next, ..
            } => {
                let mut entries = Vec::with_capacity(rows.len() as usize);
                let mut malformed = false;
                rows.visit(access, &mut |key, entry| {
                    let Ok(bytes) = <[u8; 8]>::try_from(key) else {
                        malformed = true;
                        return false;
                    };
                    entries.push((
                        u64::from_be_bytes(bytes),
                        entry.key.clone(),
                        entry.value.clone(),
                    ));
                    true
                })?;
                if malformed {
                    return Err(HeapFault::Malformed("a table row key is no ordinal".into()));
                }
                StoredObject::Table {
                    kind,
                    entries,
                    next,
                }
            }
            Body::Closure { site, captures } => {
                let mut held = Vec::with_capacity(captures.len() as usize);
                captures.visit(access, &mut |_, capture| {
                    held.push(capture.clone());
                    true
                })?;
                StoredObject::Closure {
                    site: Box::new(site),
                    captures: held,
                }
            }
            Body::Variable(value) => StoredObject::Variable(value),
        }))
    }

    /// Replaces the contents of `entity` with `object`, or installs it
    /// when the heap does not hold it yet.
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
        let after = head.cost();
        let closure = head.kind() == ObjectKind::Closure;
        let tasks = head.task_edges();
        let taken = self.take_working(entity)?;
        let before = taken.as_ref().map_or(0, |taken| taken.cost);
        let (old_closures, old_tasks) = taken
            .as_ref()
            .and_then(|taken| taken.owned)
            .map_or((0, 0), |(closure, tasks)| (u64::from(closure), tasks));
        self.population.closure_entities = self
            .population
            .closure_entities
            .checked_sub(old_closures)
            .and_then(|v| v.checked_add(u64::from(closure)))
            .ok_or_else(|| HeapFault::Malformed("closure population overflow".into()))?;
        self.population.task_edges = self
            .population
            .task_edges
            .checked_sub(old_tasks)
            .and_then(|v| v.checked_add(tasks))
            .ok_or_else(|| HeapFault::Malformed("task-edge population overflow".into()))?;
        if taken.is_some() {
            self.edit_working(entity, |current, _| {
                *current = head;
                Ok(())
            })?;
        } else {
            self.insert_working(entity, head)?;
        }
        self.header = None;
        let credit = self.spend_held(after.saturating_sub(before));
        if let Some(census) = &mut self.census {
            census
                .changed(LogicalBytes(credit), LogicalBytes(0))
                .map_err(census_fault)?;
            if before == 0 {
                census.added(LogicalBytes(after), 1).map_err(census_fault)?;
            } else {
                census
                    .changed(LogicalBytes(before), LogicalBytes(after))
                    .map_err(census_fault)?;
            }
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
                let mut rows = Tree::new(true);
                let next = values.len() as u64;
                for (at, value) in values.into_iter().enumerate() {
                    let value = self.canonical(value)?;
                    rows.write(ordinal_key(at as u64), value, self.pages())?;
                }
                Body::List { rows, next }
            }
            StoredObject::Table {
                kind,
                entries,
                next,
            } => {
                let mut lookup: Tree<Ordinal> = Tree::new(false);
                let mut rows = Tree::new(true);
                for (seq, key, value) in entries {
                    let key = self.canonical(key)?;
                    let value = self.canonical(value)?;
                    let normal = self.normal_key(&key)?.ok_or_else(|| {
                        HeapFault::Malformed("a stored entry's key is not a legal key".into())
                    })?;
                    lookup.write(
                        super::heap_impl::normal_bytes(normal),
                        Ordinal(seq),
                        self.pages(),
                    )?;
                    rows.write(ordinal_key(seq), Entry { key, value }, self.pages())?;
                }
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
