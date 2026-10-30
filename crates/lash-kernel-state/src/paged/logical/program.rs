//! Itemized source programs. Evaluation borrows facts; preparation owns grants;
//! publication transfers the canonical storage recipe with the ledger roots.
use super::*;
use crate::arena::{Arena, Registration};
use crate::paged::{FrozenRef, PagedHeap};
use lash_kernel_doc::{Value, arena::ArenaHandle, heap::PageValue};
use lash_kernel_value::tokens::encode_body;
use std::collections::BTreeSet;

#[derive(Clone)]
pub enum PublicationValue {
    Native(Value),
    Page(PageValue),
    Frozen(FrozenRef),
    Capture(ArenaHandle),
    Existing(Owner),
    Frame,
}
impl PublicationValue {
    fn root(&self) -> RootValue<'_> {
        match self {
            Self::Native(value) => RootValue::Value(value),
            Self::Page(value) => RootValue::Page(value),
            Self::Frozen(value) => RootValue::Frozen(value),
            Self::Capture(handle) => RootValue::Capture(*handle),
            Self::Existing(owner) => RootValue::Existing(*owner),
            Self::Frame => RootValue::Frame,
        }
    }
    fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Native(a), Self::Native(b)) => a.same(b),
            (Self::Page(a), Self::Page(b)) => encode_body(a) == encode_body(b),
            (Self::Frozen(a), Self::Frozen(b)) => a.root.id == b.root.id && a.summary == b.summary,
            (Self::Capture(a), Self::Capture(b)) => a == b,
            (Self::Existing(a), Self::Existing(b)) => a == b,
            (Self::Frame, Self::Frame) => true,
            _ => false,
        }
    }
    fn handles(&self, out: &mut Vec<ArenaHandle>) {
        match self {
            Self::Native(value) => out.extend(value.handle()),
            Self::Capture(handle) => out.push(*handle),
            _ => {}
        }
    }
}
/// Canonical source payloads. Their direct occurrence prices use Holding and
/// LogicalHeldV1; the executor supplies no byte delta or entity header price.
#[derive(Clone)]
pub enum VirtualBody {
    List(Vec<PageValue>),
    /// Evaluated resident members retain their native provenance and identities.
    NativeList(Vec<Value>),
    Variable(PageValue),
    Closure(Vec<EntityId>),
    Map(Vec<(PageValue, PageValue)>),
    Set(Vec<PageValue>),
    Record(Vec<(PageValue, PageValue)>),
}
impl VirtualBody {
    fn holding(&self, heap: &PagedHeap, arena: &mut Arena) -> Result<Holding, HeapFault> {
        use lash_kernel_doc::heap::PageReader;
        let access = heap.pages();
        let mut body = Holding::scalar(Cost::OBJECT);
        let mut add = |holding: Holding, field: bool| -> Result<(), HeapFault> {
            let bytes = holding
                .bytes
                .checked_sub(if field { Cost::REFERENCE } else { 0 })
                .ok_or_else(|| invalid("virtual field price underflow"))?;
            body.bytes = body
                .bytes
                .checked_add(bytes)
                .ok_or_else(|| invalid("virtual body cost overflow"))?;
            holding.each_edge(access, &mut |edge| {
                body.add_edge(edge.target, edge.count, access)
            })
        };
        match self {
            Self::NativeList(values) => {
                for value in values {
                    add(Holding::from_native(arena, access, value)?, false)?;
                }
            }
            Self::List(values) => {
                for value in values {
                    add(Holding::from_page(access, value)?, false)?;
                }
            }
            Self::Variable(value) => add(Holding::from_page(access, value)?, false)?,
            Self::Closure(captures) => {
                for entity in captures {
                    add(
                        Holding::from_page(access, &PageValue::Ref(Identity::Object(*entity)))?,
                        false,
                    )?;
                }
            }
            Self::Map(entries) | Self::Record(entries) => {
                let record = matches!(self, Self::Record(_));
                let mut keys = BTreeSet::new();
                for (key, value) in entries {
                    if record && !matches!(key, PageValue::Text(_)) {
                        return Err(invalid("record field is not text"));
                    }
                    let digest = heap
                        .normal_key(key)?
                        .ok_or_else(|| invalid("virtual table key is not a key"))?;
                    if !keys.insert(digest) {
                        return Err(invalid("virtual table key occurs twice"));
                    }
                    add(Holding::from_page(access, key)?, record)?;
                    add(Holding::from_page(access, value)?, false)?;
                }
            }
            Self::Set(values) => {
                let mut keys = BTreeSet::new();
                for key in values {
                    let digest = heap
                        .normal_key(key)?
                        .ok_or_else(|| invalid("virtual table key is not a key"))?;
                    if !keys.insert(digest) {
                        return Err(invalid("virtual set key occurs twice"));
                    }
                    add(Holding::from_page(access, key)?, false)?;
                    add(Holding::from_page(access, &PageValue::Null)?, false)?;
                }
            }
        }
        Ok(body)
    }
    fn same(&self, other: &Self) -> bool {
        let values = |a: &[PageValue], b: &[PageValue]| {
            a.len() == b.len()
                && a.iter()
                    .zip(b)
                    .all(|(a, b)| encode_body(a) == encode_body(b))
        };
        match (self, other) {
            (Self::List(a), Self::List(b)) | (Self::Set(a), Self::Set(b)) => values(a, b),
            (Self::NativeList(a), Self::NativeList(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.same(b))
            }
            (Self::Variable(a), Self::Variable(b)) => encode_body(a) == encode_body(b),
            (Self::Closure(a), Self::Closure(b)) => a == b,
            (Self::Map(a), Self::Map(b)) | (Self::Record(a), Self::Record(b)) => {
                a.len() == b.len()
                    && a.iter().zip(b).all(|((ak, av), (bk, bv))| {
                        encode_body(ak) == encode_body(bk) && encode_body(av) == encode_body(bv)
                    })
            }
            _ => false,
        }
    }
}
#[derive(Clone)]
pub struct VirtualEntity {
    pub entity: EntityId,
    pub body: VirtualBody,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FrameEvent {
    Enter { task: TaskId, invocation: u64 },
    Leave { task: TaskId, invocation: u64 },
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ScratchEvent {
    Begin { owner: Owner, bytes: u64 },
    End { owner: Owner },
}
#[derive(Clone, Default)]
pub struct JournalEvent {
    pub inputs: Vec<Owner>,
    pub outputs: Vec<(Owner, PublicationValue)>,
    pub member: Option<MemberEdit>,
    pub entities: Vec<VirtualEntity>,
    pub frame: Option<FrameEvent>,
    pub scratch: Vec<ScratchEvent>,
}
impl JournalEvent {
    fn same(&self, other: &Self) -> bool {
        self.inputs == other.inputs
            && self.frame == other.frame
            && self.scratch == other.scratch
            && self.outputs.len() == other.outputs.len()
            && self
                .outputs
                .iter()
                .zip(&other.outputs)
                .all(|((ao, av), (bo, bv))| ao == bo && av.same(bv))
            && self.entities.len() == other.entities.len()
            && self
                .entities
                .iter()
                .zip(&other.entities)
                .all(|(a, b)| a.entity == b.entity && a.body.same(&b.body))
            && match (self.member, other.member) {
                (None, None) => true,
                (Some(a), Some(b)) => {
                    a.handle == b.handle
                        && match (a.member, b.member) {
                            (Member::List(a), Member::List(b)) => a == b,
                            (Member::Variable, Member::Variable) => true,
                            (Member::Table(a), Member::Table(b)) => a.same(&b),
                            _ => false,
                        }
                        && match (a.next, b.next) {
                            (None, None) => true,
                            (Some(a), Some(b)) => a.same(&b),
                            _ => false,
                        }
                }
                _ => false,
            }
    }
    pub fn scratch_peak(&self) -> Option<u64> {
        let mut live = BTreeMap::new();
        let mut peak = 0_u64;
        for event in &self.scratch {
            match *event {
                ScratchEvent::Begin { owner, bytes } => {
                    if live.insert(owner, bytes).is_some() {
                        return None;
                    }
                }
                ScratchEvent::End { owner } => {
                    live.remove(&owner)?;
                }
            }
            peak = peak.max(
                live.values()
                    .try_fold(0_u64, |sum, bytes| sum.checked_add(*bytes))?,
            );
        }
        Some(peak)
    }
    fn handles(&self, handles: &mut Vec<ArenaHandle>) {
        for entity in &self.entities {
            if let VirtualBody::NativeList(values) = &entity.body {
                handles.extend(values.iter().filter_map(Value::handle));
            }
        }
        for (_, value) in &self.outputs {
            value.handles(handles);
        }
        if let Some(edit) = self.member {
            handles.push(edit.handle);
            handles.extend(edit.next.and_then(|v| v.handle()));
            if let Member::Table(key) = edit.member {
                handles.extend(key.handle());
            }
        }
    }
}
pub struct Program {
    events: Vec<JournalEvent>,
}
pub struct EvaluatedProgram {
    program: Program,
    dependencies: Dependencies,
    next_ordinal: u64,
}
pub struct PreparedProgram<'a> {
    publication: PreparedPublication<'a>,
    entities: Vec<VirtualEntity>,
}
pub struct AcceptedProgram<'a> {
    publication: AcceptedPublication<'a>,
    entities: Vec<VirtualEntity>,
}

