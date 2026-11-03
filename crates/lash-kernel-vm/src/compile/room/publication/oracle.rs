//! Itemized source-publication oracle retained for the independent laws.
//! Production enters through compiled source events and exit transactions.
use super::*;
use lash_kernel_state::kernel_internals::logical::{
    EvaluatedProgram, JournalEvent, Materializations, PreparedProgram, Program,
};

impl<'a> From<&'a JournalEvent> for EventView<'a> {
    fn from(event: &'a JournalEvent) -> Self {
        Self {
            inputs: &event.inputs,
            outputs: &event.outputs,
            borrowed: &event.borrowed,
            member: event.member.as_ref(),
            entities: &event.entities,
            frame: event.frame,
            scratch: &event.scratch,
            native: event.native.as_ref(),
        }
    }
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
/// A validated exact journal. The sufficient source envelope is a proof of
/// its obligations, never the exact memory request used at its verdict point.
pub(crate) struct PreparedExactPrefix<'a> {
    bound: Bound,
    journal: PreparedProgram<'a>,
}
impl<'a> PreparedExactPrefix<'a> {
    /// The exact journal's peak: the run's logical holdings at its verdict point.
    pub(crate) fn peak(&self) -> u64 {
        self.journal.peak()
    }
    /// The peak without what the ledger already held unreachable.
    pub(crate) fn collect(mut self, ledger: &Ledger, heap: &PagedHeap) -> Result<Self, HeapFault> {
        self.journal = self.journal.collect(ledger, heap.pages())?;
        Ok(self)
    }
    pub(crate) fn admit_exact<'r>(
        mut self,
        frontier: &Frontier,
        ledger: &Ledger,
        resources: SourceResources<'r>,
        allowance: &Allowance,
        memory_bound: u64,
    ) -> Result<Option<SourceAccepted<'r, PreparedPrefix<'a>>>, HeapFault> {
        if !self.bound.current(frontier)
            || !self.journal.is_current(ledger)
            || &self.bound.allowance != allowance
            || self.journal.peak() > memory_bound
        {
            return Ok(None);
        }
        let facts = self.bound.resources.clone();
        resources.admit_interval(
            &facts,
            allowance,
            PreparedPrefix {
                bound: self.bound,
                journal: self.journal,
                memory_bound,
            },
        )
    }
}

pub(crate) struct CommittedPrefix {
    pub(crate) position: Position,
    pub(crate) exit: ExitKind,
    pub(crate) balance: logical::Balance,
    pub(crate) materializations: Materializations,
}

impl Bound {
    fn current(&self, frontier: &Frontier) -> bool {
        std::rc::Rc::ptr_eq(&self.frontier, &frontier.identity)
            && self.revision == frontier.revision
            && !frontier.proven
            && self.position == frontier.position
            && self.link == frontier.link
            && self.kernel == frontier.kernel
            && self.source.owner() == frontier.source.owner
    }
}

