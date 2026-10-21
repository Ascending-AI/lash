//! Frozen occurrence pages and root-preserving cell carry (S8).
use super::directory::{
    Body, Entry, ObjectHead, Ordinal, ordinal_key, value_children, value_cost, value_grants,
};
use super::tree::{Access, Cell, SealWork, Tree};
use super::*;
use lash_kernel_doc::heap::*;
use lash_kernel_doc::heap_values::{decode_value_with_family, encode_relative};
use lash_vocabulary::names::Name;
use lash_vocabulary::tokens::{TokenCodec, TokenError, TokenReader, TokenWriter, encode_body};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ops::ControlFlow;
use std::sync::Arc;

mod datum;
mod read;

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
struct RelativeValue(PageValue);
fn relative(value: &PageValue, out: &mut TokenWriter) {
    match value {
        PageValue::List(id) => encode_relative(0, id.ordinal, out),
        PageValue::Map(id) => encode_relative(1, id.ordinal, out),
        PageValue::Set(id) => encode_relative(2, id.ordinal, out),
        PageValue::Record(id) => encode_relative(3, id.ordinal, out),
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
#[derive(Clone)]
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
fn read_header(access: &Access<'_>, frozen: &FrozenRef) -> Result<FrozenHeader, HeapFault> {
    decode_header(&access.read(frozen.root)?, frozen)
}
fn decode_header(answer: &PageAnswer, frozen: &FrozenRef) -> Result<FrozenHeader, HeapFault> {
    if answer.page.id != frozen.root.id || answer.kind != PageKind::FROZEN_ROOT {
        return Err(invalid("expected frozen root"));
    }
    let mut input = TokenReader::new(&answer.tokens);
    let mut header = FrozenHeader::decode(&mut input).map_err(|e| invalid(e.to_string()))?;
    input.finish().map_err(|e| invalid(e.to_string()))?;
    if header.charge == 0
        || header.summary != frozen.summary
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
    validate_occurrence(
        &header.value.0,
        header.summary.entities,
        !header.summary.data_only,
    )?;
    header.objects.grants(&answer.children)?;
    header.value.grants(&answer.children)?;
    Ok(header)
}
fn validate_occurrence(value: &PageValue, extent: u64, closures: bool) -> Result<(), HeapFault> {
    match value {
        PageValue::List(id) | PageValue::Map(id) | PageValue::Set(id) | PageValue::Record(id) => {
            if id.family != RELATIVE || id.ordinal >= extent {
                return Err(invalid("frozen root occurrence is outside its family"));
            }
        }
        PageValue::Tuple(t) => {
            if let Payload::Resident(values) = &t.body {
                for value in values.iter() {
                    validate_occurrence(value, extent, closures)?;
                }
            }
        }
        PageValue::Error(e) => {
            if let Payload::Resident(data) = &e.data {
                validate_occurrence(data, extent, closures)?;
            }
        }
        PageValue::Closure(id) if closures && id.family != RELATIVE => {}
        PageValue::Closure(_) | PageValue::Task(_) | PageValue::Ref(_) => {
            return Err(invalid("a frozen root contains a runtime identity"));
        }
        _ => (),
    }
    Ok(())
}
fn context(value: &mut PageValue, family: Option<FamilyId>) -> Result<(), HeapFault> {
    match value {
        PageValue::List(id) | PageValue::Map(id) | PageValue::Set(id) | PageValue::Record(id) => {
            id.family =
                family.ok_or_else(|| invalid("zero-entity frozen root contains an occurrence"))?
        }
        PageValue::Tuple(t) => Arc::make_mut(t).family = family,
        PageValue::Error(e) => {
            let error = Arc::make_mut(e);
            error.family = family;
            if let Payload::Resident(data) = &mut error.data {
                context(Arc::make_mut(data), family)?;
            }
        }
        _ => (),
    }
    Ok(())
}
/// Resolves just the requested occurrence's index path, never its members.
/// Its pages are read through the heap's residency, so each is decoded
/// once while resident however often a run reads the family (FIG-6318).
#[derive(Default)]
pub struct FrozenBase {
    /// The decoded headers of the families read, by root page; at most
    /// [`FROZEN_HEADERS`].
    headers: BTreeMap<PageId, (FrozenRef, FrozenHeader)>,
}
/// How many decoded family headers a reader holds before it starts over.
const FROZEN_HEADERS: usize = 256;
impl FamilyBase for FrozenBase {
    fn base_object(
        &mut self,
        entity: EntityId,
        frozen: &FrozenRef,
        access: &Access<'_>,
    ) -> Result<Option<ObjectHead>, HeapFault> {
        if !self
            .headers
            .get(&frozen.root.id)
            .is_some_and(|(held, _)| held.root == frozen.root && held.summary == frozen.summary)
        {
            let answer = access.read(frozen.root)?;
            let header = decode_header(&answer, frozen)?;
            if self.headers.len() >= FROZEN_HEADERS {
                self.headers.clear();
            }
            self.headers.insert(frozen.root.id, (*frozen, header));
        }
        let header = &self
            .headers
            .get(&frozen.root.id)
            .ok_or_else(|| invalid("frozen header cache missing"))?
            .1;
        let head = header.objects.get(&entity.ordinal.to_be_bytes(), access)?;
        Ok(head.map(|mut head| {
            head.family = entity.family;
            match &mut head.body {
                Body::List { rows, .. } => *rows = rows.clone().in_family(entity.family),
                Body::Table { rows, .. } => *rows = rows.clone().in_family(entity.family),
                _ => (),
            }
            head
        }))
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
    named: &'a BTreeSet<EntityId>,
    functions: BTreeSet<EntityId>,
    seen: BTreeMap<EntityId, u64>,
}
impl Freezer<'_> {
    fn sequence(&self, values: Vec<PageValue>, nested: bool) -> Result<Tree<PageValue>, HeapFault> {
        let mut rows = Tree::new(true);
        for (i, value) in values.into_iter().enumerate() {
            rows.write(
                ordinal_key(i as u64),
                RelativeValue(value),
                &self.heap.access,
            )?;
        }
        if nested {
            rows.seal_nested(&self.heap.access, &mut SealWork::new(budget()))?;
        } else {
            rows.seal(&self.heap.access, &mut SealWork::new(budget()))?;
        }
        Ok(rows.retype()?.in_family(RELATIVE))
    }
    fn value(&mut self, value: &PageValue) -> Result<PageValue, HeapWriteError> {
        if self.depth >= 128 {
            return Err(nondata());
        }
        self.depth += 1;
        self.charge = plus(self.charge, logical_size(self.heap, value)?)?;
        let result = match value {
            PageValue::List(id)
            | PageValue::Map(id)
            | PageValue::Set(id)
            | PageValue::Record(id) => self.object(*id, value),
            PageValue::Closure(id) if self.named.contains(id) => {
                self.functions.insert(*id);
                Ok(value.clone())
            }
            PageValue::Closure(_) => Err(nondata()),
            PageValue::Function(_) => Ok(value.clone()),
            PageValue::Task(_) | PageValue::Ref(_) => Err(nondata()),
            PageValue::Tuple(t) => {
                let mut values = Vec::new();
                self.heap.tuple_visit(t, &mut |v| {
                    values.push(v.clone());
                    ControlFlow::Continue(())
                })?;
                let values = values
                    .iter()
                    .map(|v| self.value(v))
                    .collect::<Result<Vec<_>, _>>()?;
                let has_edges = values.iter().any(super::directory::value_has_edges);
                let depth = values
                    .iter()
                    .map(super::heap_impl::value_depth)
                    .max()
                    .unwrap_or(0)
                    + 1;
                let rows = self.sequence(values, false)?;
                let mut summary = t.summary;
                summary.cost = LogicalBytes(rows.cost());
                summary.has_edges = has_edges;
                summary.task_edges = 0;
                summary.depth = depth;
                Ok(PageValue::Tuple(Arc::new(TupleRef {
                    normal: Default::default(),
                    family: None,
                    summary,
                    body: rows
                        .page()
                        .map_or_else(|| Payload::Resident(Arc::from([])), Payload::paged),
                })))
            }
            PageValue::Error(e) => {
                let data = self.value(&self.heap.error_data(e)?)?;
                Ok(PageValue::Error(Arc::new(frozen_error(
                    &self.heap.access,
                    e.kind.clone(),
                    e.message.clone(),
                    data,
                )?)))
            }
            _ => Ok(value.clone()),
        };
        self.depth -= 1;
        result
    }
    fn object(&mut self, id: EntityId, value: &PageValue) -> Result<PageValue, HeapWriteError> {
        if let Some(ordinal) = self.seen.get(&id).copied() {
            if self.active.contains(&id) && !self.named.contains(&id) {
                return Err(nondata());
            }
            let id = EntityId {
                family: RELATIVE,
                ordinal,
            };
            return Ok(match value {
                PageValue::Closure(_) => PageValue::Closure(id),
                PageValue::List(_) => PageValue::List(id),
                PageValue::Map(_) => PageValue::Map(id),
                PageValue::Set(_) => PageValue::Set(id),
                _ => PageValue::Record(id),
            });
        }
        if !self.active.insert(id) {
            return Err(nondata());
        }
        let ordinal = self.entities;
        self.entities = plus(self.entities, 1)?;
        self.seen.insert(id, ordinal);
        let mut elements = Vec::new();
        if !matches!(value, PageValue::Closure(_)) {
            self.heap.visit(id, &mut |e| {
                elements.push(match e {
                    Element::Item(v) => (v.clone(), PageValue::Absent),
                    Element::Entry { key, value } => (key.clone(), value.clone()),
                    Element::Field { name, value } => {
                        (PageValue::Text(name.clone()), value.clone())
                    }
                });
                ControlFlow::Continue(())
            })?;
        }
        let body = if matches!(value, PageValue::List(_)) {
            let values = elements
                .iter()
                .map(|(v, _)| self.value(v))
                .collect::<Result<Vec<_>, _>>()?;
            let rows = self.sequence(values, true)?;
            let next = rows.len();
            Body::List { rows, next }
        } else {
            let kind = match value {
                PageValue::Map(_) => ObjectKind::Map,
                PageValue::Set(_) => ObjectKind::Set,
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
                        PageValue::Absent
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
            lookup.seal_nested(&self.heap.access, &mut SealWork::new(budget()))?;
            rows.seal_nested(&self.heap.access, &mut SealWork::new(budget()))?;
            Body::Table {
                kind,
                lookup,
                next: rows.len(),
                rows: rows.retype()?.in_family(RELATIVE),
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
            PageValue::List(_) => PageValue::List(id),
            PageValue::Map(_) => PageValue::Map(id),
            PageValue::Set(_) => PageValue::Set(id),
            PageValue::Closure(_) => PageValue::Closure(id),
            _ => PageValue::Record(id),
        })
    }
}
impl Freezer<'_> {
    fn finish(
        mut self,
        value: PageValue,
    ) -> Result<(FrozenRef, BTreeSet<EntityId>), HeapWriteError> {
        self.cost = plus(self.cost, value_cost(&value))?;
        self.objects
            .seal_nested(&self.heap.access, &mut SealWork::new(budget()))?;
        let summary = FrozenSummary {
            entities: self.entities,
            cost: LogicalBytes(self.cost),
            data_only: self.functions.is_empty(),
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
        header.objects.children(&mut children);
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
        value: &PageValue,
        named: &BTreeSet<EntityId>,
    ) -> Result<(FrozenRef, BTreeSet<EntityId>), HeapWriteError> {
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
            seen: BTreeMap::new(),
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
fn logical_size(heap: &PagedHeap, value: &PageValue) -> Result<u64, HeapFault> {
    let extent = match value {
        PageValue::Int(IntRef::Inline(i)) => i.bits().div_ceil(64),
        PageValue::Int(IntRef::Paged { summary, .. }) => summary.bits.div_ceil(64),
        PageValue::Text(t) => t.summary.utf8_bytes,
        PageValue::Bytes(b) => b.len,
        PageValue::Tuple(t) => t.summary.len,
        PageValue::Error(e) => plus(e.kind.summary.utf8_bytes, e.message.summary.utf8_bytes)?,
        PageValue::List(id) | PageValue::Set(id) | PageValue::Record(id) => heap.len(*id)?,
        PageValue::Map(id) => heap
            .len(*id)?
            .checked_mul(2)
            .ok_or_else(|| invalid("map extent overflow"))?,
        _ => 0,
    };
    plus(extent, 1)
}
impl HeapLifecycle for PagedHeap {
    fn freeze(&mut self, value: &PageValue, _data_only: bool) -> Result<FrozenRef, HeapWriteError> {
        self.freeze_named(value, &BTreeSet::new())
            .map(|(frozen, _)| frozen)
    }
    fn instantiate(&mut self, frozen: &FrozenRef) -> Result<PageValue, HeapWriteError> {
        let header = read_header(&self.access, frozen)?;
        let family = if frozen.summary.entities == 0 {
            None
        } else {
            Some(self.register_family(*frozen)?)
        };
        let mut value = header.value.0.clone();
        // The installed reader keeps the families it decoded (FIG-6318).
        if self.family_base.borrow().is_none() {
            self.set_family_base(Box::new(FrozenBase::default()));
        }
        context(&mut value, family)?;
        Ok(value)
    }
    fn classify_carry(&mut self, bindings: &KeyedRoot) -> Result<CarryClassification, HeapFault> {
        self.classify_carry_with_saved(bindings, &BTreeSet::new())
    }
    fn carry(&mut self, roots: CarryRoots) -> Result<SessionRoot, HeapFault> {
        self.set_saved_functions(&roots.functions);
        let mut saved = BTreeSet::new();
        Tree::<FunctionRecord>::of_keyed(&roots.functions).visit(
            &self.access,
            &mut |_, record| {
                saved.insert(record.environment);
                true
            },
        )?;
        let classification = self.classify_carry_with_saved(&roots.bindings, &saved)?;
        let bindings = classification.carried;
        // A selected root is already sealed. Replacing it updates only its
        // holding summary and the root, never reconstructing its members.
        if self.session.page().map(|p| p.id) != bindings.page().map(|p| p.id)
            || self.session.len() != bindings.len()
            || self.session.cost() != bindings.cost().0
        {
            self.set_session_root(Tree::of_keyed(&bindings))
                .map_err(|error| match error {
                    HeapWriteError::Fault(error) => error,
                    other => invalid(format!("session root transfer failed: {other}")),
                })?;
        }
        // A session already classifying forbidden identities sheds its
        // garbage too; data-only sessions pay for collection with allocation
        // since the last live estimate.
        if self.collects_at_carry() {
            self.collect()?;
        }
        self.carried_collection_estimate();
        // A carried session holds nothing in a run's memory: the next cell
        // counts what it reads (FIG-6791).
        self.record_capacity(0);
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
        self.set_family_base(Box::new(FrozenBase::default()));
        // The heap-header certificate already counts what the bindings
        // name. Attach the admitted root without counting it again.
        self.session = Tree::of_keyed(&session.bindings);
        self.set_saved_functions(&session.functions);
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

fn frozen_error(
    access: &Access<'_>,
    kind: TextRef,
    message: TextRef,
    data: PageValue,
) -> Result<ErrorRef, HeapFault> {
    let mut rows = Tree::new(true);
    rows.write(ordinal_key(0), RelativeValue(data.clone()), access)?;
    rows.seal(access, &mut SealWork::new(budget()))?;
    let cost = LogicalBytes(plus(
        plus(kind.summary.utf8_bytes, message.summary.utf8_bytes)?,
        value_cost(&data),
    )?);
    Ok(ErrorRef {
        family: None,
        task_edges: 0,
        kind,
        message,
        depth: super::heap_impl::value_depth(&data) + 1,
        cost,
        has_edges: super::directory::value_has_edges(&data),
        data: Payload::paged(
            rows.page()
                .ok_or_else(|| invalid("error data page missing"))?,
        ),
    })
}

impl PagedHeap {
    fn classify_carry_with_saved(
        &mut self,
        bindings: &KeyedRoot,
        saved: &BTreeSet<EntityId>,
    ) -> Result<CarryClassification, HeapFault> {
        let certificate = self.data_only_certificate();
        if certificate.closure_entities == 0 && certificate.task_edges == 0 {
            return Ok(CarryClassification {
                carried: *bindings,
                dropped: Vec::new(),
            });
        }
        let mut table = Tree::<BindingRecord>::of_keyed(bindings);
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
                    if self.kind(id)? == Some(ObjectKind::Closure) && !saved.contains(&id) {
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
                dropped.push(record.name.resolve(&self.access)?);
                table.remove(key, &self.access)?;
            }
        }
        table.seal(&self.access, &mut SealWork::new(budget()))?;
        Ok(CarryClassification {
            carried: super::heap_impl::keyed(&table)?,
            dropped,
        })
    }
}