/// Owns all data needed to realize published virtual identities. It is affine
/// and may be realized only before another source publication or hard handoff.
pub struct Materializations {
    stamp: Stamp,
    entities: Vec<VirtualEntity>,
}
impl Materializations {
    pub fn publish(
        self,
        ledger: &mut Ledger,
        storage: impl FnOnce(Vec<VirtualEntity>),
    ) -> Result<(), HeapFault> {
        if !ledger.is_current(&self.stamp) {
            return Err(invalid("stale materialization owner"));
        }
        // Reserve invalidation before the infallible storage commit.
        ledger.handoff()?;
        storage(self.entities);
        Ok(())
    }
    pub fn entities(&self) -> &[VirtualEntity] {
        &self.entities
    }
}
impl Program {
    pub fn new(events: Vec<JournalEvent>) -> Self {
        Self { events }
    }
    pub fn events(&self) -> &[JournalEvent] {
        &self.events
    }
    /// Pure: capture source dependencies and the exact storage/owner program.
    /// No registrations, logical transfers, normalization or guest allocation.
    pub fn evaluate(self, ledger: &Ledger, arena: &Arena) -> Option<EvaluatedProgram> {
        let mut handles = Vec::new();
        for event in &self.events {
            event.handles(&mut handles);
        }
        Some(EvaluatedProgram {
            dependencies: Dependencies::capture(ledger, arena, &handles)?,
            next_ordinal: arena.next_ordinal(),
            program: self,
        })
    }
}
impl EvaluatedProgram {
    pub fn bind_dependencies(
        mut self,
        ledger: &Ledger,
        arena: &Arena,
        handles: &[ArenaHandle],
    ) -> Option<Self> {
        self.dependencies
            .extend(Dependencies::capture(ledger, arena, handles)?)
            .ok()?;
        Some(self)
    }

