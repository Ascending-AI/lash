//! Compiled source events and shared resource acceptance authenticate one
//! atomic ledger publication and the exact source frontier.
use super::*;
use crate::admission::{Allowance, ResourceFacts, SourceAccepted, SourceResources};
use lash_kernel_doc::{TaskId, heap::HeapFault};
use lash_kernel_state::{
    arena::{Arena, Registration},
    kernel_internals::{
        PagedHeap,
        logical::{self, Invocation, Ledger, MemberEdit},
    },
};
use std::collections::BTreeSet;
pub(crate) mod boundary;
mod control;
#[cfg(test)]
pub(crate) mod oracle;
pub(crate) use control::{ControlExit, ControlRoots, FrameCaller, FrameEntry};
mod departure;
pub(crate) use departure::DepartureExit;
mod exact;
pub(crate) use exact::{ExactEvent, ExactRoots, ExternalFacts, PreparedEvent, Successor};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Occurrence {
    pub(crate) task: TaskId,
    pub(crate) invocation: u64,
    pub(crate) loops: Vec<(Site, u64)>,
    pub(crate) action: Option<(Point, u64)>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Position {
    pub(crate) at: Point,
    pub(crate) unpaid: Option<Point>,
    pub(crate) recovery: [u8; 32],
    pub(crate) occurrence: Occurrence,
}
/// Created from the canonical source owner and runtime frame, never a code
/// cache ordinal. Every exact-source handoff expires its dependent capabilities.
pub(crate) struct Frontier {
    pub(in crate::compile::room) source: Arc<Source>,
    link: lash_kernel_value::content_ids::LinkId,
    kernel: doc::KernelVersion,
    pub(in crate::compile::room) position: Position,
    /// A proven acquisition committed past `position` without recording its
    /// coordinate. The next checked interval must hand off to its own start.
    pub(in crate::compile::room) proven: bool,
    revision: u64,
    pub(in crate::compile::room) identity: std::rc::Rc<()>,
}
/// An eligibility miss resumes exact source with its unpaid head intact.
/// Storage/host failures retain their typed infrastructure identity.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PublicationError {
    #[error("source publication declined: {0}")]
    Miss(&'static str),
    #[error(transparent)]
    Heap(#[from] HeapFault),
}

impl Source {
    pub(crate) fn frontier(
        self: &Arc<Self>,
        ledger: &Ledger,
        heap: &PagedHeap,
        invocation: &Invocation,
        link: lash_kernel_value::content_ids::LinkId,
        kernel: doc::KernelVersion,
        position: Position,
    ) -> Result<Frontier, Refused> {
        self.verify()?;
        if !ledger.is_current(&heap.logical_ledger().borrow().stamp()) {
            return Err(Refused::Source);
        }
        if invocation.id() != position.occurrence.invocation
            || invocation.frame()
                != (logical::Owner::Frame {
                    task: position.occurrence.task,
                    invocation: position.occurrence.invocation,
                })
            || !(ledger
                .invocation(position.occurrence.task, invocation.id(), heap.pages())
                .as_ref()
                .ok()
                == Some(invocation)
                || ledger.prospective_invocation(position.occurrence.task) == *invocation)
            || !self.valid_position(&position)
            || position
                .occurrence
                .loops
                .iter()
                .any(|(site, _)| !self.contains(site))
        {
            return Err(Refused::Source);
        }
        Ok(Frontier {
            source: Arc::clone(self),
            link,
            kernel,
            position,
            proven: false,
            revision: 0,
            identity: std::rc::Rc::new(()),
        })
    }
    fn contains(&self, site: &Site) -> bool {
        site.unit == self.site.unit && site.path.starts_with(&self.site.path)
    }
    fn is_source_leave(&self, point: &Point) -> bool {
        point.site == self.site && point.phase == Phase::Leave
    }
    fn valid_position(&self, position: &Position) -> bool {
        self.contains(&position.at.site)
            && match &position.unpaid {
                Some(head) => self.contains(&head.site),
                None => self.is_source_leave(&position.at),
            }
    }
}
impl Frontier {
    /// Read-only binding check before C acquires the exclusive state lease.
    pub(crate) fn region_binding_matches(
        &self,
        source: &Arc<Source>,
        link: lash_kernel_value::content_ids::LinkId,
        kernel: doc::KernelVersion,
        invocation: &Invocation,
    ) -> bool {
        Arc::ptr_eq(&self.source, source)
            && self.link == link
            && self.kernel == kernel
            && !self.proven
            && invocation.id() == self.position.occurrence.invocation
            && invocation.frame()
                == (logical::Owner::Frame {
                    task: self.position.occurrence.task,
                    invocation: self.position.occurrence.invocation,
                })
            && self.position.unpaid.is_some()
    }

    /// Prepare an authenticated successor without publishing a ledger handoff.
    /// C installs it only inside the state lease's atomic commit.
    pub(crate) fn region_successor(&self, position: Position) -> Result<Self, HeapFault> {
        if self.position.unpaid.is_none()
            || !self.source.valid_position(&position)
            || position.occurrence.task != self.position.occurrence.task
            || position.occurrence.invocation != self.position.occurrence.invocation
        {
            return Err(fault("region successor left its entered source invocation"));
        }
        let revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| fault("source frontier overflow"))?;
        Ok(Self {
            source: Arc::clone(&self.source),
            link: self.link,
            kernel: self.kernel,
            position,
            proven: false,
            revision,
            identity: std::rc::Rc::new(()),
        })
    }
    /// The sealed executor supplies the actual invocation from its state
    /// reservation. This view grants no ledger or publication authority.
    pub(crate) fn region_callee(
        &self,
        source: &Arc<Source>,
        invocation: &Invocation,
        link: lash_kernel_value::content_ids::LinkId,
        kernel: doc::KernelVersion,
        position: Position,
    ) -> Result<Self, HeapFault> {
        source
            .verify()
            .map_err(|_| fault("region callee source is invalid"))?;
        if invocation.frame()
            != (logical::Owner::Frame {
                task: self.position.occurrence.task,
                invocation: position.occurrence.invocation,
            })
            || invocation.id() != position.occurrence.invocation
            || position.occurrence.task != self.position.occurrence.task
            || !source.valid_position(&position)
        {
            return Err(fault("region callee left its reserved invocation"));
        }
        Ok(Self {
            source: Arc::clone(source),
            link,
            kernel,
            position,
            proven: false,
            revision: 0,
            identity: std::rc::Rc::new(()),
        })
    }
    pub(crate) fn position(&self) -> &Position {
        &self.position
    }
    /// Whether an interval starting at `start` needs a handoff first.
    pub(crate) fn departs(&self, start: &Position) -> bool {
        self.proven || &self.position != start
    }
    /// Commit a proven acquisition in the defining body. It advances this
    /// frontier past its recorded coordinate and expires every capability
    /// bound to the older frontier; the ledger edit itself is the caller's.
    #[cfg(test)]
    pub(crate) fn proven(&mut self) -> Result<(), HeapFault> {
        if self.position.unpaid.is_none() {
            return Err(fault("proven source event after the defining Leave"));
        }
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| fault("source frontier overflow"))?;
        self.proven = true;
        Ok(())
    }
    /// The runtime supplies canonical current coordinates after exact execution.
    /// No previously accepted suffix can survive this transition.
    pub(crate) fn handoff(
        &mut self,
        ledger: &mut Ledger,
        position: Position,
    ) -> Result<(), HeapFault> {
        if self.position.unpaid.is_none() || !self.source.valid_position(&position) {
            return Err(fault("source handoff left its defining body"));
        }
        let revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| fault("source frontier overflow"))?;
        ledger.handoff()?;
        self.revision = revision;
        self.position = position;
        self.proven = false;
        Ok(())
    }
}
fn fault(text: &str) -> HeapFault {
    HeapFault::Malformed(text.into())
}
/// Invocation-free roots derived from a verified event. Dynamic publications
/// still supply their actual task, invocation, values and ledger presence.
pub(super) struct EventRoles {
    slots: Arc<OwnerSlots>,
    inputs: Box<[Option<logical::Owner>]>,
    output: Option<logical::Owner>,
    atoms: Box<[logical::Owner]>,
    reads: Box<[logical::Owner]>,
    groups: Box<[Box<[logical::Owner]>]>,
    sessions: Box<[logical::Owner]>,
    assignment: Option<logical::Owner>,
    cell_assignment: bool,
    field: Option<logical::Owner>,
    calls: Box<[(Site, CallSource)]>,
    scratch: Box<[Amount]>,
}
fn stamp(owner: logical::Owner, occurrence: &Occurrence) -> logical::Owner {
    match owner {
        logical::Owner::Value { slot, .. } => logical::Owner::Value {
            task: occurrence.task,
            invocation: occurrence.invocation,
            slot,
        },
        logical::Owner::Frame { .. } => logical::Owner::Frame {
            task: occurrence.task,
            invocation: occurrence.invocation,
        },
        logical::Owner::Incoming(_) => logical::Owner::Incoming(occurrence.task),
        other => other,
    }
}
impl EventRoles {
    pub(super) fn compile(
        source: &Source,
        event: &Event,
        assignment: Option<&Name>,
    ) -> Result<Self, Refused> {
        let slots = source.owner_slots()?;
        let root = |owner: &Owner| slots.root(owner, TaskId::MAIN, 0);
        let inputs: Box<[_]> = event.inputs.iter().map(root).collect();
        let mut reads: BTreeSet<_> = inputs.iter().flatten().copied().collect();
        let mut groups = Vec::new();
        let mut sessions = BTreeSet::new();
        for measure in event.peak.measures().chain(event.delta.measures()) {
            if let Measure::Release(owners) = measure {
                for owner in owners {
                    if let Some(owner) = root(owner) {
                        reads.insert(owner);
                    } else if let Some(group) = slots.group(owner, TaskId::MAIN, 0) {
                        if matches!(owner, Owner::Sessions(_)) {
                            sessions.extend(group);
                        } else if !groups.contains(&group) {
                            groups.push(group);
                        }
                    }
                }
            }
        }
        Ok(Self {
            slots: Arc::clone(&slots),
            inputs,
            output: event.output.as_ref().and_then(root),
            atoms: event
                .peak
                .measures()
                .filter_map(|measure| match measure {
                    Measure::Local(owner @ Owner::Atom { at, .. })
                        if event.point.phase == Phase::Enter && at == &event.point.site =>
                    {
                        root(owner)
                    }
                    _ => None,
                })
                .collect(),
            reads: reads.into_iter().collect(),
            groups: groups.into(),
            sessions: sessions.into_iter().collect(),
            assignment: assignment.and_then(|name| {
                root(&Owner::Visible {
                    at: event.point.site.clone(),
                    name: name.clone(),
                })
            }),
            cell_assignment: source.assigns_cell(&event.point.site),
            field: root(&Owner::Operand(event.point.site.child(0))),
            calls: event
                .peak
                .measures()
                .filter_map(|measure| match measure {
                    Measure::CallPeak { at, callee } => Some((at.clone(), *callee)),
                    _ => None,
                })
                .collect(),
            scratch: source
                .room
                .scratch
                .iter()
                .filter(|lease| lease.at == event.point)
                .map(|lease| lease.amount.clone())
                .collect(),
        })
    }
}
struct EventView<'a> {
    inputs: &'a [logical::Owner],
    outputs: &'a [(logical::Owner, logical::PublicationValue)],
    borrowed: &'a [logical::Owner],
    member: Option<&'a MemberEdit>,
    entities: &'a [logical::VirtualEntity],
    frame: Option<logical::FrameEvent>,
    scratch: &'a [logical::ScratchEvent],
    native: Option<&'a doc::NativeResourceContract>,
}
impl EventView<'_> {
    fn scratch_peak(&self) -> Option<u64> {
        let mut live = std::collections::BTreeMap::new();
        let mut peak = 0;
        for event in self.scratch {
            match *event {
                logical::ScratchEvent::Begin { owner, bytes } => {
                    if live.insert(owner, bytes).is_some() {
                        return None;
                    }
                }
                logical::ScratchEvent::End { owner } => {
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
}
struct EventContext<'a> {
    earlier: &'a [EventView<'a>],
    source: &'a Source,
    slots: &'a OwnerSlots,
    occurrence: &'a Occurrence,
    ledger: &'a Ledger,
    heap: &'a PagedHeap,
    arena: &'a Arena,
}
impl EventContext<'_> {
    fn group_candidates(&self) -> Result<Vec<logical::Owner>, Refused> {
        let mut owners = self
            .ledger
            .frame_owners(
                self.occurrence.task,
                self.occurrence.invocation,
                self.heap.pages(),
            )
            .map_err(|_| Refused::Facts)?;
        owners.extend(
            self.earlier
                .iter()
                .flat_map(|event| event.outputs.iter().map(|(owner, _)| *owner)),
        );
        Ok(owners)
    }
    /// Explicit releases use the canonical current roots. The independent
    /// multi-event oracle also supplies its preceding source edits.
    fn present(&self, owner: logical::Owner) -> Result<bool, Refused> {
        for event in self.earlier.iter().rev() {
            if event.outputs.iter().any(|(output, _)| *output == owner) {
                return Ok(true);
            }
            if event.inputs.contains(&owner) {
                return Ok(false);
            }
            match event.frame {
                Some(logical::FrameEvent::Enter { task, invocation })
                    if owner == (logical::Owner::Frame { task, invocation }) =>
                {
                    return Ok(true);
                }
                Some(logical::FrameEvent::Leave { task, invocation })
                    if matches!(owner, logical::Owner::Frame { task: t, invocation: i }
                        | logical::Owner::Value { task: t, invocation: i, .. }
                        if t == task && i == invocation) =>
                {
                    return Ok(false);
                }
                _ => {}
            }
        }
        self.ledger
            .holding(owner, self.heap.pages())
            .map(|holding| holding.is_some())
            .map_err(|_| Refused::Facts)
    }

    fn check_reference(
        &self,
        owner: logical::Owner,
        handle: lash_kernel_doc::arena::ArenaHandle,
    ) -> Result<(), Refused> {
        let entity = self
            .arena
            .facts(handle)
            .map_err(|_| Refused::Facts)?
            .ledger_entity()
            .ok_or(Refused::Facts)?;
        let holding = self
            .ledger
            .holding(owner, self.heap.pages())
            .map_err(|_| Refused::Facts)?
            .ok_or(Refused::Retained)?;
        let mut edge = None;
        let mut unique = true;
        holding
            .visit_edges(self.heap.pages(), &mut |identity, count| {
                unique &= edge.replace((identity, count)).is_none();
                Ok(())
            })
            .map_err(|_| Refused::Facts)?;
        let bytes = if matches!(
            owner,
            logical::Owner::Value {
                slot: logical::Slot::Capture(_),
                ..
            }
        ) {
            0
        } else {
            Cost::REFERENCE
        };
        if !unique
            || edge != Some((doc::heap::Identity::Object(entity), 1))
            || holding.bytes() != bytes
        {
            return Err(Refused::Retained);
        }
        Ok(())
    }
    fn check_cell(
        &self,
        owner: logical::Owner,
        handle: lash_kernel_doc::arena::ArenaHandle,
    ) -> Result<(), Refused> {
        self.check_reference(owner, handle)?;
        if !matches!(self.arena.body(handle), Ok(doc::arena::Body::Cell(_))) {
            return Err(Refused::Retained);
        }
        Ok(())
    }
    fn check_resumption(
        &self,
        event: &Event,
        roles: &[Site],
        actual: &EventView<'_>,
    ) -> Result<(), Refused> {
        if actual.native.is_some()
            || !actual.borrowed.is_empty()
            || actual.member.is_some()
            || !actual.entities.is_empty()
            || actual.frame.is_some()
            || !actual.scratch.is_empty()
            || actual.inputs.len() != roles.len() + 1
            || actual.outputs.len() != roles.len() + 1
        {
            return Err(Refused::Retained);
        }
        let task = self.occurrence.task;
        let invocation = self.occurrence.invocation;
        let output = self
            .slots
            .root(&Owner::Operand(event.point.site.clone()), task, invocation)
            .ok_or(Refused::Source)?;
        let mut transfers = vec![(logical::Owner::Incoming(task), output)];
        for role in roles {
            transfers.push((
                self.slots
                    .continuation(role, task, invocation)
                    .ok_or(Refused::Source)?,
                self.slots
                    .root(&Owner::Operand(role.clone()), task, invocation)
                    .ok_or(Refused::Source)?,
            ));
        }
        for (input, output) in transfers {
            if !actual.inputs.contains(&input) || !actual.outputs.iter().any(|(owner, value)| {
                *owner == output
                    && matches!(value, logical::PublicationValue::Existing(from) if *from == input)
            })
                || self
                    .ledger
                    .holding(input, self.heap.pages())
                    .map_err(|_| Refused::Facts)?
                    .is_none()
                || self
                    .ledger
                    .holding(output, self.heap.pages())
                    .map_err(|_| Refused::Facts)?
                    .is_some()
            {
                return Err(Refused::Retained);
            }
        }
        Ok(())
    }
    fn check(
        &self,
        event: &Event,
        site: &super::interval::PublicationSite,
        actual: &EventView<'_>,
        facts: &dyn Facts,
    ) -> Result<(), Refused> {
        let Self {
            source,
            slots,
            occurrence,
            ledger: _,
            heap,
            arena,
            earlier: _,
        } = *self;
        let mut reads = BTreeSet::new();
        let mut writes = BTreeSet::new();
        if let Some(roles) = &site.restored_roles {
            if site.uncounted_resumption && occurrence.action.is_some() {
                return Err(Refused::Source);
            }
            return self.check_resumption(event, roles, actual);
        }
        let roles = site
            .roles
            .get_or_init(|| EventRoles::compile(source, event, site.assignment.as_ref()))
            .as_ref()
            .map_err(|error| *error)?;
        if !std::ptr::eq(slots, roles.slots.as_ref()) && slots != roles.slots.as_ref() {
            return Err(Refused::Source);
        }
        if site.borrowed_inputs {
            let expected: BTreeSet<_> = roles
                .inputs
                .iter()
                .map(|owner| {
                    owner
                        .map(|owner| stamp(owner, occurrence))
                        .ok_or(Refused::Source)
                })
                .collect::<Result<_, _>>()?;
            let borrowed: BTreeSet<_> = actual.borrowed.iter().copied().collect();
            return if borrowed == expected
                && borrowed.len() == actual.borrowed.len()
                && actual.inputs.is_empty()
                && actual.outputs.is_empty()
                && actual.member.is_none()
                && actual.entities.is_empty()
                && actual.frame.is_none()
                && actual.scratch.is_empty()
                && actual.native.is_none()
            {
                Ok(())
            } else {
                Err(Refused::Retained)
            };
        }
        if !actual.borrowed.is_empty() {
            return Err(Refused::Retained);
        }
        reads.extend(roles.reads.iter().map(|owner| stamp(*owner, occurrence)));
        reads.extend(roles.sessions.iter().copied());
        if let Some(owner) = roles.output {
            writes.insert(stamp(owner, occurrence));
        }
        writes.extend(roles.atoms.iter().map(|owner| stamp(*owner, occurrence)));
        if !roles.groups.is_empty() {
            let candidates = self.group_candidates()?;
            for group in &roles.groups {
                reads.extend(candidates.iter().copied().filter(|owner| {
                    match *owner {
                        logical::Owner::Value {
                            task,
                            invocation,
                            slot,
                        } if task == occurrence.task && invocation == occurrence.invocation => {
                            group
                                .binary_search(&logical::Owner::Value {
                                    task: TaskId::MAIN,
                                    invocation: 0,
                                    slot,
                                })
                                .is_ok()
                        }
                        _ => false,
                    }
                }));
            }
        }
        // The source classification is compiled once with the verified route.
        if site.assignment.is_some() {
            let owner = roles
                .assignment
                .map(|owner| stamp(owner, occurrence))
                .ok_or(Refused::Source)?;
            if roles.cell_assignment {
                let edit = actual
                    .member
                    .as_ref()
                    .filter(|edit| {
                        matches!(edit.member, logical::Member::Variable) && edit.next.is_some()
                    })
                    .ok_or(Refused::Retained)?;
                if actual.inputs.contains(&owner)
                    || actual.outputs.iter().any(|(output, _)| *output == owner)
                {
                    return Err(Refused::Retained);
                }
                self.check_cell(owner, edit.handle)?;
            } else {
                reads.insert(owner);
                writes.insert(owner);
            }
        }
        if event.point.phase == Phase::Leave && event.point.site == source.site {
            reads.insert(logical::Owner::Frame {
                task: occurrence.task,
                invocation: occurrence.invocation,
            });
            writes.insert(logical::Owner::Incoming(occurrence.task));
        }
        if roles
            .inputs
            .iter()
            .filter_map(|owner| owner.map(|owner| stamp(owner, occurrence)))
            .any(|owner| !actual.inputs.contains(&owner))
        {
            return Err(Refused::Retained);
        }
        let native = roles
            .calls
            .iter()
            .find_map(|(at, callee)| facts.call(at, *callee))
            .and_then(|room| room.native_contract.as_ref());
        if actual.native.is_some() && native.is_none() {
            return Err(Refused::Facts);
        }
        if let Some(contract) = native {
            check_native(actual, &contract.contract, heap, arena)?;
        }
        if actual.inputs.iter().any(|owner| !reads.contains(owner))
            || actual
                .outputs
                .iter()
                .any(|(owner, _)| !writes.contains(owner))
            || (actual.member.is_some() && !site.member)
            || (native.is_none() && actual.entities.len() > event.entities.len())
        {
            return Err(Refused::Retained);
        }
        if let Some(edit) = &actual.member {
            match (&edit.member, &site.field) {
                (logical::Member::Field(actual), Some(expected)) if actual == expected => {
                    let target = roles
                        .field
                        .map(|owner| stamp(owner, occurrence))
                        .ok_or(Refused::Retained)?;
                    self.check_reference(target, edit.handle)?;
                    if !matches!(arena.body(edit.handle), Ok(doc::arena::Body::Record(_))) {
                        return Err(Refused::Retained);
                    }
                }
                (logical::Member::Field(_), _) | (_, Some(_)) => return Err(Refused::Retained),
                (logical::Member::List(_) | logical::Member::Table(_), None) => {
                    let target = roles
                        .inputs
                        .first()
                        .copied()
                        .flatten()
                        .map(|owner| stamp(owner, occurrence))
                        .ok_or(Refused::Retained)?;
                    self.check_reference(target, edit.handle)?;
                }
                _ => {}
            }
        }
        let mut scratch = 0_u64;
        for amount in &roles.scratch {
            scratch = scratch
                .checked_add(
                    u64::try_from(amount.evaluate(facts).ok_or(Refused::Facts)?)
                        .map_err(|_| Refused::Scratch)?,
                )
                .ok_or(Refused::Scratch)?;
        }
        for lease in actual.scratch {
            let owner = match lease {
                logical::ScratchEvent::Begin { owner, .. }
                | logical::ScratchEvent::End { owner } => owner,
            };
            if !matches!(owner,logical::Owner::Value {task,invocation,slot:logical::Slot::Scratch(_)} if *task==occurrence.task&&*invocation==occurrence.invocation)
            {
                return Err(Refused::Scratch);
            }
        }
        if let Some(contract) = native {
            scratch = scratch.max(contract.scratch_peak);
        }
        let supplied = actual.scratch_peak().ok_or(Refused::Scratch)?;
        if supplied != scratch {
            return Err(Refused::Scratch);
        }
        if let Some(frame) = actual.frame {
            let expected = match event.point.phase {
                Phase::Enter if event.point.site == source.site => logical::FrameEvent::Enter {
                    task: occurrence.task,
                    invocation: occurrence.invocation,
                },
                Phase::Leave if event.point.site == source.site => logical::FrameEvent::Leave {
                    task: occurrence.task,
                    invocation: occurrence.invocation,
                },
                _ => return Err(Refused::Source),
            };
            if frame != expected {
                return Err(Refused::Source);
            }
        }
        // Missing explicit releases and wrong same-price owner substitutions are
        // rejected independently of the source envelope's retained numeric delta.
        for owner in reads {
            if self.present(owner)?
                && !actual.inputs.contains(&owner)
                && !matches!(actual.frame, Some(logical::FrameEvent::Leave { .. }))
            {
                return Err(Refused::Retained);
            }
        }
        if event.output.is_some()
            && !writes
                .iter()
                .all(|owner| actual.outputs.iter().any(|(out, _)| out == owner))
        {
            return Err(Refused::Retained);
        }
        Ok(())
    }
}

