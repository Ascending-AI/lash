//! A departure is a compiled transfer followed only by source scope releases.
//! Its transient record aliases the carried root, so it adds no logical peak.
use super::*;
use logical::{BoundaryTransaction, Dependencies, Owner as Root, RootValue, Slot};

pub(crate) struct DepartureExit {
    interval: Arc<CompiledInterval>,
    first: Point,
    last: Point,
    input: Option<Owner>,
    output: Option<Owner>,
    releases: Box<[Slot]>,
    terminal: bool,
}

impl Source {
    pub(crate) fn departure_exit(&self, at: &Point) -> Result<Arc<DepartureExit>, Refused> {
        if let Some(exit) = self.departures.lock().map_err(|_| Refused::Source)?.get(at) {
            return Ok(Arc::clone(exit));
        }
        let interval = self.compile_departure_suffix(at)?;
        let slots = self.owner_slots()?;
        let mut events = interval.events();
        let first = events.next().ok_or(Refused::Path)?;
        let active = Owner::ActiveDeparture(self.site.clone());
        if first.point != *at
            || first.output.as_ref() != Some(&active)
            || !first.entities.is_empty()
            || first.peak != Amount::default()
            || first.delta != Amount::default()
            || !matches!(
                interval.exit(),
                ExitKind::Return | ExitKind::Raise | ExitKind::Break | ExitKind::Continue
            )
        {
            return Err(Refused::Source);
        }
        let input = match (interval.exit(), first.inputs.as_ref()) {
            (ExitKind::Return | ExitKind::Raise, [Owner::Operand(rhs)]) => {
                Some(Owner::Operand(rhs.clone()))
            }
            (ExitKind::Break | ExitKind::Continue, []) => None,
            _ => return Err(Refused::Source),
        };
        let mut releases = BTreeSet::new();
        let mut last = None;
        let mut terminal = false;
        let mut output = None;
        for event in events {
            if last.is_some() || !event.entities.is_empty() || event.peak != Amount::default() {
                return Err(Refused::Source);
            }
            match event.point.phase {
                Phase::Leave => {
                    let groups = vec![
                        Owner::Scope(event.point.site.clone()),
                        Owner::Operands(event.point.site.clone()),
                    ];
                    let mut delta =
                        Amount::default().term(Measure::Release(groups.clone().into()), -1);
                    if event.point.site == self.site {
                        delta.fixed -= i128::from(Cost::FRAME);
                        terminal = true;
                        last = Some(event.point.clone());
                    }
                    if !event.inputs.is_empty() || event.output.is_some() || event.delta != delta {
                        return Err(Refused::Source);
                    }
                    for group in groups {
                        for root in slots
                            .group(&group, TaskId::MAIN, 0)
                            .ok_or(Refused::Source)?
                        {
                            let Root::Value { slot, .. } = root else {
                                return Err(Refused::Source);
                            };
                            releases.insert(slot);
                        }
                    }
                }
                Phase::Suspend | Phase::Catch => {
                    if event.inputs.as_ref() != std::slice::from_ref(&active)
                        || event.delta != Amount::default()
                    {
                        return Err(Refused::Source);
                    }
                    output = event.output.clone();
                    last = Some(event.point.clone());
                }
                _ => return Err(Refused::Source),
            }
        }
        let last = last.ok_or(Refused::Exit)?;
        let exit = Arc::new(DepartureExit {
            interval,
            first: at.clone(),
            last,
            input,
            output,
            releases: releases.into_iter().collect(),
            terminal,
        });
        let mut cache = self.departures.lock().map_err(|_| Refused::Source)?;
        Ok(Arc::clone(cache.entry(at.clone()).or_insert(exit)))
    }
}

impl DepartureExit {
    pub(crate) fn interval(&self) -> &Arc<CompiledInterval> {
        &self.interval
    }