    pub fn is_current(&self, ledger: &Ledger, arena: &Arena) -> bool {
        self.dependencies.is_current(ledger, arena) && self.next_ordinal == arena.next_ordinal()
    }
    pub fn events(&self) -> &[JournalEvent] {
        &self.program.events
    }
    pub fn matches(&self, ledger: &Ledger, arena: &Arena, actual: &Program) -> bool {
        self.is_current(ledger, arena)
            && self.program.events.len() == actual.events.len()
            && self
                .program
                .events
                .iter()
                .zip(&actual.events)
                .all(|(a, b)| a.same(b))
    }
    /// The actual storage recipe must match the evaluated values and provenance,
    /// even when a different recipe would have exactly the same retained price.
    pub fn prepare<'a>(
        self,
        ledger: &Ledger,
        mut registration: Registration<'a>,
        heap: &PagedHeap,
        actual: Program,
    ) -> Result<PreparedProgram<'a>, MemberFault> {
        if !self.matches(ledger, registration.arena(), &actual) {
            return Err(invalid("source program or its facts changed").into());
        }
        let access = heap.pages();
        let mut draft = Ledger {
            state: ledger.state.clone(),
            identity: Rc::clone(&ledger.identity),
            revision: ledger.revision,
        };
        let before = ledger.balance()?.total()?;
        let mut peak = before;
        let mut projected = Vec::new();
        let mut virtuals = BTreeMap::<EntityId, VirtualBody>::new();
        let mut member_edits = Vec::new();
        let mut handles = Vec::new();
        let mut scratches = BTreeMap::<Owner, u64>::new();
        for event in actual.events {
            event.handles(&mut handles);
            let mut changes = Vec::new();
            let mut pending = Vec::new();
            let mut inputs = event.inputs;
            let mut outputs = event.outputs;
            let mut next_invocation = draft.state.next_invocation;
            if let Some(frame) = event.frame {
                match frame {
                    FrameEvent::Enter { task, invocation } => {
                        if invocation != next_invocation {
                            return Err(invalid("wrong source frame activation").into());
                        }
                        next_invocation = next_invocation
                            .checked_add(1)
                            .ok_or_else(|| invalid("invocation overflow"))?;
                        outputs.push((Owner::Frame { task, invocation }, PublicationValue::Frame));
                    }
                    FrameEvent::Leave { task, invocation } => {
                        let frame = draft.invocation(task, invocation, access)?;
                        inputs.extend(draft.invocation_owners(&frame, access)?);
                        if outputs.iter().any(|(owner,_)|matches!(owner,Owner::Frame {task:t,invocation:i}|Owner::Value {task:t,invocation:i,..} if *t==task&&*i==invocation)) { return Err(invalid("leaving frame retains its owner").into()); }
                    }
                }
            }
            let mut headers = Vec::new();
            let mut unique = BTreeSet::new();
            for entity in &event.entities {
                if !unique.insert(entity.entity) {
                    return Err(invalid("virtual entity occurs twice in an event").into());
                }
                if !virtuals.contains_key(&entity.entity) {
                    if registration.reserve_entity().map_err(MemberFault::Native)? != entity.entity
                        || draft.entity_holding(entity.entity, access)?.is_some()
                    {
                        return Err(invalid("virtual identity is not fresh").into());
                    }
                    headers.push(Change::Allocate {
                        entity: entity.entity,
                        body: Holding::scalar(Cost::OBJECT),
                    });
                }
                let body = entity.body.holding(heap, registration.arena())?;
                body.each_edge(access, &mut |edge| {
                    if let Identity::Object(id) = edge.target {
                        pending.push(id);
                    }
                    Ok(())
                })?;
                changes.push(Change::Mutate {
                    entity: entity.entity,
                    body,
                });
                virtuals.insert(entity.entity, entity.body.clone());
            }
            let roots: Vec<_> = outputs
                .iter()
                .map(|(owner, value)| (*owner, value.root()))
                .collect();
            let roots = draft.ownership_changes(
                registration.arena(),
                access,
                &inputs,
                &roots,
                &mut pending,
            )?;
            let mut roots = roots;
            let initial_scratch = scratches.clone();
            let mut scratch_peak = scratches.values().try_fold(0_u64, |sum, bytes| {
                sum.checked_add(*bytes)
                    .ok_or_else(|| invalid("scratch overflow"))
            })?;
            for scratch in event.scratch {
                match scratch {
                    ScratchEvent::Begin { owner, bytes } => {
                        if !matches!(
                            owner,
                            Owner::Value {
                                slot: Slot::Scratch(_),
                                ..
                            }
                        ) || draft.holding(owner, access)?.is_some()
                        {
                            return Err(invalid("scratch lease is not vacant").into());
                        }
                        if scratches.insert(owner, bytes).is_some() {
                            return Err(invalid("scratch lease occurs twice").into());
                        }
                    }
                    ScratchEvent::End { owner } => {
                        if !matches!(
                            owner,
                            Owner::Value {
                                slot: Slot::Scratch(_),
                                ..
                            }
                        ) {
                            return Err(invalid("scratch owner kind").into());
                        }
                        if scratches.remove(&owner).is_none() {
                            return Err(invalid("scratch lease was not begun").into());
                        }
                    }
                }
                scratch_peak =
                    scratch_peak.max(scratches.values().try_fold(0_u64, |sum, bytes| {
                        sum.checked_add(*bytes)
                            .ok_or_else(|| invalid("scratch overflow"))
                    })?);
            }
            for owner in initial_scratch.keys() {
                if !scratches.contains_key(owner) {
                    roots.push((*owner, None));
                }
            }
            for (owner, bytes) in &scratches {
                if !initial_scratch.contains_key(owner) {
                    roots.push((*owner, Some(Holding::scalar(*bytes))));
                }
            }
            if let Some(edit) = event.member {
                if member_edits
                    .iter()
                    .any(|old: &MemberEdit| old.handle == edit.handle)
                {
                    return Err(invalid("member edit needs a successor frontier").into());
                }
                let (change, edges, versions) = draft.member_change(
                    registration.arena(),
                    access,
                    edit.handle,
                    edit.member,
                    edit.next,
                )?;
                changes.push(change);
                pending.extend(edges);
                handles.extend(versions);
                member_edits.push(edit);
            }
            pending.retain(|id| !virtuals.contains_key(id));
            let mut allocations = draft.allocate_membership_from(
                registration.arena(),
                access,
                pending,
                Some(heap),
            )?;
            allocations.extend(headers);
            allocations.extend(changes);
            allocations.push(Change::Roots(roots));
            let mut prepared = draft.prepare_event(allocations, access)?;
            prepared.next.next_invocation = next_invocation;
            peak = peak.max(prepared.peak).max(
                draft
                    .balance()?
                    .total()?
                    .checked_add(scratch_peak)
                    .ok_or_else(|| invalid("scratch peak overflow"))?,
            );
            projected.extend(prepared.projected);
            draft.state = prepared.next;
        }
        if !scratches.is_empty() {
            return Err(invalid("source scratch survives publication").into());
        }
        for entity in &projected {
            if let Some(handle) = registration.arena().logical_handle(*entity) {
                handles.push(handle);
            }
        }
        let mut dependencies = self.dependencies;
        dependencies.extend(
            Dependencies::capture(ledger, registration.arena(), &handles)
                .ok_or_else(|| invalid("source facts disappeared"))?,
        )?;
        let prepared = Prepared {
            identity: Rc::clone(&ledger.identity),
            revision: ledger.revision,
            next: draft.state,
            before,
            peak,
            projected,
        };
        let virtuals = virtuals
            .into_iter()
            .map(|(entity, body)| VirtualEntity { entity, body })
            .collect();
        Ok(PreparedProgram {
            publication: PreparedPublication::from_program(
                registration,
                dependencies,
                prepared,
                member_edits,
            ),
            entities: virtuals,
        })
    }
}
impl<'a> PreparedProgram<'a> {
    /// Price an itemized release against this unpublished source prefix.
    pub fn release_cost(&self, owners: &[Owner], access: &Access<'_>) -> Result<u64, HeapFault> {
        self.publication.release_cost(owners, access)
    }
    pub fn peak(&self) -> u64 {
        self.publication.peak()
    }
    pub fn balance(&self) -> Result<Balance, HeapFault> {
        self.publication.balance()
    }
    pub fn require_source_room(mut self, extra: u64) -> Option<Self> {
        self.publication = self.publication.require_source_room(extra)?;
        Some(self)
    }
    pub fn admit(self, bound: u64) -> Option<AcceptedProgram<'a>> {
        Some(AcceptedProgram {
            publication: self.publication.admit(bound).ok()?,
            entities: self.entities,
        })
    }
}
impl AcceptedProgram<'_> {
    pub fn publish(
        self,
        ledger: &mut Ledger,
        semantic: impl FnOnce(&mut Arena, Vec<MemberEdit>),
    ) -> Result<Materializations, HeapFault> {
        self.publication.publish(ledger, semantic)?;
        Ok(Materializations {
            stamp: ledger.stamp(),
            entities: self.entities,
        })
    }
}
impl Ledger {
    pub fn next_invocation(&self) -> u64 {
        self.state.next_invocation
    }
}

