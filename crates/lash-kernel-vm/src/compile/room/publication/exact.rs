//! One exact source event uses the same affine ledger boundary as a region.
//! Actual values are retained until preparation, so a holding price never
//! authorizes a substituted result or a different source transfer.
use super::*;
mod member;
mod original;
mod root;
pub(crate) use logical::PublicationValue as ExactValue;
use logical::{BoundaryTransaction, Dependencies, Owner as RootOwner, RootValue};
pub(crate) use original::{OriginalBound, OriginalPrepared};

/// These are actual live roots, not executor-authored fees or byte deltas.
/// Preparation consumes them directly under the canonical ledger owner.
#[derive(Default)]
pub(crate) struct ExactRoots<'v> {
    pub(crate) activation: Option<crate::compile::room::placement::DenseActivation<'v>>,
    pub(crate) inputs: Vec<RootOwner>,
    pub(crate) outputs: Vec<(RootOwner, ExactValue)>,
}

type ConstructionInput<'a> = (
    Option<&'a doc::NewObject>,
    Option<(&'a doc::Literal, lash_kernel_value::content_ids::LinkId)>,
    Option<(
        &'a doc::arena::ClosureIdentity,
        &'a [doc::arena::ArenaHandle],
        &'a [Name],
    )>,
    Option<doc::Value>,
);

/// Detached preparations expose actual bodies and their source-ordered phase
/// schedule while registration still permits the source verifier to read them.
/// Native arguments must agree with the evaluated source operands, followed
/// only by omitted optional arguments padded with Absent.
#[derive(Default)]
pub(crate) struct ExternalFacts<'a> {
    pub(crate) operands: &'a [doc::Value],
    pub(crate) arguments: &'a [doc::Value],
    pub(crate) member: Option<&'a MemberEdit>,
    pub(crate) entities: &'a [logical::VirtualEntity],
    pub(crate) scratch: &'a [logical::ScratchEvent],
    pub(crate) native: Option<&'a doc::NativeResourceContract>,
}

/// A dynamic choice is an evaluated source fact. Its successor is derived
/// again from source syntax, rather than accepted from a physical code cursor.
#[derive(Clone, Copy, Default)]
pub(crate) enum Successor {
    #[default]
    Normal,
    Condition(bool),
    Cursor(bool),
}

pub(crate) struct ExactEvent {
    source: Arc<Source>,
    interval: Arc<CompiledInterval>,
    at: Point,
    unpaid: Point,
    member: Option<member::MemberEvent>,
}

/// Entry dependencies, the actual roots and source exit remain inseparable.
/// No acceptance or physical coordinate survives a durable source handoff.
pub(crate) struct BoundEvent {
    frontier: std::rc::Rc<()>,
    revision: u64,
    before: Position,
    next: Position,
    dependencies: Dependencies,
    roots: ExactRoots<'static>,
    room: CheckedRoom,
    allowance: Allowance,
    resources: ResourceFacts,
}

pub(crate) struct PreparedEvent<'a> {
    frontier: std::rc::Rc<()>,
    revision: u64,
    before: Position,
    next: Position,
    transaction: BoundaryTransaction<'a>,
    release_only: bool,
    allowance: Allowance,
    resources: ResourceFacts,
}

impl ExactEvent {
    pub(crate) fn compile(
        source: &Arc<Source>,
        interval: Arc<CompiledInterval>,
        successor: Successor,
    ) -> Result<Self, Refused> {
        let mut events = interval.events();
        let at = events.next().ok_or(Refused::Path)?.point.clone();
        if events.next().is_some()
            || interval.owner() != source.owner
            || interval.defining_source() != &source.site
            || !(interval.exit() == ExitKind::Normal
                || (at.phase == Phase::Enter
                    && interval.exit() == ExitKind::Action
                    && source.root_action(&at).is_ok()))
        {
            return Err(Refused::Source);
        }
        drop(events);
        if !Arc::ptr_eq(&interval, &source.compile_event(&at, interval.exit())?) {
            return Err(Refused::Source);
        }
        let unpaid = source.exact_successor(&at, successor)?;
        let member = member::MemberEvent::compile(source, &at)?;
        Ok(Self {
            member,
            source: Arc::clone(source),
            interval,
            at,
            unpaid,
        })
    }