    pub(crate) fn next(
        &self,
        source: &Source,
        start: &Position,
        cursor: Option<&Point>,
    ) -> Result<Position, Refused> {
        let unpaid = if self.terminal {
            if cursor.is_some() {
                return Err(Refused::Source);
            }
            None
        } else if let Some(resume) = self.interval.resume() {
            if cursor.is_some() {
                return Err(Refused::Source);
            }
            Some(resume.clone())
        } else {
            let cursor = cursor.ok_or(Refused::Source)?;
            if self.interval.exit() != ExitKind::Continue
                || ![false, true].into_iter().any(|present| {
                    source
                        .next_after_for_cursor(&self.last.site, present)
                        .as_ref()
                        == Ok(cursor)
                })
            {
                return Err(Refused::Source);
            }
            Some(cursor.clone())
        };
        Ok(Position {
            at: self.last.clone(),
            unpaid,
            ..start.clone()
        })
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one source-derived departure boundary"
    )]
    pub(crate) fn prepare<'a>(
        &self,
        source: &Arc<Source>,
        frontier: &Frontier,
        invocation: &Invocation,
        ledger: &Ledger,
        mut registration: Registration<'a>,
        heap: &PagedHeap,
        input: Option<Root>,
        handles: &[doc::arena::ArenaHandle],
        next: Position,
        allowance: Allowance,
        debt: Option<&super::super::charges::ReturnDebt>,
        resources: ResourceFacts,
    ) -> Result<Option<PreparedDeparture<'a>>, HeapFault> {
        let before = &frontier.position;
        if !debt.map_or(allowance.units == 0, |debt| {
            debt.matches(source, &self.first, allowance.units)
        }) {
            return Ok(None);
        }
        if frontier.proven
            || !Arc::ptr_eq(source, &frontier.source)
            || before.at != self.first
            || before.unpaid.as_ref() != Some(&self.first)
            || before.occurrence.invocation != invocation.id()
            || invocation.frame()
                != (Root::Frame {
                    task: before.occurrence.task,
                    invocation: before.occurrence.invocation,
                })
            || next.occurrence != before.occurrence
            || next.at != self.last
            || self.interval.return_loops().is_none_or(|loops| {
                !loops
                    .iter()
                    .eq(before.occurrence.loops.iter().map(|(site, _)| site))
            })
            || allowance.before != 0
            || allowance.close.is_some()
            || allowance.strict
            || allowance.depth != 0
            || !allowance.occurrences.is_empty()
        {
            return Ok(None);
        }
        let slots = source
            .owner_slots()
            .map_err(|_| fault("departure owners failed verification"))?;
        let root = |selector: &Owner| slots.root(selector, before.occurrence.task, invocation.id());
        let expected = match &self.input {
            Some(selector) => match root(selector) {
                Some(root) => Some(root),
                None => return Ok(None),
            },
            None => None,
        };
        if expected != input {
            return Ok(None);
        }
        let cursor = if self.interval.resume().is_none() && !self.terminal {
            next.unpaid.as_ref()
        } else {
            None
        };
        if self.next(source, before, cursor).as_ref() != Ok(&next)
            || !source
                .departure_exit(&self.first)
                .is_ok_and(|exit| std::ptr::eq(exit.as_ref(), self))
        {
            return Ok(None);
        }
        if ledger
            .holding(
                root(&Owner::ActiveDeparture(source.site.clone()))
                    .ok_or_else(|| fault("departure has no record owner"))?,
                heap.pages(),
            )?
            .is_some()
            || ledger
                .holding(Root::Incoming(before.occurrence.task), heap.pages())?
                .is_some()
        {
            return Ok(None);
        }
        if next.unpaid.is_none() != self.terminal {
            return Ok(None);
        }
        let output = if self.terminal {
            Some(Root::Incoming(before.occurrence.task))
        } else {
            match &self.output {
                Some(selector) => Some(
                    root(selector).ok_or_else(|| fault("departure output has no source owner"))?,
                ),
                None => None,
            }
        };
        if let Some(output) = output
            && ledger.holding(output, heap.pages())?.is_some()
        {
            return Ok(None);
        }
        let outputs: Vec<_> = output
            .map(|owner| {
                (
                    owner,
                    input.map_or(RootValue::Departure, RootValue::Existing),
                )
            })
            .into_iter()
            .collect();
        let live = ledger.invocation_owners(invocation, heap.pages())?;
        if input.is_some_and(|input| !live.contains(&input)) {
            return Ok(None);
        }
        let dependencies = match Dependencies::capture(ledger, registration.arena(), handles) {
            Some(dependencies) => dependencies,
            None => return Ok(None),
        };
        let transaction = if self.terminal {
            ledger.prepare_leave_publication(invocation, registration, heap.pages(), &outputs)?
        } else {
            let inputs: Vec<_> = live.into_iter().filter(|owner| {
                Some(*owner) == input || matches!(owner, Root::Value { slot, .. } if self.releases.binary_search(slot).is_ok())
            }).collect();
            ledger.prepare_source_boundary(registration, heap, &inputs, &outputs)?
        };
        let transaction = transaction.bind_dependencies(dependencies)?;
        Ok(Some(PreparedDeparture {
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

pub(crate) struct PreparedDeparture<'a> {
    frontier: std::rc::Rc<()>,
    revision: u64,
    before: Position,
    next: Position,
    transaction: BoundaryTransaction<'a>,
    allowance: Allowance,
    resources: ResourceFacts,
}
impl<'a> PreparedDeparture<'a> {
    pub(crate) fn peak(&self) -> u64 {
        self.transaction.peak()
    }
    pub(crate) fn collect(mut self, ledger: &Ledger, heap: &PagedHeap) -> Result<Self, HeapFault> {
        self.transaction = self.transaction.collect(ledger, heap.pages())?;
        Ok(self)
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
        resources.admit_interval(
            &self.resources.clone(),
            &self.allowance.clone(),
            (self, bound),
        )
    }
    fn current(&self, frontier: &Frontier) -> bool {
        std::rc::Rc::ptr_eq(&self.frontier, &frontier.identity)
            && self.revision == frontier.revision
            && !frontier.proven
            && self.before == frontier.position
    }
}
impl SourceAccepted<'_, (PreparedDeparture<'_>, u64)> {
    pub(crate) fn publish_departure(
        self,
        frontier: &mut Frontier,
        ledger: &mut Ledger,
        semantic: impl FnOnce(&mut Arena),
    ) -> Result<logical::Balance, HeapFault> {
        self.commit(|(mut prepared, bound)| {
            if !prepared.current(frontier) || !prepared.transaction.is_current(ledger) {
                return Err(fault("departure frontier changed"));
            }
            let revision = frontier
                .revision
                .checked_add(1)
                .ok_or_else(|| fault("departure frontier overflow"))?;
            let balance = prepared.transaction.balance()?;
            prepared
                .transaction
                .admit(bound)
                .map_err(|_| fault("departure admission changed"))?
                .publish(ledger, |arena, edits| {
                    assert!(edits.is_empty(), "departure cannot mutate a member");
                    semantic(arena);
                })?;
            frontier.revision = revision;
            frontier.position = prepared.next;
            Ok(balance)
        })
    }
}
