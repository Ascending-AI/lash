//! Frozen occurrence pages and root-preserving cell carry (S8).
use super::directory::{
    Body, Entry, ObjectHead, Ordinal, ordinal_key, value_children, value_cost, value_grants,
};
use super::tree::{Access, Cell, SealWork, Tree};
use super::*;
use lash_kernel_doc::Name;
use lash_kernel_doc::graph_tokens::{
    TokenCodec, TokenError, TokenReader, TokenWriter, encode_body,
};
use lash_kernel_doc::heap::*;
use lash_kernel_doc::heap_values::{decode_value_with_family, encode_relative};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ops::ControlFlow;
use std::sync::Arc;

mod datum;

const RELATIVE: FamilyId = FamilyId(u64::MAX);
fn budget() -> SealBudget {
    SealBudget {
        pages: u64::MAX,
        bytes: u64::MAX,
    }
}
fn invalid(problem: impl Into<String>) -> HeapFault {
    HeapFault::Malformed(problem.into())
}
fn nondata() -> HeapWriteError {
    HeapWriteError::Raised {
        kind: "not_data",
        message: "a frozen value cannot contain a cycle, closure, task or identity reference"
            .into(),
    }
}
fn plus(a: u64, b: u64) -> Result<u64, HeapFault> {
    a.checked_add(b)
        .ok_or_else(|| invalid("frozen summary overflow"))
}