    #[expect(clippy::too_many_arguments, reason = "one actual source event entry")]
    pub(crate) fn bind(
        &self,
        frontier: &Frontier,
        ledger: &Ledger,
        arena: &Arena,
        heap: &PagedHeap,
        execution: &mut impl Execution,
        roots: ExactRoots<'_>,
        next: Position,
        allowance: Allowance,
        resources: ResourceFacts,
    ) -> Result<BoundEvent, Refused> {
        self.bind_paid_external(
            frontier,
            ledger,
            arena,
            heap,
            execution,
            roots,
            next,
            allowance,
            resources,
            ExternalFacts::default(),
        )
    }

    /// The precise stream has closed this event's canonical fixed charges.
    /// Native completion still requires its canonical measured debt.
    #[expect(
        clippy::too_many_arguments,
        reason = "one already-paid external source event"
    )]
    pub(crate) fn bind_paid_external(
        &self,
        frontier: &Frontier,
        ledger: &Ledger,
        arena: &Arena,
        heap: &PagedHeap,
        execution: &mut impl Execution,
        roots: ExactRoots<'_>,
        next: Position,
        allowance: Allowance,
        resources: ResourceFacts,
        external: ExternalFacts<'_>,
    ) -> Result<BoundEvent, Refused> {
        if allowance.before != 0
            || allowance.units != 0
            || allowance.close.is_some()
            || allowance.strict
            || allowance.depth != 0
            || !allowance.occurrences.is_empty()
            || external.native.is_some()
        {
            return Err(Refused::Source);
        }
        self.check_external_roots(&roots, &external, &next.occurrence)?;
        self.bind_view(
            frontier, ledger, arena, heap, execution, roots, next, allowance, resources, external,
            None,
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one authenticated external source event"
    )]
    pub(crate) fn bind_external(
        &self,
        frontier: &Frontier,
        ledger: &Ledger,
        arena: &Arena,
        heap: &PagedHeap,
        execution: &mut impl Execution,
        roots: ExactRoots<'_>,
        next: Position,
        allowance: Allowance,
        resources: ResourceFacts,
        external: ExternalFacts<'_>,
        debt: &crate::compile::room::charges::ChargeDebt<'_>,
    ) -> Result<BoundEvent, Refused> {
        if !debt.matches(&self.source, &self.at, &self.at) {
            return Err(Refused::Source);
        }
        if let Some(mut canonical) = debt.native_allowance(allowance.safe_point) {
            if allowance.depth < canonical.depth {
                return Err(Refused::Source);
            }
            canonical.depth = allowance.depth;
            if allowance != canonical {
                return Err(Refused::Source);
            }
        } else if allowance.units != debt.units()
            || !allowance.strict
            || allowance.before != 0
            || allowance.close.is_some()
            || !allowance.occurrences.is_empty()
        {
            return Err(Refused::Source);
        }
        self.check_external_roots(&roots, &external, &next.occurrence)?;
        let mut external = external;
        if self.source.root_action(&self.at).is_ok() {
            self.check_root_native(arena, &roots, &external)?;
            external.native = None;
        }
        self.bind_view(
            frontier, ledger, arena, heap, execution, roots, next, allowance, resources, external,
            None,
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one constructor and its source boundary"
    )]
    pub(crate) fn bind_construction(
        &self,
        frontier: &Frontier,
        ledger: &Ledger,
        arena: &Arena,
        heap: &PagedHeap,
        execution: &mut impl Execution,
        roots: ExactRoots<'_>,
        next: Position,
        allowance: Allowance,
        resources: ResourceFacts,
        external: ExternalFacts<'_>,
        construction: ConstructionInput<'_>,
        debt: Option<&crate::compile::room::charges::ChargeDebt<'_>>,
    ) -> Result<BoundEvent, Refused> {
        if let Some(debt) = debt {
            if !debt.matches(&self.source, &self.at, &self.at)
                || debt.native_allowance(allowance.safe_point).as_ref() != Some(&allowance)
            {
                return Err(Refused::Source);
            }
        } else if allowance.before != 0
            || allowance.units != 0
            || allowance.close.is_some()
            || allowance.strict
            || allowance.depth != 0
            || !allowance.occurrences.is_empty()
        {
            return Err(Refused::Source);
        }
        self.check_external_roots(&roots, &external, &next.occurrence)?;
        self.bind_view(
            frontier,
            ledger,
            arena,
            heap,
            execution,
            roots,
            next,
            allowance,
            resources,
            external,
            Some(super::super::interval::construction::ConstructorFacts {
                construction: construction.0,
                literal: construction.1,
                closure: construction.2,
                cell: construction.3,
            }),
        )
    }

    fn check_external_roots(
        &self,
        roots: &ExactRoots<'_>,
        external: &ExternalFacts<'_>,
        occurrence: &Occurrence,
    ) -> Result<(), Refused> {
        if external.native.is_some()
            && (external.operands.len() != roots.inputs.len()
                || external.arguments.len() < external.operands.len()
                || !external
                    .arguments
                    .iter()
                    .zip(external.operands)
                    .all(|(a, b)| a.same(b))
                || external.arguments[external.operands.len()..]
                    .iter()
                    .any(|v| !matches!(v, doc::Value::Absent)))
        {
            return Err(Refused::Facts);
        }
        if let Some(edit) = external.member {
            let event = self.interval.events().next().ok_or(Refused::Path)?;
            if external.operands.len() != event.inputs.len() {
                return Err(Refused::Facts);
            }
            self.member
                .as_ref()
                .ok_or(Refused::Source)?
                .check(edit, external.operands)?;
        }
        if external.native.is_some() || external.member.is_some() {
            let slots = self.source.owner_slots()?;
            let event = self.interval.events().next().ok_or(Refused::Path)?;
            let inputs = self.source.native_inputs(&event.point)?;
            let expected: Vec<_> = inputs
                .iter()
                .map(|input| {
                    slots
                        .root(input, occurrence.task, occurrence.invocation)
                        .ok_or(Refused::Source)
                })
                .collect::<Result<_, _>>()?;
            if expected != roots.inputs {
                return Err(Refused::Source);
            }
        }
        Ok(())
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one exact source event and its actual facts"
    )]
    fn bind_view(
        &self,
        frontier: &Frontier,
        ledger: &Ledger,
        arena: &Arena,
        heap: &PagedHeap,
        execution: &mut impl Execution,
        roots: ExactRoots<'_>,
        next: Position,
        allowance: Allowance,
        resources: ResourceFacts,
        external: ExternalFacts<'_>,
        construction: Option<super::super::interval::construction::ConstructorFacts<'_>>,
    ) -> Result<BoundEvent, Refused> {
        if frontier.proven
            || !Arc::ptr_eq(&self.source, &frontier.source)
            || frontier.position.at != self.at
            || frontier.position.unpaid.as_ref() != Some(&self.at)
            || next.at != self.at
            || next.unpaid.as_ref() != Some(&self.unpaid)
            || next.occurrence != frontier.position.occurrence
        {
            return Err(Refused::Source);
        }
        for (_, value) in &roots.outputs {
            if let ExactValue::Prepared(value) = value
                && (!value.graph().is_current(arena)
                    || !std::ptr::eq(external.entities, value.graph().entities()))
            {
                return Err(Refused::Facts);
            }
        }
        let slots = self.source.owner_slots()?;
        let view = EventView {
            inputs: &roots.inputs,
            outputs: &roots.outputs,
            borrowed: &[],
            member: external.member,
            entities: external.entities,
            frame: None,
            scratch: external.scratch,
            native: external.native,
        };
        let mut handles = Vec::new();
        let room = self
            .interval
            .evaluate_checked(execution, |event, site, facts| {
                if external.native.is_none()
                    && event.peak.measures().any(|measure| {
                        matches!(
                            measure,
                            Measure::CallPeak {
                                callee: CallSource::Native(_),
                                ..
                            }
                        )
                    })
                {
                    return Err(Refused::Facts);
                }
                handles.extend_from_slice(facts.dependencies().ok_or(Refused::Facts)?);
                let context = EventContext {
                    earlier: &[],
                    source: &self.source,
                    slots: &slots,
                    occurrence: &frontier.position.occurrence,
                    ledger,
                    heap,
                    arena,
                };
                context.check(event, site, &view, facts)?;
                if let Some(descriptor) = &site.construction {
                    descriptor.check(
                        (&slots, &frontier.position.occurrence, arena, heap),
                        event,
                        &roots,
                        &external,
                        construction.as_ref(),
                        frontier.link,
                        |owner, cell| context.check_cell(owner, cell),
                    )?;
                } else if construction.is_some() {
                    return Err(Refused::Source);
                }
                // A local move preserves its actual provenance, including page and
                // frozen roots. Equal-sized Native values cannot replace that move.
                if external.member.is_none()
                    && construction
                        .as_ref()
                        .is_none_or(|facts| facts.cell.is_none())
                    && (site.assignment.is_some()
                        || matches!(
                            event.output,
                            Some(Owner::Binding { .. } | Owner::SessionCell { .. })
                        ))
                {
                    if event.inputs.len() != 1 || roots.outputs.len() != 1 {
                        return Err(Refused::Retained);
                    }
                    let input = slots
                        .root(
                            &event.inputs[0],
                            next.occurrence.task,
                            next.occurrence.invocation,
                        )
                        .ok_or(Refused::Source)?;
                    if !matches!(roots.outputs[0].1, ExactValue::Existing(owner) if owner == input)
                    {
                        return Err(Refused::Retained);
                    }
                }
                if external.native.is_none()
                    && matches!(event.output, Some(Owner::Operand(_)))
                    && let Ok(input) = self.source.scalar_input(&slots, &event.point)
                {
                    let expected = match input {
                        super::super::region::ScalarInput::Literal(value) => value,
                        super::super::region::ScalarInput::Binding(slot) => {
                            let activation = roots.activation.as_ref().ok_or(Refused::Facts)?;
                            let invocation = ledger
                                .invocation(
                                    next.occurrence.task,
                                    next.occurrence.invocation,
                                    heap.pages(),
                                )
                                .map_err(|_| Refused::Facts)?;
                            if !activation.current(&self.source, ledger, &invocation) {
                                return Err(Refused::Facts);
                            }
                            activation.value(slot).ok_or(Refused::Facts)?
                        }
                    };
                    if roots.outputs.len() != 1
                        || !match &roots.outputs[0].1 {
                            ExactValue::Native(value) => value.same(&expected),
                            ExactValue::Prepared(proof) => {
                                proof.graph().is_current(arena) && proof.value().same(&expected)
                            }
                            _ => false,
                        }
                    {
                        return Err(Refused::Facts);
                    }
                }
                Ok(())
            })?;
        for (_, value) in &roots.outputs {
            match value {
                ExactValue::Native(value) => handles.extend(value.handle()),
                ExactValue::Prepared(value) => {
                    handles.extend_from_slice(value.graph().dependencies())
                }
                ExactValue::Capture(handle) => handles.push(*handle),
                _ => {}
            }
        }
        let dependencies = Dependencies::capture(ledger, arena, &handles).ok_or(Refused::Facts)?;
        Ok(BoundEvent {
            frontier: std::rc::Rc::clone(&frontier.identity),
            revision: frontier.revision,
            before: frontier.position.clone(),
            next,
            dependencies,
            roots: ExactRoots {
                activation: None,
                inputs: roots.inputs,
                outputs: roots.outputs,
            },
            room,
            allowance,
            resources,
        })
    }
}

