//! The identity-addressed paged semantic heap. Collection edits update one
//! bounded path and replace its directory head; no mutable container escapes.
use super::directory::{
    Body, Entry, ObjectHead, Ordinal, entity_bytes, entity_key, ordinal_key, value_cost,
    value_has_edges,
};
use super::tree::{Access, Cell, SealWork, Tree, checked_sum};
use super::*;
use lash_kernel_doc::Integer;
use lash_kernel_doc::graph_tokens::{TokenCodec, TokenWriter, encode_body};
use lash_kernel_doc::heap::*;
use lash_kernel_doc::heap_values::{integer_from_digits, integral_float, key_digest, text_summary};
use std::cell::RefCell;
use std::ops::{ControlFlow, Range};
use std::sync::Arc;

pub struct PagedHeap {
    pub(super) access: Access<'static>,
    pub(super) limits: lash_kernel_doc::KernelDataLimits,
    pub(super) directory: Tree<ObjectHead>,
    pub(super) families: Tree<FamilyRecord>,
    next_ordinal: u64,
    next_family: u64,
    epoch: EditEpoch,
    pub(super) header: Option<PageRef>,
    /// The accounting and population `header` records: a header sealed
    /// before either changed is not this heap's (FIG-6318).
    header_state: Option<(AccountingCertificate, DataOnlyCertificate)>,
    pub(super) accounting: AccountingCertificate,
    pub(super) population: DataOnlyCertificate,
    pub(super) family_base: RefCell<Option<Box<dyn FamilyBase>>>,
    pub(super) census: Option<Box<dyn AllocationCensus>>,
    /// The execution roots, installed only through publication.
    pub(super) session: Tree<BindingRecord>,
    pub(super) tasks: Tree<super::directory::execution::TaskRecord>,
    pub(super) joins: Tree<ListJoinRecord>,
    /// What the machine holds outside these trees.
    pub(super) holdings: super::publication::Holdings,
}
/// The heap root a heap-header page holds: a keyed leaf of one current
/// header record.
fn header_root(page: PageRef, answer: &PageAnswer) -> Result<HeapRoot, HeapFault> {
    let corrupt = |reason: String| HeapFault::Corrupt {
        page: page.id,
        reason,
    };
    if answer.kind != PageKind::KEYED_LEAF {
        return Err(corrupt("expected heap header".into()));
    }
    let mut input = lash_kernel_doc::graph_tokens::TokenReader::new(&answer.tokens);
    let (variant, fields) = input.sum().map_err(|e| corrupt(e.to_string()))?;
    if variant != 0 || fields != 1 {
        return Err(corrupt("superseded heap header".into()));
    }
    let root = HeapRoot::decode(&mut input).map_err(|e| corrupt(e.to_string()))?;
    input.finish().map_err(|e| corrupt(e.to_string()))?;
    Ok(root)
}

/// The persisted accounting of the heap whose header is `page` (S7): what
/// an embedder restores its census from before it opens a root or a
/// carried session over that heap. One page read.
///
/// # Errors
///
/// The source's faults, and [`HeapFault::Corrupt`] for a page that is not
/// a heap header.
pub fn heap_accounting(
    source: &mut dyn PageSource,
    page: PageRef,
) -> Result<AccountingCertificate, HeapFault> {
    let answers = source.read(&[page])?;
    let [answer] = answers.as_slice() else {
        return Err(HeapFault::Malformed(
            "a read answered the wrong count".into(),
        ));
    };
    Ok(header_root(page, answer)?.accounting)
}