/// Writer cells use the relative-occurrence grammar. On access the normal
/// heap tree decodes them under the receiving family's context.
#[derive(Clone)]
struct RelativeValue(Value);
fn relative(value: &Value, out: &mut TokenWriter) {
    match value {
        Value::List(id) => encode_relative(0, id.ordinal, out),
        Value::Map(id) => encode_relative(1, id.ordinal, out),
        Value::Set(id) => encode_relative(2, id.ordinal, out),
        Value::Record(id) => encode_relative(3, id.ordinal, out),
        Value::Error(e) => {
            out.sum(14, 1);
            out.seq(5);
            e.kind.encode(out);
            e.message.encode(out);
            relative(&e.data, out);
            e.cost.encode(out);
            e.has_edges.encode(out);
        }
        _ => value.encode(out),
    }
}
impl TokenCodec for RelativeValue {
    fn encode(&self, out: &mut TokenWriter) {
        relative(&self.0, out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        decode_value_with_family(input, Some(RELATIVE)).map(Self)
    }
}
impl Cell for RelativeValue {
    fn cost(&self) -> u64 {
        value_cost(&self.0)
    }
    fn children(&self, out: &mut Vec<PageRef>) {
        value_children(&self.0, out);
    }
    fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
        value_grants(&mut self.0, children)
    }
}
#[derive(Clone)]
struct RelativeEntry(Entry);
impl TokenCodec for RelativeEntry {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        relative(&self.0.key, out);
        relative(&self.0.value, out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self(Entry {
            key: RelativeValue::decode(input)?.0,
            value: RelativeValue::decode(input)?.0,
        }))
    }
}
impl Cell for RelativeEntry {
    fn cost(&self) -> u64 {
        self.0.cost()
    }
    fn children(&self, out: &mut Vec<PageRef>) {
        self.0.children(out);
    }
    fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
        self.0.grants(children)
    }
}
struct FrozenHeader {
    charge: u64,
    summary: FrozenSummary,
    value: RelativeValue,
    objects: Tree<ObjectHead>,
}
impl TokenCodec for FrozenHeader {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(4);
        self.charge.encode(out);
        self.summary.encode(out);
        self.value.encode(out);
        self.objects.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(4)?;
        Ok(Self {
            charge: u64::decode(input)?,
            summary: FrozenSummary::decode(input)?,
            value: RelativeValue::decode(input)?,
            objects: Tree::decode(input)?,
        })
    }
}
fn read_header(access: &Access, frozen: &FrozenRef) -> Result<FrozenHeader, HeapFault> {
    let answer = access.read(frozen.root)?;
    if answer.kind != PageKind::FROZEN_ROOT {
        return Err(invalid("expected frozen root"));
    }
    let mut input = TokenReader::new(&answer.tokens);
    let mut header = FrozenHeader::decode(&mut input).map_err(|e| invalid(e.to_string()))?;
    input.finish().map_err(|e| invalid(e.to_string()))?;
    if header.summary != frozen.summary
        || header.objects.len() != header.summary.entities
        || header.summary.cost.0 != plus(header.objects.cost(), value_cost(&header.value.0))?
        || header.summary.cost.0
            < header
                .summary
                .entities
                .checked_mul(32)
                .ok_or_else(|| invalid("frozen population overflow"))?
    {
        return Err(invalid("frozen root summary mismatch"));
    }
    header.objects.grants(&answer.children)?;
    header.value.grants(&answer.children)?;
    Ok(header)
}
fn context(value: &mut Value, family: Option<FamilyId>) -> Result<(), HeapFault> {
    match value {
        Value::List(id) | Value::Map(id) | Value::Set(id) | Value::Record(id) => {
            id.family =
                family.ok_or_else(|| invalid("zero-entity frozen root contains an occurrence"))?
        }
        Value::Tuple(t) => t.family = family,
        Value::Error(e) => context(Arc::make_mut(&mut Arc::make_mut(e).data), family)?,
        _ => (),
    }
    Ok(())
}
/// Resolves just the requested occurrence's index path, never its members.
pub struct FrozenBase;
impl FamilyBase for FrozenBase {
    fn base_object(
        &mut self,
        entity: EntityId,
        frozen: &FrozenRef,
        pages: &mut dyn PageSource,
    ) -> Result<Option<ObjectHead>, HeapFault> {
        // Access owns ports; this borrowed port forwards only this synchronous read.
        let answer = pages.read(&[frozen.root])?;
        let [answer] = answer.as_slice() else {
            return Err(invalid("frozen port returned wrong count"));
        };
        if answer.page.id != frozen.root.id || answer.kind != PageKind::FROZEN_ROOT {
            return Err(invalid("expected frozen root"));
        }
        let mut input = TokenReader::new(&answer.tokens);
        let mut header = FrozenHeader::decode(&mut input).map_err(|e| invalid(e.to_string()))?;
        input.finish().map_err(|e| invalid(e.to_string()))?;
        if header.summary != frozen.summary || header.objects.len() != frozen.summary.entities {
            return Err(invalid("frozen root summary mismatch"));
        }
        header.objects.grants(&answer.children)?;
        // A narrow local forwarding port avoids retaining an activation source.
        let mut requested = Vec::new();
        let objects = header.objects;
        loop {
            let access = Access::new(Box::new(Replay {
                answers: requested.clone(),
            }));
            match objects.get(&ordinal_key(entity.ordinal), &access) {
                Ok(head) => {
                    return Ok(head.map(|mut h| {
                        h.family = entity.family;
                        match &mut h.body {
                            Body::List { rows, .. } => {
                                *rows = rows.clone().in_family(entity.family)
                            }
                            Body::Table { rows, .. } => {
                                *rows = rows.clone().in_family(entity.family)
                            }
                            _ => (),
                        }
                        h
                    }));
                }
                Err(HeapFault::Missing { page }) => {
                    // The tree discovered the grant from an authenticated parent.
                    let grant = requested
                        .iter()
                        .flat_map(|a: &PageAnswer| &a.children)
                        .chain(&answer.children)
                        .find(|p| p.id == page)
                        .copied()
                        .ok_or_else(|| invalid("missing frozen child grant"))?;
                    let mut next = pages.read(&[grant])?;
                    if next.len() != 1 || next[0].page.id != page {
                        return Err(invalid("frozen port returned wrong page"));
                    }
                    requested.push(next.remove(0));
                }
                Err(error) => return Err(error),
            }
        }
    }
}
struct Replay {
    answers: Vec<PageAnswer>,
}
impl PageSource for Replay {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<PageAnswer>, HeapFault> {
        pages
            .iter()
            .map(|p| {
                self.answers
                    .iter()
                    .find(|a| a.page.id == p.id)
                    .cloned()
                    .ok_or(HeapFault::Missing { page: p.id })
            })
            .collect()
    }
    fn seal(&mut self, _: PageBody) -> Result<PageRef, HeapFault> {
        Err(invalid("frozen reader cannot seal"))
    }
}