impl BoundEvent {
    pub(crate) fn prepare<'a>(
        self,
        frontier: &Frontier,
        ledger: &Ledger,
        mut registration: Registration<'a>,
        heap: &PagedHeap,
    ) -> Result<PreparedEvent<'a>, PublicationError> {
        if !current(&self.frontier, self.revision, &self.before, frontier)
            || !self.dependencies.is_current(ledger, registration.arena())
        {
            return Err(PublicationError::Miss("exact entry dependencies changed"));
        }
        let outputs: Vec<_> = self
            .roots
            .outputs
            .iter()
            .map(|(owner, value)| (*owner, root_value(value)))
            .collect();
        let transaction =
            ledger.prepare_source_boundary(registration, heap, &self.roots.inputs, &outputs)?;
        self.finish_external(frontier, ledger, transaction)
    }

    /// Finish against the transaction prepared from the same actual graph.
    /// The transaction owns registration; no borrowed activation escapes bind.
    pub(crate) fn finish_external<'a>(
        self,
        frontier: &Frontier,
        ledger: &Ledger,
        mut transaction: BoundaryTransaction<'a>,
    ) -> Result<PreparedEvent<'a>, PublicationError> {
        if !current(&self.frontier, self.revision, &self.before, frontier) {
            return Err(PublicationError::Miss("external source frontier changed"));
        }
        let outputs: Vec<_> = self
            .roots
            .outputs
            .iter()
            .map(|(owner, value)| (*owner, root_value(value)))
            .collect();
        if !transaction.matches_roots(ledger, &self.roots.inputs, &outputs) {
            return Err(PublicationError::Miss("external boundary roots changed"));
        }
        let mut transaction = transaction.bind_dependencies(self.dependencies)?;
        if !transaction.is_current(ledger) {
            return Err(PublicationError::Miss(
                "external source dependencies changed",
            ));
        }
        let before = ledger.balance()?.total()?;
        let balance = self.room.balance();
        if i128::from(transaction.balance()?.total()?) - i128::from(before)
            != balance.retained_delta
            || transaction.peak()
                > before
                    .checked_add(balance.peak_extra)
                    .ok_or_else(|| fault("exact source peak overflow"))?
        {
            return Err(PublicationError::Miss(
                "exact source room disagrees with actual roots",
            ));
        }
        let transaction = transaction
            .require_source_room(balance.peak_extra)
            .ok_or_else(|| fault("exact source room overflow"))?;
        let release_only =
            self.roots.outputs.is_empty() && balance.peak_extra == 0 && balance.retained_delta <= 0;
        Ok(PreparedEvent {
            frontier: self.frontier,
            revision: self.revision,
            before: self.before,
            next: self.next,
            transaction,
            release_only,
            allowance: self.allowance,
            resources: self.resources,
        })
    }
}

