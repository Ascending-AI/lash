//! Source control boundaries retain cached source roles and publish final roots.
use super::*;
use logical::{BoundaryTransaction, Dependencies, Owner as Root, RootValue};

#[derive(Default)]
pub(crate) struct ControlRoots {
    pub(crate) inputs: Vec<Root>,
    pub(crate) outputs: Vec<(Root, logical::PublicationValue)>,
    pub(crate) borrowed: Vec<Root>,
}

/// The interval and its immutable publication metadata are cached by Source.
/// A descriptor borrows that authority, never a physical instruction number.
pub(crate) struct ControlExit {
    interval: Arc<CompiledInterval>,
    next: Point,
}
impl ControlExit {
    pub(crate) fn compile(
        source: &Source,
        interval: Arc<CompiledInterval>,
    ) -> Result<Self, Refused> {
        let mut events = interval.events();
        let event = events.next().ok_or(Refused::Path)?;
        if events.next().is_some()
            || interval.owner() != source.owner
            || interval.defining_source() != &source.site
        {
            return Err(Refused::Source);
        }
        let canonical = match (event.point.phase, interval.exit()) {
            (Phase::Enter, ExitKind::Action) => {
                source.compile_event(&event.point, ExitKind::Action)?
            }
            (Phase::Commit, ExitKind::Normal) => {
                let [Owner::Operand(from)] = event.inputs.as_ref() else {
                    return Err(Refused::Source);
                };
                source.compile_join(from, &event.point.site)?
            }
            _ => return Err(Refused::Source),
        };
        if !Arc::ptr_eq(&canonical, &interval) {
            return Err(Refused::Source);
        }
        let next = if interval.exit() == ExitKind::Action {
            Point {
                site: event.point.site.clone(),
                phase: Phase::Commit,
            }
        } else {
            source.next_after_commit(&event.point.site)?
        };
        drop(events);
        Ok(Self { interval, next })
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one authenticated source control boundary"
    )]
    pub(crate) fn prepare<'a>(
        &self,
        source: &Arc<Source>,
        frontier: &Frontier,
        invocation: &Invocation,
        ledger: &Ledger,
        mut registration: Registration<'a>,
        heap: &PagedHeap,
        execution: &mut impl Execution,
        roots: ControlRoots,
        next: Position,
        allowance: Allowance,
        resources: ResourceFacts,
    ) -> Result<Option<PreparedControl<'a>>, HeapFault> {
        let before = &frontier.position;
        let event = self
            .interval
            .events()
            .next()
            .ok_or_else(|| fault("control event disappeared"))?;
        if Self::compile(source, Arc::clone(&self.interval)).is_err()
            || frontier.proven
            || !Arc::ptr_eq(source, &frontier.source)
            || self.interval.owner() != source.owner
            || self.interval.defining_source() != &source.site
            || before.at != event.point
            || before.unpaid.as_ref() != Some(&event.point)
            || before.occurrence.invocation != invocation.id()
            || ledger
                .invocation(before.occurrence.task, invocation.id(), heap.pages())
                .as_ref()
                .ok()
                != Some(invocation)
            || next.at != event.point
            || next.unpaid.as_ref() != Some(&self.next)
            || next.occurrence != before.occurrence
            || allowance.before != 0
            || allowance.units != 0
            || allowance.close.is_some()
            || allowance.strict
        {
            return Ok(None);
        }
        let slots = source
            .owner_slots()
            .map_err(|_| fault("control owners failed verification"))?;
        let expected: Vec<_> = event
            .inputs
            .iter()
            .map(|input| {
                slots
                    .root(input, before.occurrence.task, invocation.id())
                    .ok_or(Refused::Source)
            })
            .collect::<Result<_, _>>()
            .map_err(|_| fault("control input has no source owner"))?;
        if self.interval.exit() == ExitKind::Action {
            if roots.borrowed != expected
                || allowance.occurrences.len() != 1
                || before
                    .occurrence
                    .action
                    .as_ref()
                    .is_none_or(|(at, _)| at != &event.point)
                || allowance.depth > 1
            {
                return Ok(None);
            }
        } else if !allowance.occurrences.is_empty() || allowance.depth != 0 {
            return Ok(None);
        }
        let view = EventView {
            inputs: &roots.inputs,
            outputs: &roots.outputs,
            borrowed: &roots.borrowed,
            member: None,
            entities: &[],
            frame: None,
            scratch: &[],
            native: None,
        };
        let mut handles = Vec::new();
        let checked = self
            .interval
            .evaluate_checked(execution, |event, site, facts| {
                handles.extend_from_slice(facts.dependencies().ok_or(Refused::Facts)?);
                EventContext {
                    earlier: &[],
                    source,
                    slots: &slots,
                    occurrence: &before.occurrence,
                    ledger,
                    heap,
                    arena: registration.arena(),
                }
                .check(event, site, &view, facts)
            });
        let Ok(room) = checked else { return Ok(None) };
        // Control transfers never replace an observed value with same-priced bytes.
        if roots
            .outputs
            .iter()
            .any(|(_, value)| !matches!(value, logical::PublicationValue::Existing(_)))
        {
            return Ok(None);
        }
        for owner in roots.borrowed.iter().chain(&roots.inputs) {
            if ledger.holding(*owner, heap.pages())?.is_none() {
                return Ok(None);
            }
        }
        let Some(dependencies) = Dependencies::capture(ledger, registration.arena(), &handles)
        else {
            return Ok(None);
        };
        let outputs: Vec<_> = roots
            .outputs
            .iter()
            .map(|(owner, value)| {
                let logical::PublicationValue::Existing(input) = value else {
                    unreachable!()
                };
                (*owner, RootValue::Existing(*input))
            })
            .collect();
        let transaction = ledger
            .prepare_source_boundary(registration, heap, &roots.inputs, &outputs)?
            .bind_dependencies(dependencies)?;
        let before_total = ledger.balance()?.total()?;
        if i128::from(transaction.balance()?.total()?) - i128::from(before_total)
            != room.balance().retained_delta
            || transaction.peak()
                > before_total
                    .checked_add(room.balance().peak_extra)
                    .ok_or_else(|| fault("control peak overflow"))?
        {
            return Ok(None);
        }
        Ok(Some(PreparedControl {
            frontier: std::rc::Rc::clone(&frontier.identity),
            revision: frontier.revision,
            before: before.clone(),
            next,
            transaction,
            allowance,
            resources,
        }))
    }
}

