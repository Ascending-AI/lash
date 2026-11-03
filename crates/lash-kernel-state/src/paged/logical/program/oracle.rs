//! Independent itemized publication oracle for laws and witness builds.
//! Ordinary execution has only the canonical boundary transaction interface.
use super::*;
use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_value::tokens::encode_body;
mod native;

impl VirtualBody {
    fn same(&self, other: &Self) -> bool {
        let values = |a: &[PageValue], b: &[PageValue]| {
            a.len() == b.len()
                && a.iter()
                    .zip(b)
                    .all(|(a, b)| encode_body(a) == encode_body(b))
        };
        match (self, other) {
            (Self::List(a), Self::List(b)) | (Self::Set(a), Self::Set(b)) => values(a, b),
            (Self::NativeList(a), Self::NativeList(b))
            | (Self::NativeSet(a), Self::NativeSet(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.same(b))
            }
            (Self::NativeMap(a), Self::NativeMap(b))
            | (Self::NativeRecord(a), Self::NativeRecord(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .zip(b)
                        .all(|((ak, av), (bk, bv))| ak.same(bk) && av.same(bv))
            }
            (Self::Variable(a), Self::Variable(b)) => encode_body(a) == encode_body(b),
            (Self::NativeVariable(a), Self::NativeVariable(b)) => a.same(b),
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
#[derive(Clone, Default)]
pub struct JournalEvent {
    /// The bound input-evaluated native event schedule, when preparing an exact native exit.
    pub native: Option<lash_kernel_doc::NativeResourceContract>,
    /// Existing owners observed without transfer by this event.
    pub borrowed: Vec<Owner>,
    pub inputs: Vec<Owner>,
    pub outputs: Vec<(Owner, PublicationValue)>,
    pub member: Option<MemberEdit>,
    pub entities: Vec<VirtualEntity>,
    pub frame: Option<FrameEvent>,
    pub scratch: Vec<ScratchEvent>,
}
impl JournalEvent {
    fn same(&self, other: &Self) -> bool {
        self.native == other.native
            && self.borrowed == other.borrowed
            && self.inputs == other.inputs
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
            && match (&self.member, &other.member) {
                (None, None) => true,
                (Some(a), Some(b)) => {
                    a.handle == b.handle
                        && a.member.same(&b.member)
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
            match &entity.body {
                VirtualBody::NativeList(values) | VirtualBody::NativeSet(values) => {
                    handles.extend(values.iter().filter_map(Value::handle))
                }
                VirtualBody::NativeMap(entries) | VirtualBody::NativeRecord(entries) => {
                    handles.extend(
                        entries
                            .iter()
                            .flat_map(|(key, value)| [key.handle(), value.handle()])
                            .flatten(),
                    );
                }
                VirtualBody::NativeVariable(value) => handles.extend(value.handle()),
                _ => {}
            }
        }
        for (_, value) in &self.outputs {
            value.handles(handles);
        }
        if let Some(edit) = &self.member {
            handles.push(edit.handle);
            handles.extend(edit.next.and_then(|v| v.handle()));
            if let Member::Table(key) = &edit.member {
                handles.extend(key.handle());
            }
        }
    }
}
#[derive(Clone)]
pub struct Program {
    events: Vec<JournalEvent>,
}
pub struct EvaluatedProgram {
    program: Program,
    dependencies: Dependencies,
    next_provisional: EntityId,
}
pub struct PreparedProgram<'a> {
    publication: BoundaryTransaction<'a>,
    entities: Vec<VirtualEntity>,
}
pub struct AcceptedProgram<'a> {
    publication: AcceptedBoundary<'a>,
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
            next_provisional: arena.next_provisional(),
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
        self.dependencies.is_current(ledger, arena)
            && self.next_provisional == arena.next_provisional()
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
            published: super::Publications::default(),
        };
        let before = ledger.balance()?.total()?;
        let mut peak = before;
        let mut projected = Vec::new();
        let mut virtuals = BTreeMap::<EntityId, VirtualBody>::new();
        let mut member_edits = Vec::new();
        let mut root_claims = publication::RootProof::default();
        let mut handles = Vec::new();
        let mut scratches = BTreeMap::<Owner, u64>::new();
        for event in actual.events {
            let construction = event.native_construction_peak(heap, registration.arena())?;
            peak = peak.max(
                draft
                    .balance()?
                    .total()?
                    .checked_add(construction)
                    .ok_or_else(|| invalid("native construction peak overflow"))?,
            );
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
            let mut borrowed = BTreeSet::new();
            for owner in &event.borrowed {
                if !borrowed.insert(*owner)
                    || inputs.contains(owner)
                    || outputs.iter().any(|(output, _)| output == owner)
                    || draft.holding(*owner, access)?.is_none()
                {
                    return Err(invalid("borrowed source input is absent or edited").into());
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
            root_claims.append(&inputs, &roots);
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
                    edit.member.clone(),
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
            publication: BoundaryTransaction::from_program(
                registration,
                dependencies,
                prepared,
                member_edits,
                root_claims,
            ),
            entities: virtuals,
        })
    }
}
impl<'a> PreparedProgram<'a> {
    /// Authenticate the prepared journal under its still-exclusive registration.
    pub fn is_current(&mut self, ledger: &Ledger) -> bool {
        self.publication.is_current(ledger)
    }
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
    /// Collect garbage the ledger held before this prefix (see `Prepared::collect`).
    pub fn collect(mut self, before: &Ledger, access: &Access<'_>) -> Result<Self, HeapFault> {
        self.publication = self.publication.collect(before, access)?;
        Ok(self)
    }
    pub fn admit(self, bound: u64) -> Option<AcceptedProgram<'a>> {
        Some(AcceptedProgram {
            publication: self.publication.admit(bound).ok()?,
            entities: self.entities,
        })
    }
}
impl AcceptedProgram<'_> {
    /// Install every prepared entity in the same infallible journal callback.
    /// Unlike deferred materialization, this needs no later handoff: no new
    /// capability can observe the publication before its storage is installed.
    pub fn publish_materialized(
        self,
        ledger: &mut Ledger,
        semantic: impl FnOnce(&mut Arena, Vec<MemberEdit>, Vec<VirtualEntity>),
    ) -> Result<Materializations, HeapFault> {
        self.publication.publish(ledger, |arena, edits| {
            semantic(arena, edits, self.entities);
        })?;
        Ok(Materializations {
            stamp: ledger.stamp(),
            entities: Vec::new(),
        })
    }

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