impl<'a> PreparedEvent<'a> {
    /// The canonical event creates no root or interior acquisition. An
    /// over-bound incoming value may therefore take its precise retirement
    /// path before the statement's final capacity verdict.
    pub(crate) fn releases_only(&self) -> bool {
        self.release_only
    }

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
        memory_bound: u64,
    ) -> Result<Option<SourceAccepted<'r, (Self, u64)>>, HeapFault> {
        if !current(&self.frontier, self.revision, &self.before, frontier)
            || !self.transaction.is_current(ledger)
            || self.peak() > memory_bound
        {
            return Ok(None);
        }
        let facts = self.resources.clone();
        let need = self.allowance.clone();
        resources.admit_interval(&facts, &need, (self, memory_bound))
    }
}

impl SourceAccepted<'_, (PreparedEvent<'_>, u64)> {
    pub(crate) fn publish_event(
        self,
        frontier: &mut Frontier,
        ledger: &mut Ledger,
        semantic: impl FnOnce(&mut Arena, Vec<MemberEdit>),
    ) -> Result<logical::Balance, PublicationError> {
        self.commit(|(mut prepared, memory_bound)| {
            if !current(
                &prepared.frontier,
                prepared.revision,
                &prepared.before,
                frontier,
            ) || !prepared.transaction.is_current(ledger)
            {
                return Err(PublicationError::Miss("exact source frontier changed"));
            }
            let revision = frontier
                .revision
                .checked_add(1)
                .ok_or_else(|| fault("source frontier overflow"))?;
            let balance = prepared.transaction.balance()?;
            let accepted = prepared
                .transaction
                .admit(memory_bound)
                .map_err(|_| fault("exact source admission changed"))?;
            accepted.publish(ledger, semantic)?;
            frontier.position = prepared.next;
            frontier.revision = revision;
            Ok(balance)
        })
    }
}

