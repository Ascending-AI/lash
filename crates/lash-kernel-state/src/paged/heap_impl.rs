//! The identity-addressed paged semantic heap. Collection edits update one
//! bounded path and replace its directory head; no mutable container escapes.
use super::directory::{Body, Entry, ObjectHead, Ordinal, entity_key, ordinal_key, value_cost};
use super::source::INLINE_BODY_MAX;
use super::tree::{Access, Cell, SealWork, Tree, checked_sum};
use super::*;
use lash_kernel_doc::graph_tokens::{TokenCodec, TokenWriter, encode_body};
use lash_kernel_doc::heap::*;
use lash_kernel_doc::heap_values::{integer_from_digits, integral_float, key_digest, text_summary};
use lash_kernel_doc::{Integer, TaskId};
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::ops::{ControlFlow, Range};
use std::sync::Arc;

pub struct PagedHeap {
    pub access: Access,
    pub directory: Tree<ObjectHead>,
    pub families: Tree<FamilyRecord>,
    next_ordinal: u64,
    next_family: u64,
    epoch: EditEpoch,
    pub(super) header: Option<PageRef>,
    pub(super) accounting: AccountingCertificate,
    family_base: RefCell<Option<Box<dyn FamilyBase>>>,
    pub(super) census: Option<Box<dyn AllocationCensus>>,
    pub session: Tree<BindingRecord>,
    pub tasks: Tree<super::directory::execution::TaskPage>,
    temporary: Vec<Value>,
    pub(super) reserved: u64,
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
        if answer.kind != PageKind::KEYED_LEAF {
            return Err(HeapFault::Corrupt {
                page: page.id,
                reason: "expected heap header".into(),
            });
        }
        let mut input = lash_kernel_doc::graph_tokens::TokenReader::new(&answer.tokens);
        let (variant, fields) = input.sum().map_err(|e| HeapFault::Corrupt {
            page: page.id,
            reason: e.to_string(),
        })?;
        if variant != 0 || fields != 1 {
            return Err(HeapFault::Corrupt {
                page: page.id,
                reason: "superseded heap header".into(),
            });
        }
        let mut root = HeapRoot::decode(&mut input).map_err(|e| HeapFault::Corrupt {
            page: page.id,
            reason: e.to_string(),
        })?;
        input.finish().map_err(|e| HeapFault::Corrupt {
            page: page.id,
            reason: e.to_string(),
        })?;
        if let Some(p) = &mut root.directory.page {
            super::tree::grant(p, &answer.children)?;
        }
        if let Some(p) = &mut root.families.page {
            super::tree::grant(p, &answer.children)?;
        }
        let directory = Tree::open(
            root.directory.page,
            root.directory.len,
            root.directory.cost,
            false,
        );
        let families = Tree::open(
            root.families.page,
            root.families.len,
            root.families.cost,
            false,
        );
        Ok(Self {
            access,
            directory,
            families,
            next_ordinal: root.next_ordinal,
            next_family: root.next_family,
            epoch,
            header: Some(page),
            accounting: root.accounting,
            family_base: RefCell::new(None),
            census: None,
            session: Tree::new(false),
            tasks: Tree::new(false),
            temporary: Vec::new(),
            reserved: 0,
        })
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
    pub fn release_statement_roots(&mut self) -> Result<(), HeapFault> {
        if let Some(census) = &mut self.census {
            census
                .changed(LogicalBytes(self.reserved), LogicalBytes(0))
                .map_err(census_fault)?;
            self.accounting = census.certificate();
        }
        self.temporary.clear();
        self.reserved = 0;
        self.header = None;
        Ok(())
    }
    pub fn epoch(&self) -> EditEpoch {
        self.epoch
    }
    pub fn heap_root(&self) -> HeapRoot {
        HeapRoot {
            directory: keyed(&self.directory),
            families: keyed(&self.families),
            next_ordinal: self.next_ordinal,
            next_family: self.next_family,
            accounting: self.accounting,
        }
    }
    pub(super) fn head(&self, entity: EntityId) -> Result<Option<ObjectHead>, HeapFault> {
        if entity.family != FamilyId::PRIMARY {
            let Some(family) = self
                .families
                .get(&ordinal_key(entity.family.0), &self.access)?
            else {
                return Ok(None);
            };
            if entity.ordinal >= family.extent || !self.family_member(&family, entity.ordinal)? {
                return Ok(None);
            }
        }
        if let Some(head) = self.directory.get(&entity_key(entity), &self.access)? {
            return Ok(Some(head));
        }
        if entity.family == FamilyId::PRIMARY {
            return Ok(None);
        }
        let Some(family) = self
            .families
            .get(&ordinal_key(entity.family.0), &self.access)?
        else {
            return Ok(None);
        };
        if entity.ordinal >= family.extent || !self.family_member(&family, entity.ordinal)? {
            return Ok(None);
        }
        let mut base = self.family_base.borrow_mut();
        let base = base
            .as_mut()
            .ok_or_else(|| HeapFault::Malformed("lazy family reader is not installed".into()))?;
        let head = self
            .access
            .with_source(|pages| base.base_object(entity, &family.base, pages))?
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
    fn required(&self, entity: EntityId, kind: ObjectKind) -> Result<ObjectHead, HeapWriteError> {
        match self.head(entity)? {
            Some(head) if head.kind() == kind => Ok(head),
            _ => Err(raised("type_error", "the object has another kind")),
        }
    }
    pub fn set_statement_roots(&mut self, values: Vec<Value>) {
        self.temporary = values;
    }
    pub(super) fn admit(&mut self, bytes: u64, extra: &[Value]) -> Result<(), HeapWriteError> {
        let mut values = self.temporary.clone();
        values.extend_from_slice(extra);
        self.reserve_logical(
            LogicalBytes(bytes),
            &TransientRoots {
                registers: &values,
                pinned: &[],
                native_args: &[],
                fresh: &[],
                reserved: LogicalBytes(self.reserved),
            },
        )
    }
    fn publish(&mut self, entity: EntityId, head: ObjectHead) -> Result<(), HeapWriteError> {
        let before = self.head(entity)?.map_or(0, |h| h.cost());
        let after = head.cost();
        let upper = self
            .accounting
            .upper
            .0
            .checked_sub(before)
            .and_then(|u| u.checked_add(after))
            .ok_or_else(|| HeapFault::Malformed("directory cost overflow".into()))?;
        if let Some(census) = &mut self.census {
            if before == 0 {
                census.added(LogicalBytes(after), 1).map_err(census_fault)?;
            } else {
                census
                    .changed(LogicalBytes(before), LogicalBytes(after))
                    .map_err(census_fault)?;
            }
            self.accounting = census.certificate();
        } else {
            self.accounting.upper = LogicalBytes(upper);
            if before == 0 {
                self.accounting.entities += 1;
            }
        }
        self.directory
            .write(entity_key(entity), head, &self.access)?;
        self.header = None;
        Ok(())
    }
    fn make_head(&self, init: ObjectInit) -> Result<ObjectHead, HeapFault> {
        let body = match init {
            ObjectInit::List(values) => {
                let mut rows = Tree::new(true);
                let next = values.len() as u64;
                let inline = if encode_body(&values).len() <= INLINE_BODY_MAX as usize {
                    Some(values)
                } else {
                    for (i, v) in values.into_iter().enumerate() {
                        rows.write(ordinal_key(i as u64), v, &self.access)?;
                    }
                    None
                };
                Body::List { inline, rows, next }
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
                let mut rows = Tree::new(true);
                for (i, id) in captures.into_iter().enumerate() {
                    rows.write(ordinal_key(i as u64), id, &self.access)?;
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
                    .get(&ordinal_key(ordinal), &self.access)?
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
                e.data = Arc::new(self.canonical((*e.data).clone())?);
                e.cost = LogicalBytes(
                    e.kind
                        .summary
                        .utf8_bytes
                        .saturating_add(e.message.summary.utf8_bytes)
                        .saturating_add(value_cost(&e.data)),
                );
                let mut edges = Vec::new();
                self.edges_of(&e.data, &mut edges)?;
                e.has_edges = !edges.is_empty();
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
            IntRef::Paged { summary, digits } => {
                let len = summary.bits.div_ceil(8);
                let mut out = Vec::new();
                super::payload::visit(
                    &self.access,
                    *digits,
                    binary_summary(len),
                    false,
                    0..len,
                    &mut |b| {
                        out.extend_from_slice(b);
                        ControlFlow::Continue(())
                    },
                )?;
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
            directory: Tree::new(false),
            families: Tree::new(false),
            next_ordinal: 0,
            next_family: 1,
            epoch,
            header: None,
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
            temporary: Vec::new(),
            reserved: 0,
        }
    }
    fn open(
        root: &HeapRoot,
        source: Box<dyn PageSource>,
        epoch: EditEpoch,
    ) -> Result<Self, HeapFault> {
        Ok(Self {
            access: Access::new(source),
            directory: Tree::open(
                root.directory.page,
                root.directory.len,
                root.directory.cost,
                false,
            ),
            families: Tree::open(
                root.families.page,
                root.families.len,
                root.families.cost,
                false,
            ),
            next_ordinal: root.next_ordinal,
            next_family: root.next_family,
            epoch,
            header: None,
            accounting: root.accounting,
            family_base: RefCell::new(None),
            census: None,
            session: Tree::new(false),
            tasks: Tree::new(false),
            temporary: Vec::new(),
            reserved: 0,
        })
    }
    fn kind(&self, entity: EntityId) -> Result<Option<ObjectKind>, HeapFault> {
        Ok(self.head(entity)?.map(|h| h.kind()))
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
        match self.head(variable)?.map(|h| h.body) {
            Some(Body::Variable(v)) => Ok(v),
            _ => Err(HeapFault::Malformed("expected a shared variable".into())),
        }
    }
    fn closure(&self, closure: EntityId) -> Result<ClosureView, HeapFault> {
        match self.head(closure)?.map(|h| h.body) {
            Some(Body::Closure { site, captures }) => {
                let mut values = Vec::new();
                captures.visit(&self.access, &mut |_, id| {
                    values.push(*id);
                    true
                })?;
                Ok(ClosureView {
                    site,
                    captures: values,
                })
            }
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
            Some(Body::Table { rows, .. }) => rows.passed(&ordinal_key(last.0), &self.access),
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
        let mut values = Vec::new();
        match &init {
            ObjectInit::List(v) | ObjectInit::Set(v) => values.extend_from_slice(v),
            ObjectInit::Map(v) => {
                for (k, v) in v {
                    values.push(k.clone());
                    values.push(v.clone());
                }
            }
            ObjectInit::Record(v) => {
                for (k, v) in v {
                    values.push(Value::Text(k.clone()));
                    values.push(v.clone());
                }
            }
            ObjectInit::Variable(v) => values.push(v.clone()),
            ObjectInit::Closure { captures, .. } => {
                values.extend(captures.iter().map(|id| Value::Ref(Identity::Object(*id))))
            }
        }
        let head = self.make_head(init)?;
        self.admit(head.cost(), &values)?;
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
        let mut head = self.required(list, ObjectKind::List)?;
        if index > head.len() {
            return Err(raised(
                "index_out_of_range",
                "index exceeds the list length",
            ));
        }
        let value = self.canonical(value)?;
        let before = self.list_get(list, index)?.as_ref().map_or(0, value_cost);
        self.admit(
            value_cost(&value).saturating_sub(before),
            &[Value::List(list), value.clone()],
        )?;
        if let Body::List { inline, rows, next } = &mut head.body {
            if let Some(values) = inline {
                if index == values.len() as u64 {
                    values.push(value);
                    *next += 1;
                } else {
                    values[index as usize] = value;
                }
                if encode_body(values).len() > INLINE_BODY_MAX as usize {
                    for (i, v) in values.iter().enumerate() {
                        rows.write(ordinal_key(i as u64), v.clone(), &self.access)?;
                    }
                    *next = values.len() as u64;
                    *inline = None;
                }
            } else {
                let key = if index == rows.len() {
                    let key = ordinal_key(*next);
                    *next = next
                        .checked_add(1)
                        .ok_or_else(|| HeapFault::Malformed("list ordinal overflow".into()))?;
                    key
                } else {
                    rows.rank_get(index, &self.access)?
                        .ok_or_else(|| HeapFault::Malformed("list rank missing".into()))?
                        .0
                };
                rows.write(key, value, &self.access)?;
            }
        }
        self.publish(list, head)
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
            Body::List {
                inline: Some(v), ..
            } => v.remove(index as usize),
            Body::List { rows, .. } => {
                let (key, _) = rows
                    .rank_get(index, &self.access)?
                    .ok_or_else(|| HeapFault::Malformed("list rank missing".into()))?;
                rows.remove(&key, &self.access)?
                    .ok_or_else(|| HeapFault::Malformed("list row missing".into()))?
            }
            _ => return Err(HeapFault::Malformed("list descriptor kind mismatch".into()).into()),
        };
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
            rows.remove(&ordinal_key(ordinal), &self.access)?;
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
        let head = self.required(variable, ObjectKind::Variable)?;
        let value = self.canonical(value)?;
        self.admit(
            (32 + value_cost(&value)).saturating_sub(head.cost()),
            &[Value::Ref(Identity::Object(variable)), value.clone()],
        )?;
        self.publish(
            variable,
            ObjectHead {
                body: Body::Variable(value),
                family: variable.family,
            },
        )
    }
    fn normal_key(&self, value: &Value) -> Result<Option<NormalKey>, HeapFault> {
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
                    n.encode(out);
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
        let mut out = TokenWriter::new();
        if !tokens(self, value, &mut out)? {
            return Ok(None);
        }
        let bytes = out.into_bytes();
        Ok(Some(if bytes.len() <= NormalKey::INLINE_MAX {
            NormalKey::Inline(bytes)
        } else {
            NormalKey::Digest(key_digest(&bytes))
        }))
    }
    fn equal(&self, a: &Value, b: &Value) -> Result<bool, HeapFault> {
        let mut pending = vec![(a.clone(), b.clone())];
        let mut seen = BTreeSet::new();
        while let Some((a, b)) = pending.pop() {
            match (&a, &b) {
                (Value::Null, Value::Null) | (Value::Absent, Value::Absent) => (),
                (Value::Bool(a), Value::Bool(b)) if a == b => (),
                (Value::Int(a), Value::Int(b)) if self.copy_int(a)? == self.copy_int(b)? => (),
                (Value::Float(a), Value::Float(b)) if a.get() == b.get() => (),
                (Value::Int(i), Value::Float(f)) | (Value::Float(f), Value::Int(i)) => {
                    if integral_float(f.get()).as_ref() != Some(&self.copy_int(i)?) {
                        return Ok(false);
                    }
                }
                (Value::Text(a), Value::Text(b)) if text_same(self, a, b)? => (),
                (Value::Bytes(a), Value::Bytes(b)) if bytes_same(self, a, b)? => (),
                (Value::Timestamp(a), Value::Timestamp(b)) if a == b => (),
                (Value::Function(a), Value::Function(b)) if a == b => (),
                (Value::Handle(a), Value::Handle(b)) if a == b => (),
                (Value::Task(a), Value::Task(b)) if a == b => (),
                (Value::Ref(a), Value::Ref(b)) if a == b => (),
                (Value::Closure(a), Value::Closure(b)) if a == b => (),
                (Value::Tuple(a), Value::Tuple(b)) => {
                    if a.summary.len != b.summary.len {
                        return Ok(false);
                    }
                    for i in 0..a.summary.len {
                        pending.push((
                            required_value(self.tuple_get(a, i)?)?,
                            required_value(self.tuple_get(b, i)?)?,
                        ));
                    }
                }
                (Value::Error(a), Value::Error(b)) => {
                    if !text_same(self, &a.kind, &b.kind)?
                        || !text_same(self, &a.message, &b.message)?
                    {
                        return Ok(false);
                    }
                    pending.push(((*a.data).clone(), (*b.data).clone()));
                }
                (Value::List(a), Value::List(b)) => {
                    if !seen.insert((0, *a, *b)) {
                        continue;
                    }
                    let len = self.len(*a)?;
                    if len != self.len(*b)? {
                        return Ok(false);
                    }
                    for i in 0..len {
                        pending.push((
                            required_value(self.list_get(*a, i)?)?,
                            required_value(self.list_get(*b, i)?)?,
                        ));
                    }
                }
                (Value::Map(a), Value::Map(b))
                | (Value::Set(a), Value::Set(b))
                | (Value::Record(a), Value::Record(b)) => {
                    if !seen.insert((1, *a, *b)) {
                        continue;
                    }
                    if self.len(*a)? != self.len(*b)? {
                        return Ok(false);
                    }
                    let Some(ObjectHead {
                        body: Body::Table { rows, .. },
                        ..
                    }) = self.head(*a)?
                    else {
                        return Err(HeapFault::Malformed("table identity missing".into()));
                    };
                    let mut error = None;
                    let mut same = true;
                    rows.visit(&self.access, &mut |_, e| {
                        match self.table_get(*b, &e.key) {
                            Ok(Some(v)) => pending.push((e.value.clone(), v)),
                            Ok(None) => same = false,
                            Err(e) => error = Some(e),
                        }
                        same && error.is_none()
                    })?;
                    if let Some(e) = error {
                        return Err(e);
                    }
                    if !same {
                        return Ok(false);
                    }
                }
                _ => return Ok(false),
            }
        }
        Ok(true)
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
            Value::Error(e) if e.has_edges => self.edges_of(&e.data, out)?,
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
        if self.header.is_some() {
            return Ok(Some(SealedHeap {
                root: self.heap_root(),
                sealed_pages: 0,
            }));
        }
        let mut work = SealWork::new(budget);
        if !self.directory.seal(&self.access, &mut work)?
            || !self.families.seal(&self.access, &mut work)?
        {
            return Ok(None);
        }
        let root = self.heap_root();
        let mut out = TokenWriter::new();
        out.sum(0, 1);
        root.encode(&mut out);
        let tokens = out.into_bytes();
        let children = [root.directory.page, root.families.page]
            .into_iter()
            .flatten()
            .collect();
        if !work.take(tokens.len() as u64 + 128) {
            return Ok(None);
        }
        self.header = Some(self.access.seal(PageKind::KEYED_LEAF, tokens, children)?);
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
        let before = self.table_get(table, &key)?.as_ref().map_or(0, value_cost);
        let key_cost = if before == 0 {
            value_cost(&key).saturating_sub(if kind == ObjectKind::Record { 16 } else { 0 })
        } else {
            0
        };
        self.admit(
            value_cost(&value).saturating_sub(before) + key_cost,
            &[
                Value::Ref(Identity::Object(table)),
                key.clone(),
                value.clone(),
            ],
        )?;
        if let Body::Table {
            lookup, rows, next, ..
        } = &mut head.body
        {
            if let Some(Ordinal(ordinal)) = lookup.get(&normal, &self.access)? {
                let mut e = rows
                    .get(&ordinal_key(ordinal), &self.access)?
                    .ok_or_else(|| HeapFault::Malformed("lookup points at no entry".into()))?;
                e.value = value;
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
        let Some(ObjectHead {
            body: Body::Table { lookup, rows, .. },
            ..
        }) = self.head(table)?
        else {
            return Ok(None);
        };
        let Some(Ordinal(ordinal)) = lookup.get(&normal_bytes(normal), &self.access)? else {
            return Ok(None);
        };
        rows.get(&ordinal_key(ordinal), &self.access)?
            .map(|e| e.value)
            .map(Some)
            .ok_or_else(|| HeapFault::Malformed("lookup points at no entry".into()))
    }
}
pub(super) fn keyed<T: Cell>(tree: &Tree<T>) -> KeyedRoot {
    KeyedRoot {
        page: tree.page(),
        len: tree.len(),
        cost: LogicalBytes(tree.cost()),
    }
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
        Value::Error(e) => 1 + value_depth(&e.data),
        _ => 0,
    }
}
fn text_same(heap: &PagedHeap, a: &TextRef, b: &TextRef) -> Result<bool, HeapFault> {
    if a.summary != b.summary {
        return Ok(false);
    }
    if let (Payload::Paged(a), Payload::Paged(b)) = (&a.body, &b.body) {
        return Ok(a.id == b.id);
    }
    Ok(heap.copy_text(a)? == heap.copy_text(b)?)
}
fn bytes_same(heap: &PagedHeap, a: &BytesRef, b: &BytesRef) -> Result<bool, HeapFault> {
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
            summary: text_summary(kind),
            body: Payload::Resident(Arc::from(kind)),
        },
        message: TextRef {
            summary: text_summary(message),
            body: Payload::Resident(Arc::from(message)),
        },
        data: Arc::new(Value::Null),
        cost: LogicalBytes((kind.len() + message.len()) as u64 + 16),
        has_edges: false,
    }))
}
pub(super) fn native_write(error: HeapWriteError) -> NativeError {
    match error {
        HeapWriteError::Raised { kind, message } => native_raised(kind, &message),
        HeapWriteError::Memory => NativeError::Memory,
        HeapWriteError::Fault(e) => NativeError::Heap(e),
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
        out.extend(self.membership.page);
    }
    fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
        super::tree::grant(&mut self.base.root, children)?;
        if let Some(p) = &mut self.membership.page {
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

impl CensusGraph for PagedHeap {
    fn root_cost(&mut self) -> Result<LogicalBytes, HeapFault> {
        Ok(LogicalBytes(checked_sum(
            [self.session.cost(), self.tasks.cost()].into_iter(),
        )?))
    }
    fn value_cost(&mut self, value: &Value) -> Result<LogicalBytes, HeapFault> {
        Ok(self.cost_of(value))
    }
    fn value_edges(
        &mut self,
        value: &Value,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        self.stream_edges(value, out)
    }
    fn persistent_roots(
        &mut self,
        visit: &mut dyn FnMut(&Value) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        let binding =
            |binding: &BindingRecord, visit: &mut dyn FnMut(&Value) -> Result<(), HeapFault>| {
                match &binding.value {
                    BoundRecord::Value(v) => visit(v),
                    BoundRecord::Shared(id) => visit(&Value::Ref(Identity::Object(*id))),
                }
            };
        visit_tree(&self.session, &self.access, &mut |b| binding(b, visit))?;
        visit_tree(&self.tasks, &self.access, &mut |task| {
            match &task.state {
                TaskStateRecord::Resuming(IncomingRecord::Value(v) | IncomingRecord::Raise(v)) => {
                    visit(v)?
                }
                TaskStateRecord::Resuming(IncomingRecord::Joined(t))
                | TaskStateRecord::Joining(t) => visit(&Value::Task(*t))?,
                _ => (),
            }
            visit_tree(&task.frames, &self.access, &mut |frame| {
                visit_tree(&frame.bindings, &self.access, &mut |b| binding(b, visit))?;
                visit_tree(&frame.iterated, &self.access, visit)?;
                visit_tree(&frame.departing, &self.access, visit)?;
                if let Some(args) = &frame.arguments {
                    visit_tree(args, &self.access, visit)?;
                }
                Ok(())
            })
        })
    }
    fn entity_cost(&mut self, entity: EntityId) -> Result<Option<EntityCost>, HeapFault> {
        Ok(self.head(entity)?.map(|head| EntityCost {
            local: LogicalBytes(head.cost()),
            has_edges: true,
        }))
    }
    fn entity_edges(
        &mut self,
        entity: EntityId,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        let Some(head) = self.head(entity)? else {
            return Ok(());
        };
        let mut error = None;
        match head.body {
            Body::Variable(v) => return self.stream_edges(&v, out),
            Body::Closure { captures, .. } => captures.visit(
                &self.access,
                &mut |_, id| match out(Identity::Object(*id)) {
                    Ok(()) => true,
                    Err(e) => {
                        error = Some(e);
                        false
                    }
                },
            )?,
            Body::List {
                inline: Some(v), ..
            } => {
                for v in &v {
                    self.stream_edges(v, out)?;
                }
            }
            Body::List { rows, .. } => {
                rows.visit(&self.access, &mut |_, v| match self.stream_edges(v, out) {
                    Ok(()) => true,
                    Err(e) => {
                        error = Some(e);
                        false
                    }
                })?
            }
            Body::Table { rows, .. } => rows.visit(&self.access, &mut |_, e| match self
                .stream_edges(&e.key, out)
                .and_then(|()| self.stream_edges(&e.value, out))
            {
                Ok(()) => true,
                Err(e) => {
                    error = Some(e);
                    false
                }
            })?,
        }
        error.map_or(Ok(()), Err)
    }
    fn task_result(
        &mut self,
        task: TaskId,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<LogicalBytes, HeapFault> {
        let Some(task) = self.tasks.get(&ordinal_key(task.0), &self.access)? else {
            return Ok(LogicalBytes(0));
        };
        let value = match &task.state {
            TaskStateRecord::Ended(EndedTask::Returned(v) | EndedTask::Raised(v))
                if task.result_retained =>
            {
                v
            }
            _ => return Ok(LogicalBytes(0)),
        };
        self.stream_edges(value, out)?;
        Ok(self.cost_of(value))
    }
    fn sweep(&mut self, unreachable: &SweepSet) -> Result<(), HeapFault> {
        for entity in &unreachable.entities {
            if entity.family != FamilyId::PRIMARY {
                self.sweep_family_member(*entity)?;
            }
            self.directory.remove(&entity_key(*entity), &self.access)?;
        }
        for task in &unreachable.ended_task_results {
            if let Some(mut row) = self.tasks.get(&ordinal_key(task.0), &self.access)? {
                row.state = match row.state {
                    TaskStateRecord::Ended(EndedTask::Returned(_)) => {
                        TaskStateRecord::Ended(EndedTask::Returned(Value::Null))
                    }
                    TaskStateRecord::Ended(EndedTask::Raised(_)) => {
                        TaskStateRecord::Ended(EndedTask::Raised(Value::Null))
                    }
                    _ => {
                        return Err(HeapFault::Malformed(
                            "sweep named a running task result".into(),
                        ));
                    }
                };
                row.result_retained = false;
                self.tasks.write(ordinal_key(task.0), row, &self.access)?;
            }
        }
        self.header = None;
        Ok(())
    }
    fn census_after(
        &mut self,
        after: Option<EntityId>,
        limit: usize,
    ) -> Result<Vec<EntityId>, HeapFault> {
        let mut key = after.map(entity_key);
        let mut entities = Vec::new();
        while entities.len() < limit {
            let Some((next, _)) = self.directory.after(key.as_deref(), &self.access)? else {
                break;
            };
            if next.len() != 16 {
                return Err(HeapFault::Malformed(
                    "directory identity key is not 16 bytes".into(),
                ));
            }
            entities.push(EntityId {
                family: FamilyId(u64::from_be_bytes(
                    next[..8]
                        .try_into()
                        .map_err(|_| HeapFault::Malformed("identity family malformed".into()))?,
                )),
                ordinal: u64::from_be_bytes(
                    next[8..]
                        .try_into()
                        .map_err(|_| HeapFault::Malformed("identity ordinal malformed".into()))?,
                ),
            });
            key = Some(next);
        }
        Ok(entities)
    }
    fn ended_tasks_after(
        &mut self,
        after: Option<TaskId>,
        limit: usize,
    ) -> Result<Vec<TaskId>, HeapFault> {
        let mut key = after.map(|t| ordinal_key(t.0));
        let mut tasks = Vec::new();
        while tasks.len() < limit {
            let Some((next, row)) = self.tasks.after(key.as_deref(), &self.access)? else {
                break;
            };
            if !matches!(row.state, TaskStateRecord::Ended(EndedTask::Returned(_) | EndedTask::Raised(_)) if row.result_retained)
            {
                key = Some(next);
                continue;
            }
            tasks.push(TaskId(u64::from_be_bytes(
                next.as_slice()
                    .try_into()
                    .map_err(|_| HeapFault::Malformed("task identity key is not 8 bytes".into()))?,
            )));
            key = Some(next);
        }
        Ok(tasks)
    }
}
impl PagedHeap {
    /// Streams nested immutable edges in place, stopping on the first visitor
    /// failure. Scalar payloads are never loaded by a trace.
    pub fn stream_edges(
        &self,
        value: &Value,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        match value {
            Value::List(id)
            | Value::Map(id)
            | Value::Set(id)
            | Value::Record(id)
            | Value::Closure(id) => out(Identity::Object(*id))?,
            Value::Task(id) => out(Identity::Task(*id))?,
            Value::Ref(id) => out(*id)?,
            Value::Tuple(t) if t.summary.has_edges => {
                let mut error = None;
                self.tuple_visit(t, &mut |v| match self.stream_edges(v, out) {
                    Ok(()) => ControlFlow::Continue(()),
                    Err(e) => {
                        error = Some(e);
                        ControlFlow::Break(())
                    }
                })?;
                if let Some(e) = error {
                    return Err(e);
                }
            }
            Value::Error(e) if e.has_edges => self.stream_edges(&e.data, out)?,
            _ => (),
        }
        Ok(())
    }
}

pub(super) fn tuple_tree(tuple: &TupleRef, page: PageRef) -> Tree<Value> {
    let tree = Tree::<Value>::open(Some(page), tuple.summary.len, tuple.summary.cost, true);
    match tuple.family {
        Some(family) => tree.in_family(family),
        None => tree,
    }
}

fn visit_tree<T: Cell>(
    tree: &Tree<T>,
    access: &Access,
    visit: &mut dyn FnMut(&T) -> Result<(), HeapFault>,
) -> Result<(), HeapFault> {
    let mut error = None;
    tree.visit(access, &mut |_, v| match visit(v) {
        Ok(()) => true,
        Err(e) => {
            error = Some(e);
            false
        }
    })?;
    error.map_or(Ok(()), Err)
}