struct Freezer<'a> {
    heap: &'a PagedHeap,
    objects: Tree<ObjectHead>,
    active: BTreeSet<EntityId>,
    entities: u64,
    cost: u64,
    charge: u64,
    depth: usize,
    named: &'a BTreeMap<EntityId, Name>,
    functions: BTreeSet<Name>,
}
impl Freezer<'_> {
    fn sequence(&self, values: Vec<Value>) -> Result<Tree<Value>, HeapFault> {
        let mut rows = Tree::new(true);
        for (i, value) in values.into_iter().enumerate() {
            rows.write(
                ordinal_key(i as u64),
                RelativeValue(value),
                &self.heap.access,
            )?;
        }
        rows.seal(&self.heap.access, &mut SealWork::new(budget()))?;
        Ok(Tree::open(
            rows.page(),
            rows.len(),
            LogicalBytes(rows.cost()),
            true,
        ))
    }
    fn value(&mut self, value: &Value) -> Result<Value, HeapWriteError> {
        if self.depth >= 128 {
            return Err(nondata());
        }
        self.depth += 1;
        self.charge = plus(self.charge, logical_size(self.heap, value)?)?;
        let result = match value {
            Value::List(id) | Value::Map(id) | Value::Set(id) | Value::Record(id) => {
                self.object(*id, value)
            }
            Value::Closure(id) => match self.named.get(id) {
                Some(name) => {
                    self.functions.insert(name.clone());
                    Ok(Value::Function(name.clone()))
                }
                None => Err(nondata()),
            },
            Value::Function(name) => {
                self.functions.insert(name.clone());
                Ok(value.clone())
            }
            Value::Task(_) | Value::Ref(_) => Err(nondata()),
            Value::Tuple(t) => {
                let mut values = Vec::new();
                self.heap.tuple_visit(t, &mut |v| {
                    values.push(v.clone());
                    ControlFlow::Continue(())
                })?;
                let values = values
                    .iter()
                    .map(|v| self.value(v))
                    .collect::<Result<Vec<_>, _>>()?;
                let rows = self.sequence(values)?;
                let mut summary = t.summary;
                summary.cost = LogicalBytes(rows.cost());
                Ok(Value::Tuple(TupleRef {
                    family: None,
                    summary,
                    body: rows
                        .page()
                        .map_or_else(|| Payload::Resident(Arc::from([])), Payload::Paged),
                }))
            }
            Value::Error(e) => {
                let mut e = (**e).clone();
                e.data = Arc::new(self.value(&e.data)?);
                Ok(Value::Error(Arc::new(e)))
            }
            _ => Ok(value.clone()),
        };
        self.depth -= 1;
        result
    }
    fn object(&mut self, id: EntityId, value: &Value) -> Result<Value, HeapWriteError> {
        if !self.active.insert(id) {
            return Err(nondata());
        }
        let ordinal = self.entities;
        self.entities = plus(self.entities, 1)?;
        let mut elements = Vec::new();
        self.heap.visit(id, &mut |e| {
            elements.push(match e {
                Element::Item(v) => (v.clone(), Value::Absent),
                Element::Entry { key, value } => (key.clone(), value.clone()),
                Element::Field { name, value } => (Value::Text(name.clone()), value.clone()),
            });
            ControlFlow::Continue(())
        })?;
        let body = if matches!(value, Value::List(_)) {
            let values = elements
                .iter()
                .map(|(v, _)| self.value(v))
                .collect::<Result<Vec<_>, _>>()?;
            let rows = self.sequence(values)?;
            let next = rows.len();
            Body::List {
                inline: None,
                rows,
                next,
            }
        } else {
            let kind = match value {
                Value::Map(_) => ObjectKind::Map,
                Value::Set(_) => ObjectKind::Set,
                _ => ObjectKind::Record,
            };
            let mut lookup = Tree::new(false);
            let mut rows = Tree::new(true);
            for (i, (key, value)) in elements.iter().enumerate() {
                let normal = self.heap.normal_key(key)?.ok_or_else(nondata)?;
                let key_bytes = match normal {
                    NormalKey::Inline(v) => [vec![0], v].concat(),
                    NormalKey::Digest(v) => [vec![1], v.to_vec()].concat(),
                };
                let entry = Entry {
                    key: if kind == ObjectKind::Record {
                        key.clone()
                    } else {
                        self.value(key)?
                    },
                    value: if kind == ObjectKind::Set {
                        Value::Absent
                    } else {
                        self.value(value)?
                    },
                };
                lookup.write(key_bytes, Ordinal(i as u64), &self.heap.access)?;
                rows.write(
                    ordinal_key(i as u64),
                    RelativeEntry(entry),
                    &self.heap.access,
                )?;
            }
            lookup.seal(&self.heap.access, &mut SealWork::new(budget()))?;
            rows.seal(&self.heap.access, &mut SealWork::new(budget()))?;
            Body::Table {
                kind,
                lookup,
                rows: Tree::open(rows.page(), rows.len(), LogicalBytes(rows.cost()), true),
                next: rows.len(),
            }
        };
        let head = ObjectHead {
            family: FamilyId::PRIMARY,
            body,
        };
        self.cost = plus(self.cost, head.cost())?;
        self.objects
            .write(ordinal_key(ordinal), head, &self.heap.access)?;
        self.active.remove(&id);
        let id = EntityId {
            family: RELATIVE,
            ordinal,
        };
        Ok(match value {
            Value::List(_) => Value::List(id),
            Value::Map(_) => Value::Map(id),
            Value::Set(_) => Value::Set(id),
            _ => Value::Record(id),
        })
    }
}
impl Freezer<'_> {
    fn finish(mut self, value: Value) -> Result<(FrozenRef, BTreeSet<Name>), HeapWriteError> {
        self.cost = plus(self.cost, value_cost(&value))?;
        self.objects
            .seal(&self.heap.access, &mut SealWork::new(budget()))?;
        let summary = FrozenSummary {
            entities: self.entities,
            cost: LogicalBytes(self.cost),
            data_only: true,
        };
        let functions = self.functions;
        let header = FrozenHeader {
            charge: self.charge,
            summary,
            value: RelativeValue(value),
            objects: self.objects,
        };
        let mut children = Vec::new();
        header.value.children(&mut children);
        children.extend(header.objects.page());
        let root = self
            .heap
            .access
            .seal(PageKind::FROZEN_ROOT, encode_body(&header), children)?;
        Ok((FrozenRef { root, summary }, functions))
    }
}
impl PagedHeap {
    /// Saved captures may replace named closure occurrences with self-contained
    /// declared-function references, never with a live-directory pointer.
    pub fn freeze_named(
        &mut self,
        value: &Value,
        named: &BTreeMap<EntityId, Name>,
    ) -> Result<(FrozenRef, BTreeSet<Name>), HeapWriteError> {
        let mut freezer = Freezer {
            heap: self,
            objects: Tree::new(false),
            active: BTreeSet::new(),
            entities: 0,
            cost: 0,
            charge: 0,
            depth: 0,
            named,
            functions: BTreeSet::new(),
        };
        let value = freezer.value(value)?;
        freezer.finish(value)
    }
}
impl PagedHeap {
    /// The charge preserved at the semantic freeze boundary; no object walk.
    pub fn frozen_charge(&self, frozen: &FrozenRef) -> Result<u64, HeapFault> {
        Ok(read_header(&self.access, frozen)?.charge)
    }
}
fn logical_size(heap: &PagedHeap, value: &Value) -> Result<u64, HeapFault> {
    let extent = match value {
        Value::Int(IntRef::Inline(i)) => i.bits().div_ceil(64),
        Value::Int(IntRef::Paged { summary, .. }) => summary.bits.div_ceil(64),
        Value::Text(t) => t.summary.utf8_bytes,
        Value::Bytes(b) => b.len,
        Value::Tuple(t) => t.summary.len,
        Value::Error(e) => plus(e.kind.summary.utf8_bytes, e.message.summary.utf8_bytes)?,
        Value::List(id) | Value::Set(id) | Value::Record(id) => heap.len(*id)?,
        Value::Map(id) => heap
            .len(*id)?
            .checked_mul(2)
            .ok_or_else(|| invalid("map extent overflow"))?,
        _ => 0,
    };
    plus(extent, 1)
}
impl HeapLifecycle for PagedHeap {
    fn freeze(&mut self, value: &Value, _data_only: bool) -> Result<FrozenRef, HeapWriteError> {
        self.freeze_named(value, &BTreeMap::new())
            .map(|(frozen, _)| frozen)
    }
    fn instantiate(&mut self, frozen: &FrozenRef) -> Result<Value, HeapWriteError> {
        let header = read_header(&self.access, frozen)?;
        let family = if frozen.summary.entities == 0 {
            self.reserve(0, frozen.summary.cost.0)
                .map_err(|error| match error {
                    NativeError::Heap(error) => HeapWriteError::Fault(error),
                    NativeError::Memory => HeapWriteError::Memory,
                    other => HeapWriteError::Fault(invalid(format!(
                        "frozen reservation failed: {other}"
                    ))),
                })?;
            None
        } else {
            Some(self.register_family(*frozen)?)
        };
        self.set_family_base(Box::new(FrozenBase));
        let mut value = header.value.0;
        context(&mut value, family)?;
        Ok(value)
    }
    fn classify_carry(&mut self, bindings: &KeyedRoot) -> Result<CarryClassification, HeapFault> {
        let certificate = self.data_only_certificate();
        if certificate.closure_entities == 0 && certificate.task_edges == 0 {
            return Ok(CarryClassification {
                carried: *bindings,
                dropped: Vec::new(),
            });
        }
        let mut table =
            Tree::<BindingRecord>::open(bindings.page, bindings.len, bindings.cost, false);
        let mut candidates = Vec::new();
        table.visit(&self.access, &mut |key, record| {
            candidates.push((key.to_vec(), record.clone()));
            true
        })?;
        let mut roots = Vec::new();
        let mut pending = VecDeque::new();
        for (_, record) in &candidates {
            let mut edges = Vec::new();
            match &record.value {
                BoundRecord::Value(value) => self.edges_of(value, &mut edges)?,
                BoundRecord::Shared(id) => edges.push(Identity::Object(*id)),
            }
            pending.extend(edges.iter().copied());
            roots.push(edges);
        }
        // One union graph, then reverse propagation from forbidden nodes.
        let mut seen = BTreeSet::new();
        let mut reverse: BTreeMap<Identity, Vec<Identity>> = BTreeMap::new();
        let mut bad = BTreeSet::new();
        while let Some(identity) = pending.pop_front() {
            if !seen.insert(identity) {
                continue;
            }
            match identity {
                Identity::Task(_) => {
                    bad.insert(identity);
                }
                Identity::Object(id) => {
                    if self.kind(id)? == Some(ObjectKind::Closure) {
                        bad.insert(identity);
                        continue;
                    }
                    let mut edges = Vec::new();
                    self.entity_edges(id, &mut |e| {
                        edges.push(e);
                        Ok(())
                    })?;
                    for edge in edges {
                        reverse.entry(edge).or_default().push(identity);
                        pending.push_back(edge);
                    }
                }
            }
        }
        let mut queue: VecDeque<_> = bad.iter().copied().collect();
        while let Some(identity) = queue.pop_front() {
            if let Some(parents) = reverse.get(&identity) {
                for parent in parents {
                    if bad.insert(*parent) {
                        queue.push_back(*parent);
                    }
                }
            }
        }
        let mut dropped = Vec::new();
        for ((key, record), edges) in candidates.iter().zip(roots) {
            if edges.iter().any(|e| bad.contains(e)) {
                dropped.push(record.name.clone());
                table.remove(key, &self.access)?;
            }
        }
        table.seal(&self.access, &mut SealWork::new(budget()))?;
        Ok(CarryClassification {
            carried: super::heap_impl::keyed(&table),
            dropped,
        })
    }
    fn carry(&mut self, roots: CarryRoots) -> Result<SessionRoot, HeapFault> {
        let classification = self.classify_carry(&roots.bindings)?;
        let bindings = classification.carried;
        // A selected root is already sealed. Replacing it updates only its
        // holding summary and the root, never reconstructing its members.
        self.set_session_root(Tree::open(
            bindings.page,
            bindings.len,
            bindings.cost,
            false,
        ))
        .map_err(|error| match error {
            HeapWriteError::Fault(error) => error,
            other => invalid(format!("session root transfer failed: {other}")),
        })?;
        self.seal(budget())?
            .ok_or_else(|| invalid("unbounded heap seal incomplete"))?;
        Ok(SessionRoot {
            heap: self
                .sealed_root()
                .ok_or_else(|| invalid("heap header missing"))?,
            bindings,
            functions: roots.functions,
            certificate: self.data_only_certificate(),
        })
    }
    fn adopt(&mut self, session: &SessionRoot) -> Result<KeyedRoot, HeapFault> {
        if self.sealed_root().map(|p| p.id) != Some(session.heap.id) {
            return Err(invalid("adopt requires the session's heap version"));
        }
        if session.certificate != self.data_only_certificate() {
            return Err(invalid(
                "session population certificate differs from its heap",
            ));
        }
        self.set_family_base(Box::new(FrozenBase));
        // The heap-header certificate already includes this binding holding
        // cost. Attach the admitted root without adding that cost again.
        self.session = Tree::open(
            session.bindings.page,
            session.bindings.len,
            session.bindings.cost,
            false,
        );
        Ok(session.bindings)
    }
}

/// Function tables retain the exact immutable bundle pages as their children.
impl Cell for PageRef {
    fn cost(&self) -> u64 {
        0
    }
    fn children(&self, out: &mut Vec<PageRef>) {
        out.push(*self);
    }
    fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
        super::tree::grant(self, children)
    }
}