fn current(
    identity: &std::rc::Rc<()>,
    revision: u64,
    before: &Position,
    frontier: &Frontier,
) -> bool {
    std::rc::Rc::ptr_eq(identity, &frontier.identity)
        && revision == frontier.revision
        && !frontier.proven
        && before == &frontier.position
}

impl Source {
    pub(crate) fn exact_successor(
        &self,
        at: &Point,
        successor: Successor,
    ) -> Result<Point, Refused> {
        match at.phase {
            // A native call act answers at its Enter and continues as if
            // committed; a root action resumes at its own Commit.
            Phase::Enter if self.answers_at_enter(&at.site) => self.next_after_commit(&at.site),
            Phase::Enter => {
                self.root_action(at)?;
                Ok(Point {
                    site: at.site.clone(),
                    phase: Phase::Commit,
                })
            }
            Phase::Commit | Phase::Restore => self
                .next_after_commit(&at.site)
                .or_else(|_| self.next_after_loop_commit(at)),
            Phase::Open => self.compile_leaf(&at.site).map(|(_, next, _)| next),
            Phase::Test => match successor {
                Successor::Condition(taken) => self
                    .next_after_while_test(&at.site, taken)
                    .or_else(|_| self.next_after_if_test(&at.site, taken)),
                Successor::Normal => {
                    let mut node = doc::Node::Block(&self.body);
                    for index in &at.site.path[self.site.path.len()..] {
                        node = *node.children().get(*index as usize).ok_or(Refused::Path)?;
                    }
                    if !matches!(node, doc::Node::Expr(doc::Expr::Let { .. })) {
                        return Err(Refused::Facts);
                    }
                    self.first_event(&at.site.child(1))
                }
                Successor::Cursor(true) => {
                    let mut node = doc::Node::Block(&self.body);
                    for index in &at.site.path[self.site.path.len()..] {
                        node = *node.children().get(*index as usize).ok_or(Refused::Path)?;
                    }
                    if !matches!(node, doc::Node::Stmt(doc::Stmt::For { via: None, .. })) {
                        return Err(Refused::Source);
                    }
                    self.first_event(&at.site.child(1))
                }
                Successor::Cursor(false) => Err(Refused::Path),
            },
            Phase::Leave if at.site != self.site => self.exact_scope_successor(&at.site, successor),
            _ => Err(Refused::Path),
        }
    }
    fn next_after_loop_commit(&self, at: &Point) -> Result<Point, Refused> {
        self.verify()?;
        if at.phase != Phase::Commit
            || at.site.unit != self.site.unit
            || !at.site.path.starts_with(&self.site.path)
            || at.site.path.len() <= self.site.path.len()
            || !self.room.events.contains_key(at)
        {
            return Err(Refused::Path);
        }
        let mut parent = at.site.clone();
        let index = parent.path.pop().ok_or(Refused::Path)?;
        let mut node = doc::Node::Block(&self.body);
        for index in &parent.path[self.site.path.len()..] {
            node = *node.children().get(*index as usize).ok_or(Refused::Path)?;
        }
        let doc::Node::Block(block) = node else {
            return Err(Refused::Path);
        };
        if !matches!(
            block.get(index as usize),
            Some(doc::Stmt::For { via: None, .. } | doc::Stmt::While { .. })
        ) {
            return Err(Refused::Source);
        }
        if (index as usize) + 1 < block.len() {
            self.first_event(&parent.child(index.checked_add(1).ok_or(Refused::Path)?))
        } else {
            let next = Point {
                phase: if parent == self.site {
                    Phase::Fallthrough
                } else {
                    Phase::Leave
                },
                site: parent,
            };
            self.room
                .events
                .contains_key(&next)
                .then_some(next)
                .ok_or(Refused::Path)
        }
    }