impl Frontier {
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
        if self.proven
            || self.position.unpaid.is_none()
            || !self.source.valid_position(&self.position)
            || interval.owner() != self.source.owner
            || interval.defining_source() != &self.source.site
            || interval.start() != &self.position.at.site
            || interval
                .events()
                .next()
                .is_none_or(|event| event.point != self.position.at)
            || next.occurrence != self.position.occurrence
            || !self.source.valid_position(&next)
            || next.unpaid.is_none() != self.source.is_source_leave(&next.at)
            || interval
                .events()
                .last()
                .is_none_or(|event| event.point != next.at)
        {
            return Err(Refused::Source);
        }
        let events: Vec<_> = interval.events().collect();
        let views: Vec<_> = program.events().iter().map(EventView::from).collect();
        if events.len() != program.events().len() {
            return Err(Refused::Path);
        }
        let slots = self.source.owner_slots()?;
        let mut dependencies = Vec::new();
        let mut cursor = 0;
        let room = interval.evaluate_checked(execution, |event, site, facts| {
            dependencies.extend_from_slice(facts.dependencies().ok_or(Refused::Facts)?);
            let earlier = &views[..cursor];
            let actual = &program.events()[cursor];
            cursor += 1;
            EventContext {
                earlier,
                source: &self.source,
                slots: &slots,
                occurrence: &self.position.occurrence,
                ledger,
                heap,
                arena,
            }
            .check(event, site, &EventView::from(actual), facts)
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
    #[expect(
        clippy::too_many_arguments,
        reason = "one exact source publication frontier"
    )]
    pub(crate) fn prepare_exact<'a>(
        self,
        frontier: &Frontier,
        ledger: &Ledger,
        mut registration: Registration<'a>,
        heap: &PagedHeap,
        actual: Program,
        exit: ExitKind,
        position: &Position,
    ) -> Result<PreparedExactPrefix<'a>, PublicationError> {
        if !self.bound.current(frontier)
            || exit != self.bound.source.exit()
            || position != &self.bound.next
        {
            return Err(PublicationError::Miss("source exit or recovery changed"));
        }
        if !self.program.matches(ledger, registration.arena(), &actual) {
            return Err(PublicationError::Miss(
                "source program or dependencies changed",
            ));
        }
        let journal = self
            .program
            .prepare(ledger, registration, heap, actual)
            .map_err(|error| match error {
                logical::MemberFault::Heap(error) => error,
                logical::MemberFault::Native(error) => fault(&error.to_string()),
            })?;
        let before = ledger.balance()?.total()?;
        let after = journal.balance()?.total()?;
        let room = self.bound.room.balance();
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
        Ok(PreparedExactPrefix {
            bound: self.bound,
            journal,
        })
    }

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
        registration: Registration<'a>,
        heap: &PagedHeap,
        actual: Program,
        exit: ExitKind,
        position: &Position,
    ) -> Result<SourceAccepted<'r, PreparedPrefix<'a>>, PublicationError> {
        self.try_map(|accepted| {
            let exact = accepted.evaluation.prepare_exact(
                frontier,
                ledger,
                registration,
                heap,
                actual,
                exit,
                position,
            )?;
            Ok(PreparedPrefix {
                bound: exact.bound,
                journal: exact.journal,
                memory_bound: accepted.memory_bound,
            })
        })
    }
}
impl<'a> SourceAccepted<'_, PreparedPrefix<'a>> {
    pub(crate) fn publish(
        self,
        frontier: &mut Frontier,
        ledger: &mut Ledger,
        semantic: impl FnOnce(&mut Arena, Vec<MemberEdit>),
    ) -> Result<CommittedPrefix, PublicationError> {
        self.publish_with(frontier, ledger, |accepted, ledger| {
            accepted.publish(ledger, semantic)
        })
    }

    pub(crate) fn publish_materialized(
        self,
        frontier: &mut Frontier,
        ledger: &mut Ledger,
        semantic: impl FnOnce(&mut Arena, Vec<MemberEdit>, Vec<logical::VirtualEntity>),
    ) -> Result<CommittedPrefix, PublicationError> {
        self.publish_with(frontier, ledger, |accepted, ledger| {
            accepted.publish_materialized(ledger, semantic)
        })
    }

    fn publish_with(
        self,
        frontier: &mut Frontier,
        ledger: &mut Ledger,
        publish: impl FnOnce(
            logical::AcceptedProgram<'a>,
            &mut Ledger,
        ) -> Result<Materializations, HeapFault>,
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
            let position = prepared.bound.next.clone();
            let exit = prepared.bound.source.exit();
            let accepted = prepared
                .journal
                .admit(prepared.memory_bound)
                .ok_or_else(|| fault("source admission changed"))?;
            let materializations = publish(accepted, ledger)?;
            frontier.revision = revision;
            frontier.position = prepared.bound.next;
            Ok(CommittedPrefix {
                position,
                exit,
                balance,
                materializations,
            })
        })
    }
}
