//! Root actions create their canonical atom owners before native completion.
use super::*;

impl Source {
    pub(in crate::compile::room) fn root_action(
        &self,
        at: &Point,
    ) -> Result<&doc::Action, Refused> {
        if at.site.unit != self.site.unit || !at.site.path.starts_with(&self.site.path) {
            return Err(Refused::Source);
        }
        let mut node = doc::Node::Block(&self.body);
        for index in &at.site.path[self.site.path.len()..] {
            node = *node.children().get(*index as usize).ok_or(Refused::Path)?;
        }
        match node {
            doc::Node::Action(action @ doc::Action::Call { .. }) => Ok(action),
            _ => Err(Refused::Source),
        }
    }

    pub(crate) fn native_inputs(&self, at: &Point) -> Result<Box<[Owner]>, Refused> {
        if let Ok(action) = self.root_action(at) {
            return Ok((0..super::super::super::action_atoms(action).len())
                .map(|index| Owner::Atom {
                    at: at.site.clone(),
                    index: index as u32,
                })
                .collect());
        }
        self.compile_event(at, ExitKind::Normal)?
            .events()
            .next()
            .map(|event| event.inputs.clone())
            .ok_or(Refused::Path)
    }
}

impl ExactEvent {
    pub(super) fn check_root_native(
        &self,
        arena: &Arena,
        roots: &ExactRoots<'_>,
        facts: &ExternalFacts<'_>,
    ) -> Result<(), Refused> {
        let [(_, ExactValue::Prepared(proof))] = roots.outputs.as_slice() else {
            return Err(Refused::Facts);
        };
        let contract = facts.native.ok_or(Refused::Facts)?;
        if self.at.phase != Phase::Commit
            || !crate::compile::scalar(&proof.value())
            || !proof.graph().is_current(arena)
            || !facts.entities.is_empty()
            || !facts.scratch.is_empty()
            || !contract.events.is_empty()
        {
            return Err(Refused::Facts);
        }
        let canonical = doc::NativeResourceContract::returned(
            doc::Args::new(facts.arguments, arena.objects(), &arena.semantics()),
            &proof.value(),
        )
        .map_err(|_| Refused::Facts)?;
        if contract != &canonical {
            return Err(Refused::Facts);
        }
        Ok(())
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one root action and its current atom values"
    )]
    pub(crate) fn bind_root_arguments(
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
        sessions: &crate::machine::SourceSessionReads,
    ) -> Result<BoundEvent, Refused> {
        if self.at.phase != Phase::Enter || !roots.inputs.is_empty() {
            return Err(Refused::Source);
        }
        let doc::Action::Call { args, .. } = self.source.root_action(&self.at)? else {
            return Err(Refused::Source);
        };
        if args.len() != roots.outputs.len() {
            return Err(Refused::Facts);
        }
        let slots = self.source.owner_slots()?;
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
        for (index, (atom, (owner, value))) in args.iter().zip(&roots.outputs).enumerate() {
            let expected_owner = slots
                .root(
                    &Owner::Atom {
                        at: self.at.site.clone(),
                        index: index as u32,
                    },
                    next.occurrence.task,
                    next.occurrence.invocation,
                )
                .ok_or(Refused::Source)?;
            if *owner != expected_owner {
                return Err(Refused::Source);
            }
            let expected = match atom {
                doc::Atom::Variable(name) => {
                    match super::super::super::root_owners::visible(
                        &slots.scopes,
                        &self.at.site,
                        name,
                    ) {
                        Some(binding) => {
                            let RootOwner::Value { slot, .. } = slots
                                .root(binding, next.occurrence.task, next.occurrence.invocation)
                                .ok_or(Refused::Source)?
                            else {
                                return Err(Refused::Source);
                            };
                            activation.value(slot).ok_or(Refused::Facts)?
                        }
                        None if self.source.site.unit == lash_kernel_value::sites::Unit::Main => {
                            sessions.value(name, ledger).ok_or(Refused::Facts)?
                        }
                        None => return Err(Refused::Source),
                    }
                }
                doc::Atom::Literal(literal) => match literal {
                    doc::Literal::Null => doc::Value::Null,
                    doc::Literal::Absent => doc::Value::Absent,
                    doc::Literal::Bool(v) => doc::Value::Bool(*v),
                    doc::Literal::Int(v) => doc::Value::Int(v.word().ok_or(Refused::Facts)?),
                    doc::Literal::Float(v) => doc::Value::Float(*v),
                    doc::Literal::Text(v) => {
                        doc::Value::Text(doc::arena::ShortText::new(v).ok_or(Refused::Facts)?)
                    }
                    _ => return Err(Refused::Source),
                },
            };
            if !matches!(value, ExactValue::Native(value) if value.same(&expected)) {
                return Err(Refused::Facts);
            }
        }
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
}