    fn exact_scope_successor(&self, at: &Site, successor: Successor) -> Result<Point, Refused> {
        if at.unit != self.site.unit || !at.path.starts_with(&self.site.path) {
            return Err(Refused::Path);
        }
        let mut node = doc::Node::Block(&self.body);
        let mut path = self.site.clone();
        let mut parents = Vec::new();
        for &index in &at.path[self.site.path.len()..] {
            parents.push((node, path.clone(), index));
            node = *node.children().get(index as usize).ok_or(Refused::Path)?;
            path = path.child(index);
        }
        if !matches!(node, doc::Node::Block(_)) {
            return Err(Refused::Path);
        }
        let (parent, site, index) = parents.last().ok_or(Refused::Path)?;
        match parent {
            doc::Node::Stmt(doc::Stmt::While { .. }) if *index == 1 => {
                return self.first_event(&site.child(0));
            }
            doc::Node::Stmt(doc::Stmt::For { .. }) if *index == 1 => {
                let Successor::Cursor(continued) = successor else {
                    return Err(Refused::Facts);
                };
                return self.next_after_for_cursor(site, continued);
            }
            doc::Node::Stmt(doc::Stmt::Try(statement)) => {
                let scope = if *index == 0 {
                    TryScope::Body
                } else if *index == 1 && statement.catch.is_some() {
                    TryScope::Catch
                } else {
                    TryScope::Finally
                };
                return self.next_after_try_scope(site, scope);
            }
            _ => {}
        }
        // Ordinary selected blocks finish their enclosing statement. Resume at
        // the next statement in its source block, or that block's own exit.
        for (node, site, index) in parents.into_iter().rev() {
            if let doc::Node::Block(body) = node {
                return if (index as usize) + 1 < body.len() {
                    self.first_event(&site.child(index + 1))
                } else {
                    Ok(Point {
                        phase: if site == self.site {
                            Phase::Fallthrough
                        } else {
                            Phase::Leave
                        },
                        site,
                    })
                };
            }
        }
        Err(Refused::Path)
    }
}

fn root_value(value: &ExactValue) -> RootValue<'_> {
    match value {
        ExactValue::Native(value) => RootValue::Value(value),
        ExactValue::Prepared(value) => RootValue::Prepared(value),
        ExactValue::Existing(owner) => RootValue::Existing(*owner),
        ExactValue::Capture(handle) => RootValue::Capture(*handle),
        ExactValue::Page(value) => RootValue::Page(value),
        ExactValue::Frozen(value) => RootValue::Frozen(value),
        ExactValue::Frame => RootValue::Frame,
        ExactValue::Departure => RootValue::Departure,
    }
}
