//! The source interval, itemized ownership program and shared resource
//! acceptance travel together through one publication and one source frontier.
use super::*;
use crate::admission::{Allowance, ResourceFacts, SourceAccepted, SourceResources};
use lash_kernel_doc::{TaskId, heap::HeapFault};
use lash_kernel_state::{
    arena::{Arena, Registration},
    kernel_internals::{
        PagedHeap,
        logical::{
            self, EvaluatedProgram, Invocation, JournalEvent, Ledger, Materializations, MemberEdit,
            PreparedProgram, Program,
        },
    },
};
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Occurrence {
    pub(crate) task: TaskId,
    pub(crate) invocation: u64,
    pub(crate) loops: Vec<(Site, u64)>,
    pub(crate) action: Option<(Point, u64)>,
    pub(crate) departure: Option<(Point, u64)>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Position {
    pub(crate) at: Point,
    pub(crate) unpaid: Point,
    pub(crate) recovery: [u8; 32],
    pub(crate) occurrence: Occurrence,
}
/// Created from the canonical source owner and runtime frame, never a code
/// cache ordinal. Every exact-source handoff expires its dependent capabilities.
pub(crate) struct Frontier {
    source: Arc<Source>,
    link: lash_kernel_value::content_ids::LinkId,
    kernel: doc::KernelVersion,
    position: Position,
    revision: u64,
    identity: std::rc::Rc<()>,
}
struct Bound {
    source: Arc<CompiledInterval>,
    position: Position,
    next: Position,
    frontier: std::rc::Rc<()>,
    revision: u64,
    link: lash_kernel_value::content_ids::LinkId,
    kernel: doc::KernelVersion,
    allowance: Allowance,
    resources: ResourceFacts,
    room: CheckedRoom,
}
pub(crate) struct EvaluatedInterval {
    bound: Bound,
    program: EvaluatedProgram,
}
pub(crate) struct AcceptedInterval {
    evaluation: EvaluatedInterval,
    memory_bound: u64,
}
pub(crate) struct PreparedPrefix<'a> {
    bound: Bound,
    journal: PreparedProgram<'a>,
    memory_bound: u64,
}
pub(crate) struct CommittedPrefix {
    pub(crate) position: Position,
    pub(crate) exit: ExitKind,
    pub(crate) balance: logical::Balance,
    pub(crate) materializations: Materializations,
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
            || !self.contains(&position.at.site)
            || !self.contains(&position.unpaid.site)
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
            revision: 0,
            identity: std::rc::Rc::new(()),
        })
    }
    fn contains(&self, site: &Site) -> bool {
        site.unit == self.site.unit && site.path.starts_with(&self.site.path)
    }
}
impl Bound {
    fn current(&self, frontier: &Frontier) -> bool {
        std::rc::Rc::ptr_eq(&self.frontier, &frontier.identity)
            && self.revision == frontier.revision
            && self.position == frontier.position
            && self.link == frontier.link
            && self.kernel == frontier.kernel
            && self.source.owner() == frontier.source.owner
    }
}
impl Frontier {
    pub(crate) fn position(&self) -> &Position {
        &self.position
    }
    /// The runtime supplies canonical current coordinates after exact execution.
    /// No previously accepted suffix can survive this transition.
    pub(crate) fn handoff(
        &mut self,
        ledger: &mut Ledger,
        position: Position,
    ) -> Result<(), HeapFault> {
        if !self.source.contains(&position.at.site) || !self.source.contains(&position.unpaid.site)
        {
            return Err(fault("source handoff left its defining body"));
        }
        let revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| fault("source frontier overflow"))?;
        ledger.handoff()?;
        self.revision = revision;
        self.position = position;
        Ok(())
    }
    #[expect(clippy::too_many_arguments, reason = "one checked source occurrence")]
    pub(crate) fn evaluate_interval(
        &self,
        interval: Arc<CompiledInterval>,
        execution: &mut impl Execution,
        ledger: &Ledger,
        arena: &Arena,
        heap: &PagedHeap,
        program: Program,
        next: Position,
        allowance: Allowance,
        resources: ResourceFacts,
    ) -> Result<EvaluatedInterval, Refused> {
        if interval.owner() != self.source.owner
            || interval.start() != &self.position.at.site
            || interval
                .events()
                .next()
                .is_none_or(|event| event.point != self.position.at)
            || next.occurrence != self.position.occurrence
            || !self.source.contains(&next.at.site)
            || !self.source.contains(&next.unpaid.site)
            || interval
                .events()
                .last()
                .is_none_or(|event| event.point != next.at)
        {
            return Err(Refused::Source);
        }
        let events: Vec<_> = interval.events().collect();
        if events.len() != program.events().len() {
            return Err(Refused::Path);
        }
        let slots = self.source.owner_slots()?;
        let mut dependencies = Vec::new();
        let mut cursor = 0;
        let room = interval.evaluate_checked(execution, |event, site, facts| {
            dependencies.extend_from_slice(facts.dependencies().ok_or(Refused::Facts)?);
            let actual = &program.events()[cursor];
            cursor += 1;
            EventContext {
                source: &self.source,
                slots: &slots,
                occurrence: &self.position.occurrence,
                ledger,
                heap,
                arena,
            }
            .check(event, site, actual, facts)
        })?;
        let program = program
            .evaluate(ledger, arena)
            .and_then(|program| program.bind_dependencies(ledger, arena, &dependencies))
            .ok_or(Refused::Facts)?;
        Ok(EvaluatedInterval {
            bound: Bound {
                source: interval,
                position: self.position.clone(),
                next,
                frontier: std::rc::Rc::clone(&self.identity),
                revision: self.revision,
                link: self.link,
                kernel: self.kernel,
                allowance,
                resources,
                room,
            },
            program,
        })
    }
}
impl EvaluatedInterval {
    pub(crate) fn admit_interval<'a>(
        self,
        frontier: &Frontier,
        ledger: &Ledger,
        arena: &Arena,
        resources: SourceResources<'a>,
        allowance: &Allowance,
        memory_bound: u64,
    ) -> Result<Option<SourceAccepted<'a, AcceptedInterval>>, HeapFault> {
        if !self.bound.current(frontier)
            || !self.program.is_current(ledger, arena)
            || &self.bound.allowance != allowance
            || ledger
                .balance()?
                .total()?
                .checked_add(self.bound.room.balance().peak_extra)
                .is_none_or(|peak| peak > memory_bound)
        {
            return Ok(None);
        }
        let facts = self.bound.resources.clone();
        resources.admit_interval(
            &facts,
            allowance,
            AcceptedInterval {
                evaluation: self,
                memory_bound,
            },
        )
    }
}
impl<'r> SourceAccepted<'r, AcceptedInterval> {
    #[expect(
        clippy::too_many_arguments,
        reason = "one exact source publication frontier"
    )]
    pub(crate) fn prepare_publication<'a>(
        self,
        frontier: &Frontier,
        ledger: &Ledger,
        mut registration: Registration<'a>,
        heap: &PagedHeap,
        actual: Program,
        exit: ExitKind,
        position: &Position,
    ) -> Result<SourceAccepted<'r, PreparedPrefix<'a>>, PublicationError> {
        self.try_map(|accepted| {
            let evaluation = accepted.evaluation;
            if !evaluation.bound.current(frontier)
                || exit != evaluation.bound.source.exit()
                || position != &evaluation.bound.next
            {
                return Err(PublicationError::Miss("source exit or recovery changed"));
            }
            if !evaluation
                .program
                .matches(ledger, registration.arena(), &actual)
            {
                return Err(PublicationError::Miss(
                    "source program or dependencies changed",
                ));
            }
            let journal = evaluation
                .program
                .prepare(ledger, registration, heap, actual)
                .map_err(|error| match error {
                    logical::MemberFault::Heap(error) => error,
                    logical::MemberFault::Native(error) => fault(&error.to_string()),
                })?;
            let before = ledger.balance()?.total()?;
            let after = journal.balance()?.total()?;
            let room = evaluation.bound.room.balance();
            if i128::from(after) - i128::from(before) != room.retained_delta
                || journal.peak()
                    > before
                        .checked_add(room.peak_extra)
                        .ok_or_else(|| fault("source peak overflow"))?
            {
                return Err(PublicationError::Miss(
                    "source obligation was understated or ownership changed",
                ));
            }
            Ok(PreparedPrefix {
                bound: evaluation.bound,
                journal,
                memory_bound: accepted.memory_bound,
            })
        })
    }
}
impl SourceAccepted<'_, PreparedPrefix<'_>> {
    pub(crate) fn publish(
        self,
        frontier: &mut Frontier,
        ledger: &mut Ledger,
        semantic: impl FnOnce(&mut Arena, Vec<MemberEdit>),
    ) -> Result<CommittedPrefix, PublicationError> {
        self.commit(|prepared| {
            if !prepared.bound.current(frontier) {
                return Err(PublicationError::Miss("source frontier changed"));
            }
            let revision = frontier
                .revision
                .checked_add(1)
                .ok_or_else(|| fault("source frontier overflow"))?;
            let balance = prepared.journal.balance()?;
            let accepted = prepared
                .journal
                .admit(prepared.memory_bound)
                .ok_or_else(|| fault("source admission changed"))?;
            let materializations = accepted.publish(ledger, semantic)?;
            frontier.revision = revision;
            frontier.position = prepared.bound.next;
            Ok(CommittedPrefix {
                position: frontier.position.clone(),
                exit: prepared.bound.source.exit(),
                balance,
                materializations,
            })
        })
    }
}
fn fault(text: &str) -> HeapFault {
    HeapFault::Malformed(text.into())
}
struct EventContext<'a> {
    source: &'a Source,
    slots: &'a OwnerSlots,
    occurrence: &'a Occurrence,
    ledger: &'a Ledger,
    heap: &'a PagedHeap,
    arena: &'a Arena,
}
impl EventContext<'_> {
    fn check(
        &self,
        event: &Event,
        site: &super::interval::PublicationSite,
        actual: &JournalEvent,
        facts: &dyn Facts,
    ) -> Result<(), Refused> {
        let Self {
            source,
            slots,
            occurrence,
            ledger,
            heap,
            arena,
        } = *self;
        let root = |owner: &Owner| slots.root(owner, occurrence.task, occurrence.invocation);
        let mut reads = BTreeSet::new();
        let mut writes = BTreeSet::new();
        for owner in &event.inputs {
            if let Some(owner) = root(owner) {
                reads.insert(owner);
            }
        }
        if let Some(owner) = &event.output
            && let Some(owner) = root(owner)
        {
            writes.insert(owner);
        }
        for measure in event.peak.terms.keys().chain(event.delta.terms.keys()) {
            match measure {
                Measure::Release(owners) | Measure::NativeRelease { owners, .. } => {
                    for owner in owners {
                        if let Some(owner) = root(owner) {
                            reads.insert(owner);
                        } else if let Some(group) =
                            slots.group(owner, occurrence.task, occurrence.invocation)
                        {
                            reads.extend(group.iter().copied());
                        }
                    }
                }
                _ => {}
            }
        }
        // The source classification is compiled once with the verified route.
        if let Some(name) = &site.assignment {
            let owner = root(&Owner::Visible {
                at: event.point.site.clone(),
                name: name.clone(),
            })
            .ok_or(Refused::Source)?;
            reads.insert(owner);
            writes.insert(owner);
        }
        if event.point.phase == Phase::Leave {
            reads.insert(logical::Owner::Frame {
                task: occurrence.task,
                invocation: occurrence.invocation,
            });
            if event.point.site == source.site {
                writes.insert(logical::Owner::Incoming(occurrence.task));
            }
        }
        if event
            .inputs
            .iter()
            .filter_map(root)
            .any(|owner| !actual.inputs.contains(&owner))
        {
            return Err(Refused::Retained);
        }
        let native = event
            .peak
            .terms
            .keys()
            .find_map(|measure| match measure {
                Measure::CallPeak { at, callee } => facts.call(at, *callee),
                _ => None,
            })
            .and_then(|room| room.native_contract.as_ref());
        if let Some(contract) = native {
            check_native(actual, contract, heap, arena)?;
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
        let mut scratch = 0_u64;
        for lease in source
            .room
            .scratch
            .iter()
            .filter(|lease| lease.at == event.point)
        {
            scratch = scratch
                .checked_add(
                    u64::try_from(lease.amount.evaluate(facts).ok_or(Refused::Facts)?)
                        .map_err(|_| Refused::Scratch)?,
                )
                .ok_or(Refused::Scratch)?;
        }
        for lease in &actual.scratch {
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
            scratch = scratch.max(contract.peak_scratch_bytes().ok_or(Refused::Scratch)?);
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
            if ledger
                .holding(owner, heap.pages())
                .map_err(|_| Refused::Facts)?
                .is_some()
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
    actual: &JournalEvent,
    contract: &doc::NativeResourceContract,
    heap: &PagedHeap,
    arena: &Arena,
) -> Result<(), Refused> {
    use doc::NativeResourceEvent as Event;
    let declarations: Vec<_> = contract
        .events
        .iter()
        .filter_map(|event| {
            if let Event::Entity {
                owner,
                kind,
                members,
                ..
            } = event
            {
                Some((*owner, *kind, members))
            } else {
                None
            }
        })
        .collect();
    if declarations.len() != actual.entities.len()
        || actual.outputs.len() != 1
        || actual.member.is_some()
    {
        return Err(Refused::Facts);
    }
    let owners: BTreeMap<_, _> = actual
        .entities
        .iter()
        .zip(&declarations)
        .map(|(entity, (owner, _, _))| (entity.entity, *owner))
        .collect();
    if owners.len() != declarations.len() {
        return Err(Refused::Facts);
    }
    for (entity, (_, kind, members)) in actual.entities.iter().zip(&declarations) {
        let (actual_kind, actual_members) = entity
            .body
            .native_members(heap, arena, &owners)
            .map_err(|_| Refused::Facts)?;
        if &actual_kind != kind || &actual_members != *members {
            return Err(Refused::Facts);
        }
    }
    if actual.outputs[0]
        .1
        .native_holding(heap, arena, &owners)
        .map_err(|_| Refused::Facts)?
        != contract.output
    {
        return Err(Refused::Facts);
    }
    Ok(())
}