fn check_native(
    actual: &EventView<'_>,
    contract: &doc::NativeResourceContract,
    heap: &PagedHeap,
    arena: &Arena,
) -> Result<(), Refused> {
    let prepared = match actual.outputs {
        [(_, logical::PublicationValue::Prepared(value))] => {
            if !value.graph().is_current(arena)
                || !std::ptr::eq(actual.entities, value.graph().entities())
            {
                return Err(Refused::Facts);
            }
            Some(value)
        }
        _ => None,
    };
    if actual.native.is_some_and(|bound| bound != contract)
        || actual.outputs.len() != 1
        || actual.member.is_some()
    {
        return Err(Refused::Facts);
    }
    let declarations: Vec<_> = contract
        .events
        .iter()
        .filter_map(|event| match event {
            doc::NativeResourceEvent::Entity {
                owner,
                kind,
                members,
                ..
            } => Some((*owner, *kind, members)),
            _ => None,
        })
        .collect();
    if declarations.len() != actual.entities.len() {
        return Err(Refused::Facts);
    }
    let owners: std::collections::BTreeMap<_, _> = actual
        .entities
        .iter()
        .zip(&declarations)
        .map(|(entity, (owner, _, _))| (entity.entity, *owner))
        .collect();
    if owners.len() != actual.entities.len() {
        return Err(Refused::Facts);
    }
    for (index, (entity, (_, kind, members))) in
        actual.entities.iter().zip(declarations).enumerate()
    {
        let (actual_kind, actual_members) = if let Some(value) = prepared {
            value
                .graph()
                .native_members(index, &owners)
                .map_err(|_| Refused::Facts)?
        } else {
            entity
                .body
                .native_members(heap, arena, &owners)
                .map_err(|_| Refused::Facts)?
        };
        if actual_kind != kind || actual_members != *members {
            return Err(Refused::Facts);
        }
    }
    let output = if let Some(value) = prepared {
        value.native_holding(&owners)
    } else {
        actual.outputs[0]
            .1
            .native_holding(heap, arena, &owners)
            .map_err(|_| Refused::Facts)?
    };
    if output != contract.output {
        return Err(Refused::Facts);
    }
    // Scratch ordering is part of the declaration, including leases ending
    // before an allocation. Equal peaks do not authorize another schedule.
    // The source event checks the canonical destination, which may be a
    // retained loop-control slot. Scratch is scoped to its invocation.
    let logical::Owner::Value {
        task, invocation, ..
    } = actual.outputs[0].0
    else {
        return Err(Refused::Facts);
    };
    let owner = |index: doc::NativeResourceOwner| logical::Owner::Value {
        task,
        invocation,
        slot: logical::Slot::Scratch(logical::SourceRole::native_resource(index.0)),
    };
    let mut active = Vec::new();
    let mut scratch = Vec::new();
    for event in &contract.events {
        active.retain(|(index, release)| {
            if *release == doc::NativeResourceRelease::Before(event.owner()) {
                scratch.push(logical::ScratchEvent::End {
                    owner: owner(*index),
                });
                false
            } else {
                true
            }
        });
        if let doc::NativeResourceEvent::Scratch {
            owner: index,
            bytes,
            release,
        } = event
        {
            scratch.push(logical::ScratchEvent::Begin {
                owner: owner(*index),
                bytes: *bytes,
            });
            active.push((*index, *release));
        }
    }
    scratch.extend(
        active
            .into_iter()
            .map(|(index, _)| logical::ScratchEvent::End {
                owner: owner(index),
            }),
    );
    if scratch != actual.scratch {
        return Err(Refused::Scratch);
    }
    Ok(())
}