impl PagedHeap {
    /// Opens the one heap-header page; directory and payloads stay lazy.
    pub fn open_header(
        page: PageRef,
        source: Box<dyn PageSource>,
        epoch: EditEpoch,
    ) -> Result<Self, HeapFault> {
        let access = Access::new(source);
        let answer = access.read(page)?;
        let mut root = header_root(page, &answer)?;
        if let Some(p) = root.directory.page_mut() {
            super::tree::grant(p, &answer.children)?;
        }
        if let Some(p) = root.families.page_mut() {
            super::tree::grant(p, &answer.children)?;
        }
        let directory = Tree::of_keyed(&root.directory);
        let families = Tree::of_keyed(&root.families);
        Ok(Self {
            access,
            limits: lash_kernel_doc::KernelDataLimits::standard(),
            directory,
            families,
            next_ordinal: root.next_ordinal,
            next_family: root.next_family,
            epoch,
            header: Some(page),
            header_state: Some((root.accounting, root.population)),
            accounting: root.accounting,
            population: root.population,
            family_base: RefCell::new(None),
            census: None,
            session: Tree::new(false),
            tasks: Tree::new(false),
            joins: Tree::new(false),
            holdings: super::publication::Holdings::opened(root.next_ordinal, root.next_family),
        })
    }
    pub fn set_data_limits(&mut self, limits: lash_kernel_doc::KernelDataLimits) {
        self.limits = limits;
    }
    /// The machine installs the accounting lane's admitted census and bound.
    pub fn set_census(&mut self, census: Box<dyn AllocationCensus>) -> Result<(), HeapFault> {
        if census.certificate() != self.accounting {
            return Err(HeapFault::Malformed(
                "census certificate differs from heap header".into(),
            ));
        }
        self.census = Some(census);
        Ok(())
    }
    /// The page port the machine's own record trees read and edit through:
    /// names, sites, task identities, frames and waits. The heap's
    /// directory, families and installed roots are not reachable through
    /// it; they change only through the heap's operations.
    pub fn pages(&self) -> &Access<'static> {
        &self.access
    }
    pub fn data_only_certificate(&self) -> DataOnlyCertificate {
        self.population
    }
    pub fn certificate(&self) -> AccountingCertificate {
        self.accounting
    }
    pub fn next_family(&mut self) -> Result<FamilyId, HeapFault> {
        let id = FamilyId(self.next_family);
        self.next_family = self
            .next_family
            .checked_add(1)
            .ok_or_else(|| HeapFault::Malformed("family allocator overflow".into()))?;
        self.header = None;
        Ok(id)
    }
    /// Installs the lifecycle's lazy-family reader. It is called only after
    /// current membership has been proved by that reader.
    pub fn set_family_base(&mut self, base: Box<dyn FamilyBase>) {
        *self.family_base.borrow_mut() = Some(base);
    }
    pub fn epoch(&self) -> EditEpoch {
        self.epoch
    }
    /// The next ordinal of the primary family.
    pub fn next_ordinal(&self) -> u64 {
        self.next_ordinal
    }
    /// The sealed heap's root.
    ///
    /// # Errors
    ///
    /// [`HeapFault::Malformed`] while the directory or the family table
    /// holds unsealed rows.
    pub fn heap_root(&self) -> Result<HeapRoot, HeapFault> {
        Ok(HeapRoot {
            directory: keyed(&self.directory)?,
            families: keyed(&self.families)?,
            next_ordinal: self.next_ordinal,
            next_family: self.next_family,
            accounting: self.accounting,
            population: self.population,
        })
    }
    pub(super) fn head(&self, entity: EntityId) -> Result<Option<ObjectHead>, HeapFault> {
        let family = if entity.family == FamilyId::PRIMARY {
            None
        } else {
            let Some(family) = self
                .families
                .get(&entity.family.0.to_be_bytes(), &self.access)?
            else {
                return Ok(None);
            };
            if entity.ordinal >= family.extent {
                return Err(HeapFault::Corrupt {
                    page: family.base.root.id,
                    reason: "relative occurrence is outside its family extent".into(),
                });
            }
            if !self.family_member(&family, entity.ordinal)? {
                return Ok(None);
            }
            Some(family)
        };
        if let Some(head) = self.directory.get(&entity_bytes(entity), &self.access)? {
            return Ok(Some(head));
        }
        let Some(family) = family else {
            return Ok(None);
        };
        let mut base = self.family_base.borrow_mut();
        let base = base
            .as_mut()
            .ok_or_else(|| HeapFault::Malformed("lazy family reader is not installed".into()))?;
        let head = base
            .base_object(entity, &family.base, &self.access)?
            .ok_or_else(|| {
                HeapFault::Malformed("a live family entity has no frozen body".into())
            })?;
        if head.family != entity.family {
            return Err(HeapFault::Malformed(
                "frozen head has another family context".into(),
            ));
        }
        Ok(Some(head))
    }
    pub(super) fn read_head<R>(
        &self,
        entity: EntityId,
        read: impl FnOnce(Option<&ObjectHead>) -> Result<R, HeapFault>,
    ) -> Result<R, HeapFault> {
        if entity.family == FamilyId::PRIMARY {
            self.directory
                .get_with(&entity_bytes(entity), &self.access, read)?
        } else {
            let head = self.head(entity)?;
            read(head.as_ref())
        }
    }
    fn required(&self, entity: EntityId, kind: ObjectKind) -> Result<ObjectHead, HeapWriteError> {
        match self.head(entity)? {
            Some(head) if head.kind() == kind => Ok(head),
            _ => Err(raised("type_error", "the object has another kind")),
        }
    }
    fn publish(&mut self, entity: EntityId, head: ObjectHead) -> Result<(), HeapWriteError> {
        self.change_head(
            entity,
            head.cost(),
            head.kind() == ObjectKind::Closure,
            head.task_edges(),
            |heap| heap.directory.write(entity_key(entity), head, &heap.access),
        )
    }
    /// Reserve before lending a directory cell to its edit, so a memory
    /// refusal leaves it unchanged and nested trees retain unique ownership.
    fn change_head(
        &mut self,
        entity: EntityId,
        after: u64,
        closure: bool,
        tasks: u64,
        edit: impl FnOnce(&mut Self) -> Result<(), HeapFault>,
    ) -> Result<(), HeapWriteError> {
        let before = self.head(entity)?.map_or(0, |h| h.cost());
        let mut census = self
            .census
            .take()
            .ok_or_else(|| HeapFault::Malformed("edit has no census".into()))?;
        let credit = self.holdings.credit(after.saturating_sub(before));
        let decision = census.reserve(LogicalBytes(
            after.saturating_sub(before).saturating_sub(credit),
        ));
        if let ReservationDecision::NeedsPrecision(pending) = &decision
            && let Some(refusal) = self.refuses_precision(pending.bytes.0)
        {
            self.census = Some(census);
            return Err(refusal);
        }
        let result = (|| {
            census
                .changed(LogicalBytes(credit), LogicalBytes(0))
                .map_err(census_fault)?;
            self.holdings.spend(credit);
            let old = self.directory.get(&entity_bytes(entity), &self.access)?;
            let old_closures =
                old.as_ref()
                    .is_some_and(|h| h.kind() == ObjectKind::Closure) as u64;
            let old_tasks = old.as_ref().map_or(0, Cell::task_edges);
            self.population.closure_entities = self
                .population
                .closure_entities
                .checked_sub(old_closures)
                .and_then(|v| v.checked_add(closure as u64))
                .ok_or_else(|| HeapFault::Malformed("closure population overflow".into()))?;
            self.population.task_edges = self
                .population
                .task_edges
                .checked_sub(old_tasks)
                .and_then(|v| v.checked_add(tasks))
                .ok_or_else(|| HeapFault::Malformed("task-edge population overflow".into()))?;
            drop(old);
            edit(self)?;
            self.header = None;
            if matches!(decision, ReservationDecision::NeedsPrecision(_)) {
                return self.refine_installed(&mut *census, &[entity], 0);
            }
            if before == 0 {
                census.added(LogicalBytes(after), 1).map_err(census_fault)?;
            } else {
                census
                    .changed(LogicalBytes(before), LogicalBytes(after))
                    .map_err(census_fault)?;
            }
            Ok(())
        })();
        self.accounting = census.certificate();
        self.census = Some(census);
        result
    }
    fn make_head(&self, init: ObjectInit) -> Result<ObjectHead, HeapFault> {
        let body = match init {
            ObjectInit::List(values) => {
                let mut rows = Tree::new(true);
                let next = values.len() as u64;
                for (i, v) in values.into_iter().enumerate() {
                    rows.write(ordinal_key(i as u64), v, &self.access)?;
                }
                Body::List { rows, next }
            }
            ObjectInit::Map(values) => self.make_table(ObjectKind::Map, values)?,
            ObjectInit::Set(values) => self.make_table(
                ObjectKind::Set,
                values.into_iter().map(|v| (v, Value::Null)).collect(),
            )?,
            ObjectInit::Record(values) => self.make_table(
                ObjectKind::Record,
                values
                    .into_iter()
                    .map(|(k, v)| (Value::Text(k), v))
                    .collect(),
            )?,
            ObjectInit::Variable(value) => Body::Variable(value),
            ObjectInit::Closure { site, captures } => {
                let mut rows = Tree::new(false);
                for (name, id) in captures {
                    let key = name.key(&self.access)?;
                    if rows.get(&key, &self.access)?.is_some() {
                        return Err(HeapFault::Malformed(
                            "duplicate closure capture name".into(),
                        ));
                    }
                    rows.write(key, (name, id), &self.access)?;
                }
                Body::Closure {
                    site,
                    captures: rows,
                }
            }
        };
        Ok(ObjectHead {
            body,
            family: FamilyId::PRIMARY,
        })
    }
    fn make_table(&self, kind: ObjectKind, values: Vec<(Value, Value)>) -> Result<Body, HeapFault> {
        let mut lookup: Tree<Ordinal> = Tree::new(false);
        let mut rows = Tree::new(true);
        let mut next = 0;
        for (key, value) in values {
            let normal = self
                .key(&key)
                .map_err(|e| HeapFault::Malformed(e.to_string()))?;
            if let Some(Ordinal(ordinal)) = lookup.get(&normal, &self.access)? {
                let mut entry: Entry = rows
                    .get(&ordinal.to_be_bytes(), &self.access)?
                    .ok_or_else(|| HeapFault::Malformed("lookup points at no entry".into()))?;
                entry.value = value;
                rows.write(ordinal_key(ordinal), entry, &self.access)?;
            } else {
                lookup.write(normal, Ordinal(next), &self.access)?;
                rows.write(ordinal_key(next), Entry { key, value }, &self.access)?;
                next += 1;
            }
        }
        Ok(Body::Table {
            kind,
            lookup,
            rows,
            next,
        })
    }
    fn key(&self, value: &Value) -> Result<Vec<u8>, HeapWriteError> {
        self.normal_key(value)?.map(normal_bytes).ok_or_else(|| {
            raised(
                "invalid_key",
                "expected an immutable key; use ref for an object or task",
            )
        })
    }
    pub(super) fn canonical(&mut self, value: Value) -> Result<Value, HeapWriteError> {
        Ok(match value {
            Value::Function(t) => {
                let Value::Text(t) = self.canonical(Value::Text(t))? else {
                    unreachable!()
                };
                Value::Function(t)
            }
            Value::Text(t) => match &t.body {
                Payload::Resident(s) if s.len() as u64 > IMMUTABLE_INLINE_MAX => {
                    Value::Text(self.text(s).map_err(write_native)?)
                }
                _ => Value::Text(t),
            },
            Value::Bytes(b) => match &b.body {
                Payload::Resident(s) if s.len() as u64 > IMMUTABLE_INLINE_MAX => {
                    Value::Bytes(self.bytes(s).map_err(write_native)?)
                }
                _ => Value::Bytes(b),
            },
            Value::Int(IntRef::Inline(i)) if i.bits().div_ceil(8) > IMMUTABLE_INLINE_MAX => {
                Value::Int(self.int(i).map_err(write_native)?)
            }
            Value::Tuple(t) => match &t.body {
                Payload::Resident(v) => Value::Tuple(self.tuple(v.to_vec()).map_err(write_native)?),
                _ => Value::Tuple(t),
            },
            Value::Error(e) => {
                let mut e = (*e).clone();
                let Value::Text(kind) = self.canonical(Value::Text(e.kind))? else {
                    return Err(HeapFault::Malformed("error kind is not text".into()).into());
                };
                let Value::Text(message) = self.canonical(Value::Text(e.message))? else {
                    return Err(HeapFault::Malformed("error message is not text".into()).into());
                };
                e.kind = kind;
                e.message = message;
                if matches!(e.data, Payload::Paged(_)) {
                    return Ok(Value::Error(Arc::new(e)));
                }
                let data = match &e.data {
                    Payload::Resident(v) => self.canonical((**v).clone())?,
                    Payload::Paged(_) => self.error_data(&e)?,
                };
                e.depth = value_depth(&data) + 1;
                e.task_edges = lash_kernel_doc::heap_values::task_count(&data);
                e.family = None;
                if e.depth as usize > self.limits.value_depth() {
                    return Err(raised("too_deep", "error nesting exceeds K-VAL-034"));
                }
                e.cost = LogicalBytes(checked_sum(
                    [
                        e.kind.summary.utf8_bytes,
                        e.message.summary.utf8_bytes,
                        value_cost(&data),
                    ]
                    .into_iter(),
                )?);
                e.has_edges = value_has_edges(&data);
                e.data = if encode_body(&data).len() <= IMMUTABLE_INLINE_MAX as usize {
                    Payload::Resident(Arc::new(data))
                } else {
                    let mut tree = Tree::new(true);
                    tree.write(ordinal_key(0), data, &self.access)?;
                    tree.seal(
                        &self.access,
                        &mut SealWork::new(SealBudget {
                            pages: u64::MAX,
                            bytes: u64::MAX,
                        }),
                    )?;
                    Payload::Paged(
                        tree.page()
                            .ok_or_else(|| HeapFault::Malformed("error data has no page".into()))?,
                    )
                };
                Value::Error(Arc::new(e))
            }
            other => other,
        })
    }
    pub(super) fn copy_text(&self, text: &TextRef) -> Result<Arc<str>, HeapFault> {
        if let Payload::Resident(t) = &text.body {
            return Ok(Arc::clone(t));
        }
        let mut bytes = Vec::new();
        self.payload_visit(
            &Value::Text(text.clone()),
            0..text.summary.utf8_bytes,
            &mut |c| {
                if let Chunk::Text(t) = c {
                    bytes.extend_from_slice(t.as_bytes());
                }
                ControlFlow::Continue(())
            },
        )?;
        String::from_utf8(bytes)
            .map(Arc::from)
            .map_err(|e| HeapFault::Malformed(e.to_string()))
    }
    pub(super) fn copy_bytes(&self, bytes: &BytesRef) -> Result<Arc<[u8]>, HeapFault> {
        if let Payload::Resident(b) = &bytes.body {
            return Ok(Arc::clone(b));
        }
        let mut out = Vec::new();
        self.payload_visit(&Value::Bytes(bytes.clone()), 0..bytes.len, &mut |c| {
            if let Chunk::Bytes(b) = c {
                out.extend_from_slice(b);
            }
            ControlFlow::Continue(())
        })?;
        Ok(out.into())
    }
    pub(super) fn copy_int(&self, int: &IntRef) -> Result<Integer, HeapFault> {
        match int {
            IntRef::Inline(i) => Ok(i.clone()),
            IntRef::Paged {
                summary, digits, ..
            } => {
                let len = summary.bits.div_ceil(8);
                let mut out = Vec::new();
                self.payload_visit(&Value::Int(int.clone()), 0..len, &mut |chunk| {
                    if let Chunk::Bytes(bytes) = chunk {
                        out.extend_from_slice(bytes);
                    }
                    ControlFlow::Continue(())
                })?;
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
}
impl SemanticHeap for PagedHeap {
    fn fresh(source: Box<dyn PageSource>, epoch: EditEpoch) -> Self {
        Self {
            access: Access::new(source),
            limits: lash_kernel_doc::KernelDataLimits::standard(),
            directory: Tree::new(false),
            families: Tree::new(false),
            next_ordinal: 0,
            next_family: 1,
            epoch,
            header: None,
            header_state: None,
            population: DataOnlyCertificate {
                closure_entities: 0,
                task_edges: 0,
            },
            accounting: AccountingCertificate {
                metric: AccountingMetric::LocalCostV1,
                upper: LogicalBytes(0),
                entities: 0,
                refinements: 0,
            },
            family_base: RefCell::new(None),
            census: None,
            session: Tree::new(false),
            tasks: Tree::new(false),
            joins: Tree::new(false),
            holdings: super::publication::Holdings::opened(0, 1),
        }
    }
    fn open(
        root: &HeapRoot,
        source: Box<dyn PageSource>,
        epoch: EditEpoch,
    ) -> Result<Self, HeapFault> {
        Ok(Self {
            access: Access::new(source),
            limits: lash_kernel_doc::KernelDataLimits::standard(),
            directory: Tree::of_keyed(&root.directory),
            families: Tree::of_keyed(&root.families),
            next_ordinal: root.next_ordinal,
            next_family: root.next_family,
            epoch,
            header: None,
            header_state: None,
            accounting: root.accounting,
            population: root.population,
            family_base: RefCell::new(None),
            census: None,
            session: Tree::new(false),
            tasks: Tree::new(false),
            joins: Tree::new(false),
            holdings: super::publication::Holdings::opened(root.next_ordinal, root.next_family),
        })
    }
    fn canonical_value(&mut self, value: Value) -> Result<Value, HeapWriteError> {
        self.canonical(value)
    }
    fn kind(&self, entity: EntityId) -> Result<Option<ObjectKind>, HeapFault> {
        self.read_head(entity, |head| Ok(head.map(ObjectHead::kind)))
    }
    fn value_of(&self, entity: EntityId) -> Result<Option<Value>, HeapFault> {
        Ok(self.kind(entity)?.and_then(|k| {
            Some(match k {
                ObjectKind::List => Value::List(entity),
                ObjectKind::Map => Value::Map(entity),
                ObjectKind::Set => Value::Set(entity),
                ObjectKind::Record => Value::Record(entity),
                ObjectKind::Closure => Value::Closure(entity),
                ObjectKind::Variable => return None,
            })
        }))
    }
    fn variable_get(&self, variable: EntityId) -> Result<Value, HeapFault> {
        self.read_head(variable, |head| match head.map(|h| &h.body) {
            Some(Body::Variable(v)) => Ok(v.clone()),
            _ => Err(HeapFault::Malformed("expected a shared variable".into())),
        })
    }
    fn closure(&self, closure: EntityId) -> Result<ClosureView, HeapFault> {
        match self.head(closure)?.map(|h| h.body) {
            Some(Body::Closure { site, captures }) => Ok(ClosureView { site, captures }),
            _ => Err(HeapFault::Malformed("expected a closure".into())),
        }
    }
    fn table_after(
        &self,
        table: EntityId,
        after: Option<TableCursor>,
    ) -> Result<Option<(TableCursor, Value)>, HeapFault> {
        let Some(ObjectHead {
            body: Body::Table { rows, .. },
            ..
        }) = self.head(table)?
        else {
            return Ok(None);
        };
        let key = after.map(|a| ordinal_key(a.0));
        Ok(rows.after(key.as_deref(), &self.access)?.map(|(k, e)| {
            (
                TableCursor(u64::from_be_bytes(k.try_into().unwrap_or([0; 8]))),
                e.key,
            )
        }))
    }
    fn table_passed(&self, table: EntityId, last: Option<TableCursor>) -> Result<u64, HeapFault> {
        let Some(last) = last else {
            return Ok(0);
        };
        match self.head(table)?.map(|h| h.body) {
            Some(Body::Table { rows, .. }) => rows.passed(&last.0.to_be_bytes(), &self.access),
            _ => Ok(0),
        }
    }
    fn allocate_object(&mut self, init: ObjectInit) -> Result<EntityId, HeapWriteError> {
        let init = match init {
            ObjectInit::List(v) => ObjectInit::List(
                v.into_iter()
                    .map(|v| self.canonical(v))
                    .collect::<Result<_, _>>()?,
            ),
            ObjectInit::Map(v) => ObjectInit::Map(
                v.into_iter()
                    .map(|(k, v)| Ok((self.canonical(k)?, self.canonical(v)?)))
                    .collect::<Result<_, HeapWriteError>>()?,
            ),
            ObjectInit::Set(v) => ObjectInit::Set(
                v.into_iter()
                    .map(|v| self.canonical(v))
                    .collect::<Result<_, _>>()?,
            ),
            ObjectInit::Record(v) => ObjectInit::Record(
                v.into_iter()
                    .map(|(k, v)| {
                        let Value::Text(k) = self.canonical(Value::Text(k))? else {
                            return Err(
                                HeapFault::Malformed("record field is not text".into()).into()
                            );
                        };
                        Ok((k, self.canonical(v)?))
                    })
                    .collect::<Result<_, HeapWriteError>>()?,
            ),
            ObjectInit::Variable(v) => ObjectInit::Variable(self.canonical(v)?),
            other => other,
        };
        let head = self.make_head(init)?;
        let id = EntityId {
            family: FamilyId::PRIMARY,
            ordinal: self.next_ordinal,
        };
        let next = self
            .next_ordinal
            .checked_add(1)
            .ok_or_else(|| HeapFault::Malformed("identity ordinal overflow".into()))?;
        self.publish(id, head)?;
        self.next_ordinal = next;
        Ok(id)
    }
    fn list_write(
        &mut self,
        list: EntityId,
        index: u64,
        value: Value,
    ) -> Result<(), HeapWriteError> {
        let head = self.required(list, ObjectKind::List)?;
        if index > head.len() {
            return Err(raised(
                "index_out_of_range",
                "index exceeds the list length",
            ));
        }
        let value = self.canonical(value)?;
        let (old, cost, tasks) = match &head.body {
            Body::List { rows, next } => {
                let old = if index == rows.len() {
                    next.checked_add(1)
                        .ok_or_else(|| HeapFault::Malformed("list ordinal overflow".into()))?;
                    None
                } else {
                    Some(
                        rows.rank_get(index, &self.access)?
                            .ok_or_else(|| HeapFault::Malformed("list rank missing".into()))?
                            .1,
                    )
                };
                // Match the tree's checked aggregates before reservation. Add
                // the head's minimum only after the row delta, as cost() does.
                let cost = rows
                    .cost()
                    .checked_sub(old.as_ref().map_or(0, value_cost))
                    .and_then(|cost| cost.checked_add(value_cost(&value)))
                    .ok_or_else(|| HeapFault::Malformed("logical aggregate overflow".into()))?;
                let tasks = rows
                    .task_edges()
                    .checked_sub(old.as_ref().map_or(0, Cell::task_edges))
                    .and_then(|tasks| tasks.checked_add(value.task_edges()))
                    .ok_or_else(|| HeapFault::Malformed("logical aggregate overflow".into()))?;
                (old, cost, tasks)
            }
            _ => unreachable!("required list"),
        };
        let after = census::MIN_ENTITY_COST.0.saturating_add(cost);
        drop(head);
        if let Some(old) = &old {
            self.dropped(list, old);
        }
        self.change_head(list, after, false, tasks, |heap| {
            let mut value = Some(value);
            let mut update = |head: &mut ObjectHead| {
                let Body::List { rows, next } = &mut head.body else {
                    unreachable!("required list")
                };
                let key = if index == rows.len() {
                    let key = ordinal_key(*next);
                    *next += 1;
                    key
                } else {
                    rows.rank_get(index, &heap.access)?
                        .ok_or_else(|| HeapFault::Malformed("list rank missing".into()))?
                        .0
                };
                rows.write(
                    key,
                    value
                        .take()
                        .ok_or_else(|| HeapFault::Malformed("list edit repeated".into()))?,
                    &heap.access,
                )
            };
            let key = entity_key(list);
            if heap.directory.get(&key, &heap.access)?.is_some() {
                heap.directory
                    .update_existing(&key, &heap.access, &mut update)
            } else {
                // The first write to an immutable family occurrence creates
                // its sparse override; subsequent writes edit that owner.
                let mut head = heap
                    .head(list)?
                    .ok_or_else(|| HeapFault::Malformed("list disappeared".into()))?;
                update(&mut head)?;
                heap.directory.write(key, head, &heap.access)
            }
        })
    }
    fn list_remove(&mut self, list: EntityId, index: u64) -> Result<Value, HeapWriteError> {
        let mut head = self.required(list, ObjectKind::List)?;
        if index >= head.len() {
            return Err(raised(
                "index_out_of_range",
                "index exceeds the list length",
            ));
        }
        let value = match &mut head.body {
            Body::List { rows, .. } => {
                let (key, _) = rows
                    .rank_get(index, &self.access)?
                    .ok_or_else(|| HeapFault::Malformed("list rank missing".into()))?;
                rows.remove(&key, &self.access)?
                    .ok_or_else(|| HeapFault::Malformed("list row missing".into()))?
            }
            _ => return Err(HeapFault::Malformed("list descriptor kind mismatch".into()).into()),
        };
        self.dropped(list, &value);
        self.publish(list, head)?;
        Ok(value)
    }
    fn map_write(&mut self, map: EntityId, key: Value, value: Value) -> Result<(), HeapWriteError> {
        self.table_write(map, ObjectKind::Map, key, value)
    }
    fn set_write(
        &mut self,
        set: EntityId,
        member: Value,
        present: bool,
    ) -> Result<(), HeapWriteError> {
        self.required(set, ObjectKind::Set)?;
        if present {
            self.table_write(set, ObjectKind::Set, member, Value::Null)
        } else {
            self.table_remove(set, &member)
        }
    }
    fn table_remove(&mut self, table: EntityId, key: &Value) -> Result<(), HeapWriteError> {
        let normal = self.key(key)?;
        let Some(mut head) = self.head(table)? else {
            return Err(raised("type_error", "expected a table"));
        };
        let Body::Table { lookup, rows, .. } = &mut head.body else {
            return Err(raised("type_error", "expected a table"));
        };
        if let Some(Ordinal(ordinal)) = lookup.remove(&normal, &self.access)? {
            if let Some(entry) = rows.remove(&ordinal.to_be_bytes(), &self.access)? {
                self.dropped(table, &entry.key);
                self.dropped(table, &entry.value);
            }
            self.publish(table, head)?;
        }
        Ok(())
    }
    fn record_write(
        &mut self,
        record: EntityId,
        field: FieldName<'_>,
        value: Value,
    ) -> Result<(), HeapWriteError> {
        let key = match field {
            FieldName::Static(s) => Value::Text(self.text(s).map_err(write_native)?),
            FieldName::Text(t) => Value::Text(t.clone()),
        };
        self.table_write(record, ObjectKind::Record, key, value)
    }
    fn record_remove(
        &mut self,
        record: EntityId,
        field: FieldName<'_>,
    ) -> Result<(), HeapWriteError> {
        self.required(record, ObjectKind::Record)?;
        let key = match field {
            FieldName::Static(s) => Value::Text(self.text(s).map_err(write_native)?),
            FieldName::Text(t) => Value::Text(t.clone()),
        };
        self.table_remove(record, &key)
    }
    fn variable_write(&mut self, variable: EntityId, value: Value) -> Result<(), HeapWriteError> {
        if let Body::Variable(old) = self.required(variable, ObjectKind::Variable)?.body {
            self.dropped(variable, &old);
        }
        let value = self.canonical(value)?;
        self.publish(
            variable,
            ObjectHead {
                body: Body::Variable(value),
                family: variable.family,
            },
        )
    }
    fn edges_of(&self, value: &Value, out: &mut Vec<Identity>) -> Result<(), HeapFault> {
        match value {
            Value::List(id)
            | Value::Map(id)
            | Value::Set(id)
            | Value::Record(id)
            | Value::Closure(id) => out.push(Identity::Object(*id)),
            Value::Task(id) => out.push(Identity::Task(*id)),
            Value::Ref(id) => out.push(*id),
            Value::Tuple(t) if t.summary.has_edges => {
                for i in 0..t.summary.len {
                    self.edges_of(&required_value(self.tuple_get(t, i)?)?, out)?;
                }
            }
            Value::Error(e) if e.has_edges => self.edges_of(&self.error_data(e)?, out)?,
            _ => (),
        }
        Ok(())
    }
    fn cost_of(&self, value: &Value) -> LogicalBytes {
        LogicalBytes(value_cost(value))
    }
    fn tuple_slice(
        &mut self,
        tuple: &TupleRef,
        range: Range<u64>,
    ) -> Result<TupleRef, HeapWriteError> {
        if range.start > range.end || range.end > tuple.summary.len {
            return Err(raised(
                "index_out_of_range",
                "tuple slice is outside its length",
            ));
        }
        let mut values = Vec::new();
        for i in range {
            values.push(required_value(self.tuple_get(tuple, i)?)?);
        }
        self.tuple(values).map_err(write_native)
    }
    fn reserve_logical(
        &mut self,
        bytes: LogicalBytes,
        transient: &TransientRoots<'_>,
    ) -> Result<(), HeapWriteError> {
        let Some(mut census) = self.census.take() else {
            return Err(
                HeapFault::Malformed("the machine has not installed its census".into()).into(),
            );
        };
        let result = match census.reserve(bytes) {
            ReservationDecision::Granted(_) => Ok(()),
            ReservationDecision::NeedsPrecision(pending) => {
                match census.refine(self, transient, pending) {
                    Ok(RefinedDecision::Granted(_)) => Ok(()),
                    Ok(RefinedDecision::Refused { .. }) => Err(HeapWriteError::Memory),
                    Err(error) => Err(HeapWriteError::Fault(census_fault(error))),
                }
            }
        };
        self.accounting = census.certificate();
        self.census = Some(census);
        result
    }
    fn seal(&mut self, budget: SealBudget) -> Result<Option<SealedHeap>, HeapFault> {
        if self.header.is_some() && self.header_state == Some((self.accounting, self.population)) {
            return Ok(Some(SealedHeap {
                root: self.heap_root()?,
                sealed_pages: 0,
            }));
        }
        let mut work = SealWork::new(budget);
        if !self.directory.seal(&self.access, &mut work)?
            || !self.families.seal(&self.access, &mut work)?
        {
            return Ok(None);
        }
        let root = self.heap_root()?;
        let mut out = TokenWriter::new();
        out.sum(0, 1);
        root.encode(&mut out);
        let tokens = out.into_bytes();
        let children = [root.directory.page(), root.families.page()]
            .into_iter()
            .flatten()
            .collect();
        if !work.take(tokens.len() as u64 + 128) {
            return Ok(None);
        }
        self.header = Some(self.access.seal(PageKind::KEYED_LEAF, tokens, children)?);
        self.header_state = Some((root.accounting, root.population));
        self.epoch.0 = self
            .epoch
            .0
            .checked_add(1)
            .ok_or_else(|| HeapFault::Malformed("edit epoch overflow".into()))?;
        Ok(Some(SealedHeap {
            root,
            sealed_pages: work.pages,
        }))
    }
    fn evict(&mut self) -> Result<(), HeapFault> {
        self.seal(SealBudget {
            pages: u64::MAX,
            bytes: u64::MAX,
        })?;
        self.access.clear();
        Ok(())
    }
    fn sealed_root(&self) -> Option<PageRef> {
        self.header
    }
}
impl PagedHeap {
    fn table_write(
        &mut self,
        table: EntityId,
        kind: ObjectKind,
        key: Value,
        value: Value,
    ) -> Result<(), HeapWriteError> {
        let normal = self.key(&key)?;
        let mut head = self.required(table, kind)?;
        let key = self.canonical(key)?;
        let value = self.canonical(value)?;
        let mut dropped = None;
        if let Body::Table {
            lookup, rows, next, ..
        } = &mut head.body
        {
            if let Some(Ordinal(ordinal)) = lookup.get(&normal, &self.access)? {
                let mut e = rows
                    .get(&ordinal.to_be_bytes(), &self.access)?
                    .ok_or_else(|| HeapFault::Malformed("lookup points at no entry".into()))?;
                dropped = Some(std::mem::replace(&mut e.value, value));
                rows.write(ordinal_key(ordinal), e, &self.access)?;
            } else {
                let following = next
                    .checked_add(1)
                    .ok_or_else(|| HeapFault::Malformed("table ordinal overflow".into()))?;
                rows.write(ordinal_key(*next), Entry { key, value }, &self.access)?;
                lookup.write(normal, Ordinal(*next), &self.access)?;
                *next = following;
            }
        }
        if let Some(old) = dropped {
            self.dropped(table, &old);
        }
        self.publish(table, head)
    }
}
impl PagedHeap {
    pub(super) fn table_get(
        &self,
        table: EntityId,
        key: &Value,
    ) -> Result<Option<Value>, HeapFault> {
        let Some(normal) = self.normal_key(key)? else {
            return Ok(None);
        };
        self.read_head(table, |head| {
            let Some(Body::Table { lookup, rows, .. }) = head.map(|h| &h.body) else {
                return Ok(None);
            };
            let Some(Ordinal(ordinal)) = lookup.get(&normal_bytes(normal), &self.access)? else {
                return Ok(None);
            };
            rows.get(&ordinal.to_be_bytes(), &self.access)?
                .map(|e| e.value)
                .map(Some)
                .ok_or_else(|| HeapFault::Malformed("lookup points at no entry".into()))
        })
    }
}
pub(super) fn keyed<T: Cell>(tree: &Tree<T>) -> Result<KeyedRoot, HeapFault> {
    tree.keyed_root()
}
fn normal_bytes(key: NormalKey) -> Vec<u8> {
    match key {
        NormalKey::Inline(b) => [vec![0], b].concat(),
        NormalKey::Digest(d) => [vec![1], d.to_vec()].concat(),
    }
}
pub(super) fn binary_summary(len: u64) -> TextSummary {
    TextSummary {
        utf8_bytes: len,
        chars: 0,
        utf16_units: 0,
    }
}
pub(super) fn value_depth(value: &Value) -> u32 {
    match value {
        Value::Tuple(t) => t.summary.depth,
        Value::Error(e) => e.depth,
        _ => 0,
    }
}
pub(super) fn text_same(heap: &PagedHeap, a: &TextRef, b: &TextRef) -> Result<bool, HeapFault> {
    if a.summary != b.summary {
        return Ok(false);
    }
    if let (Payload::Paged(a), Payload::Paged(b)) = (&a.body, &b.body) {
        return Ok(a.id == b.id);
    }
    Ok(heap.copy_text(a)? == heap.copy_text(b)?)
}
pub(super) fn bytes_same(heap: &PagedHeap, a: &BytesRef, b: &BytesRef) -> Result<bool, HeapFault> {
    if a.len != b.len {
        return Ok(false);
    }
    if let (Payload::Paged(a), Payload::Paged(b)) = (&a.body, &b.body) {
        return Ok(a.id == b.id);
    }
    Ok(heap.copy_bytes(a)? == heap.copy_bytes(b)?)
}
pub(super) fn required_value(value: Option<Value>) -> Result<Value, HeapFault> {
    value.ok_or_else(|| HeapFault::Malformed("an admitted collection element is absent".into()))
}
fn raised(kind: &'static str, message: &str) -> HeapWriteError {
    HeapWriteError::Raised {
        kind,
        message: message.into(),
    }
}
pub(super) fn native_raised(kind: &str, message: &str) -> NativeError {
    NativeError::Raised(Arc::new(ErrorRef {
        kind: TextRef {
            normal: Default::default(),
            summary: text_summary(kind),
            body: Payload::Resident(Arc::from(kind)),
        },
        message: TextRef {
            normal: Default::default(),
            summary: text_summary(message),
            body: Payload::Resident(Arc::from(message)),
        },
        family: None,
        task_edges: 0,
        data: Payload::Resident(Arc::new(Value::Null)),
        depth: 1,
        cost: LogicalBytes((kind.len() + message.len()) as u64 + 16),
        has_edges: false,
    }))
}
pub(super) fn native_write(error: HeapWriteError) -> NativeError {
    match error {
        HeapWriteError::Raised { kind, message } => native_raised(kind, &message),
        HeapWriteError::Memory => NativeError::Memory,
        HeapWriteError::Fault(e) => NativeError::Heap(e),
        // The heap recorded the refusal; the machine's write wrapper reads
        // it (FIG-6318).
        HeapWriteError::NeedsPrecision { .. } => NativeError::Memory,
    }
}
pub(super) fn write_native(error: NativeError) -> HeapWriteError {
    match error {
        NativeError::Heap(e) => HeapWriteError::Fault(e),
        NativeError::Memory => HeapWriteError::Memory,
        other => HeapWriteError::Fault(HeapFault::Malformed(format!(
            "unexpected immutable constructor failure: {other}"
        ))),
    }
}
impl Cell for FamilyRecord {
    fn cost(&self) -> u64 {
        self.base.summary.cost.0
    }
    fn children(&self, out: &mut Vec<PageRef>) {
        out.push(self.base.root);
        out.extend(self.membership.page());
    }
    fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
        super::tree::grant(&mut self.base.root, children)?;
        if let Some(p) = self.membership.page_mut() {
            super::tree::grant(p, children)?;
        }
        Ok(())
    }
}

pub(super) fn census_fault(error: CensusFault) -> HeapFault {
    match error {
        CensusFault::Heap(error) => error,
        CensusFault::Malformed(message) => HeapFault::Malformed(message),
    }
}

impl PagedHeap {
    pub(super) fn normalize_value(&self, value: &Value) -> Result<Option<NormalKey>, HeapFault> {
        fn tokens(
            heap: &PagedHeap,
            value: &Value,
            out: &mut TokenWriter,
        ) -> Result<bool, HeapFault> {
            match value {
                Value::Null => out.sum(0, 0),
                Value::Bool(b) => {
                    out.sum(1, 1);
                    b.encode(out);
                }
                Value::Int(i) => {
                    out.sum(2, 1);
                    heap.copy_int(i)?.encode(out);
                }
                Value::Float(f) => {
                    if let Some(i) = integral_float(f.get()) {
                        out.sum(2, 1);
                        i.encode(out);
                    } else {
                        out.sum(3, 1);
                        f.encode(out);
                    }
                }
                Value::Text(t) => {
                    out.sum(4, 1);
                    out.text(&heap.copy_text(t)?);
                }
                Value::Bytes(b) => {
                    out.sum(5, 1);
                    out.bytes(&heap.copy_bytes(b)?);
                }
                Value::Timestamp(t) => {
                    out.sum(6, 1);
                    t.encode(out);
                }
                Value::Ref(i) => {
                    out.sum(7, 1);
                    i.encode(out);
                }
                Value::Function(n) => {
                    out.sum(8, 1);
                    out.text(&heap.copy_text(n)?);
                }
                Value::Tuple(t) => {
                    out.sum(9, 1);
                    out.seq(t.summary.len);
                    for i in 0..t.summary.len {
                        let v = heap.tuple_get(t, i)?.ok_or_else(|| {
                            HeapFault::Malformed("tuple key element missing".into())
                        })?;
                        if !tokens(heap, &v, out)? {
                            return Ok(false);
                        }
                    }
                }
                _ => return Ok(false),
            }
            Ok(true)
        }
        let function = matches!(value, Value::Function(_));
        let cache = match value {
            Value::Text(text) | Value::Function(text) => Some(&text.normal),
            Value::Bytes(bytes) => Some(&bytes.normal),
            Value::Tuple(tuple) => Some(&tuple.normal),
            Value::Int(IntRef::Paged { normal, .. }) => Some(normal),
            _ => None,
        };
        if let Some(key) = cache.and_then(|cache| {
            if function {
                cache.get_function()
            } else {
                cache.get()
            }
        }) {
            return Ok(key.clone());
        }
        let mut out = TokenWriter::new();
        if !tokens(self, value, &mut out)? {
            if let Some(cache) = cache {
                if function {
                    cache.set_function(None);
                } else {
                    cache.set(None);
                }
            }
            return Ok(None);
        }
        let bytes = out.into_bytes();
        let key = Some(if bytes.len() <= NormalKey::INLINE_MAX {
            NormalKey::Inline(bytes)
        } else {
            NormalKey::Digest(key_digest(&bytes))
        });
        if let Some(cache) = cache {
            if function {
                cache.set_function(key.clone());
            } else {
                cache.set(key.clone());
            }
        }
        Ok(key)
    }
}