impl PublicationValue {
    pub fn native_holding(
        &self,
        heap: &PagedHeap,
        arena: &Arena,
        owners: &BTreeMap<EntityId, lash_kernel_doc::NativeResourceOwner>,
    ) -> Result<lash_kernel_doc::NativeResourceHolding, HeapFault> {
        use lash_kernel_doc::NativeResourceOrigin as Origin;
        let (bytes, entity, alias) = match self {
            Self::Native(value) => (
                arena.logical_local(value)?,
                value
                    .handle()
                    .and_then(|handle| arena.facts(handle).ok()?.entity),
                matches!(
                    value,
                    Value::List(_)
                        | Value::Map(_)
                        | Value::Set(_)
                        | Value::Record(_)
                        | Value::Closure(_)
                ),
            ),
            Self::Page(value) => {
                let entity = match value {
                    PageValue::List(id)
                    | PageValue::Map(id)
                    | PageValue::Set(id)
                    | PageValue::Record(id)
                    | PageValue::Closure(id) => Some(*id),
                    _ => None,
                };
                (
                    Holding::from_page(heap.pages(), value)?.bytes(),
                    entity,
                    entity.is_some(),
                )
            }
            _ => return Err(invalid("native result provenance is unavailable")),
        };
        let origin = if alias {
            entity
                .and_then(|id| owners.get(&id).copied())
                .map_or(Origin::Alias, Origin::Fresh)
        } else {
            Origin::Immutable
        };
        Ok(lash_kernel_doc::NativeResourceHolding { bytes, origin })
    }
}
impl VirtualBody {
    pub fn native_members(
        &self,
        heap: &PagedHeap,
        arena: &Arena,
        owners: &BTreeMap<EntityId, lash_kernel_doc::NativeResourceOwner>,
    ) -> Result<
        (
            lash_kernel_doc::NativeResourceEntity,
            Vec<lash_kernel_doc::NativeResourceHolding>,
        ),
        HeapFault,
    > {
        use lash_kernel_doc::NativeResourceEntity as Kind;
        let mut members = Vec::new();
        let mut add = |value: PublicationValue| -> Result<(), HeapFault> {
            members.push(value.native_holding(heap, arena, owners)?);
            Ok(())
        };
        let kind = match self {
            Self::NativeList(values) => {
                for value in values {
                    add(PublicationValue::Native(*value))?;
                }
                Kind::List
            }
            Self::List(values) => {
                for value in values {
                    add(PublicationValue::Page(value.clone()))?;
                }
                Kind::List
            }
            Self::Set(values) => {
                for value in values {
                    add(PublicationValue::Page(value.clone()))?;
                    add(PublicationValue::Page(PageValue::Null))?;
                }
                Kind::Set
            }
            Self::Variable(value) => {
                add(PublicationValue::Page(value.clone()))?;
                Kind::Variable
            }
            Self::Closure(captures) => {
                for id in captures {
                    add(PublicationValue::Page(PageValue::Ref(Identity::Object(
                        *id,
                    ))))?;
                }
                Kind::Closure
            }
            Self::Map(entries) => {
                for (key, value) in entries {
                    add(PublicationValue::Page(key.clone()))?;
                    add(PublicationValue::Page(value.clone()))?;
                }
                Kind::Map
            }
            Self::Record(entries) => {
                let mut names = 0_u64;
                for (key, value) in entries {
                    let PageValue::Text(text) = key else {
                        return Err(invalid("record field is not text"));
                    };
                    names = names
                        .checked_add(text.summary.utf8_bytes)
                        .ok_or_else(|| invalid("record name overflow"))?;
                    add(PublicationValue::Page(value.clone()))?;
                }
                Kind::Record {
                    field_name_bytes: names,
                }
            }
        };
        Ok((kind, members))
    }
}