pub(crate) struct PreparedControl<'a> {
    frontier: std::rc::Rc<()>,
    revision: u64,
    before: Position,
    next: Position,
    transaction: BoundaryTransaction<'a>,
    allowance: Allowance,
    resources: ResourceFacts,
}
impl<'a> PreparedControl<'a> {
    pub(crate) fn peak(&self) -> u64 {
        self.transaction.peak()
    }
    pub(crate) fn collect(mut self, ledger: &Ledger, heap: &PagedHeap) -> Result<Self, HeapFault> {
        self.transaction = self.transaction.collect(ledger, heap.pages())?;
        Ok(self)
    }
    fn current(&self, frontier: &Frontier) -> bool {
        std::rc::Rc::ptr_eq(&self.frontier, &frontier.identity)
            && self.revision == frontier.revision
            && !frontier.proven
            && self.before == frontier.position
    }
    pub(crate) fn admit<'r>(
        mut self,
        frontier: &Frontier,
        ledger: &Ledger,
        resources: SourceResources<'r>,
        bound: u64,
    ) -> Result<Option<SourceAccepted<'r, (Self, u64)>>, HeapFault> {
        if !self.current(frontier) || !self.transaction.is_current(ledger) || self.peak() > bound {
            return Ok(None);
        }
        if let Some(index) = self.allowance.occurrences.first()
            && self
                .before
                .occurrence
                .action
                .as_ref()
                .is_none_or(|(_, ordinal)| resources.counters.current(*index) != Some(*ordinal))
        {
            return Ok(None);
        }
        resources.admit_interval(
            &self.resources.clone(),
            &self.allowance.clone(),
            (self, bound),
        )
    }
}
impl SourceAccepted<'_, (PreparedControl<'_>, u64)> {
    pub(crate) fn publish_control(
        self,
        frontier: &mut Frontier,
        ledger: &mut Ledger,
        semantic: impl FnOnce(&mut Arena),
    ) -> Result<logical::Balance, HeapFault> {
        self.commit(|(mut prepared, bound)| {
            if !prepared.current(frontier) || !prepared.transaction.is_current(ledger) {
                return Err(fault("control frontier changed"));
            }
            let revision = frontier
                .revision
                .checked_add(1)
                .ok_or_else(|| fault("control frontier overflow"))?;
            let balance = prepared.transaction.balance()?;
            prepared
                .transaction
                .admit(bound)
                .map_err(|_| fault("control admission changed"))?
                .publish(ledger, |arena, edits| {
                    assert!(edits.is_empty());
                    semantic(arena);
                })?;
            frontier.revision = revision;
            frontier.position = prepared.next;
            Ok(balance)
        })
    }
}
