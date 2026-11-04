//! Consume candidates without completing their reservation before admission.
use super::*;
use crate::compile::room::publication::Occurrence;

pub(crate) struct ScopedPrefix<'plan> {
    source: Arc<Source>,
    caller: logical::Invocation,
    range: logical::InvocationRange,
    completed: u64,
    dependencies: logical::Dependencies,
    calls: Vec<SettledCall<'plan>>,
}

pub(in crate::compile::room) struct ScopedPrefixParts<'plan> {
    pub(in crate::compile::room) range: logical::InvocationRange,
    pub(in crate::compile::room) completed: u64,
    pub(in crate::compile::room) dependencies: logical::Dependencies,
    pub(in crate::compile::room) calls: Vec<SettledCall<'plan>>,
}

/// The measured program and cold roles outlive the candidate's ledger borrow.
/// Dependencies and the actual reservation have exactly one owner, the prefix.
pub(crate) struct SettledCall<'plan> {
    pub(in crate::compile::room) call: &'plan VerifiedCall,
    pub(in crate::compile::room) debt: ComposedDebt<'plan>,
    pub(in crate::compile::room) room: CheckedRoom,
    pub(in crate::compile::room) depth: u64,
    pub(in crate::compile::room) occurrences: Vec<usize>,
    pub(in crate::compile::room) caller: logical::Invocation,
    pub(in crate::compile::room) invocation: logical::Invocation,
    caller_values: Vec<(Slot, Value)>,
    pub(in crate::compile::room) roles: Vec<(Slot, Value)>,
    captures: Vec<(Slot, doc::arena::ArenaHandle)>,
    evaluated: EvaluatedArguments,
    completions: Vec<ScopedCompletion>,
}

impl VerifiedCall {
    /// Root actions borrow their source binding for the callable. Nested
    /// value calls consume the separately evaluated callable operand.
    pub(crate) fn consumed_roles(&self) -> impl Iterator<Item = Slot> + '_ {
        self.argument_slots.iter().copied().chain(
            self.callable
                .filter(|slot| matches!(slot, Slot::Operand(_))),
        )
    }
}

impl CallScope<'_> {
    pub(crate) fn consume_prefix<'plan>(
        self,
        arena: &Arena,
        calls: Vec<ScopedCall<'plan>>,
    ) -> Result<Option<ScopedPrefix<'plan>>, doc::heap::HeapFault> {
        if calls.is_empty()
            || self.identity.get() != 0
            || !self.range.is_current(self.ledger, &self.caller)
        {
            return Ok(None);
        }
        let mut offset = 0_u64;
        // Validate the entire selected prefix before taking any of its facts.
        for candidate in &calls {
            let width = candidate.completed_invocations();
            if !Rc::ptr_eq(&self.identity, &candidate.identity)
                || candidate.offset != offset
                || candidate.caller != self.caller
                || !candidate.current(self.ledger, arena)
                || self.range.invocation(offset).as_ref() != Some(&candidate.invocation)
                || candidate.target.as_ref().is_some_and(|target| {
                    self.target
                        .as_ref()
                        .is_none_or(|actual| !Rc::ptr_eq(actual, target))
                })
                || width == 0
            {
                return Ok(None);
            }
            let Some(next) = offset
                .checked_add(width)
                .filter(|next| *next <= self.range.len())
            else {
                return Ok(None);
            };
            offset = next;
        }
        let mut dependencies = None::<logical::Dependencies>;
        let mut completed = Vec::with_capacity(calls.len());
        for candidate in calls {
            let Some(debt) = candidate.debt() else {
                return Ok(None);
            };
            if !candidate.call.owns_debt(&debt) {
                return Ok(None);
            }
            let room = candidate.checked_room();
            let depth = candidate.depth();
            let occurrences = candidate.occurrences().to_vec();
            let roles = candidate.roles().collect();
            let captures = candidate
                .target
                .as_ref()
                .map_or(&[][..], |target| target.captures.as_slice())
                .iter()
                .map(|(slot, cell, _)| (*slot, *cell))
                .collect();
            let ScopedCall {
                call,
                caller,
                invocation,
                caller_values,
                evaluated,
                completions,
                dependencies: next,
                target,
                ..
            } = candidate;
            drop(target);
            if let Some(dependencies) = &mut dependencies {
                dependencies.extend(next)?;
            } else {
                dependencies = Some(next);
            }
            completed.push(SettledCall {
                call,
                debt,
                room,
                depth,
                occurrences,
                caller,
                invocation,
                caller_values,
                roles,
                captures,
                evaluated,
                completions,
            });
        }
        let Some(mut dependencies) = dependencies else {
            return Ok(None);
        };
        if let Some(target) = self.target {
            // Any unconsumed candidate keeps the shared target borrowed. A
            // different prefix cannot steal or clone its authentication.
            let Ok(target) = Rc::try_unwrap(target) else {
                return Ok(None);
            };
            dependencies.extend(target.dependencies)?;
        }
        Ok(Some(ScopedPrefix {
            source: self.source,
            caller: self.caller,
            range: self.range,
            completed: offset,
            dependencies,
            calls: completed,
        }))
    }
}

impl<'plan> ScopedPrefix<'plan> {
    pub(in crate::compile::room) fn source(&self) -> &Arc<Source> {
        &self.source
    }
    pub(in crate::compile::room) fn caller(&self) -> &logical::Invocation {
        &self.caller
    }
    pub(in crate::compile::room) fn current(&self, ledger: &Ledger, arena: &Arena) -> bool {
        self.range.is_current(ledger, &self.caller) && self.dependencies.is_current(ledger, arena)
    }
    pub(in crate::compile::room) fn into_parts(self) -> ScopedPrefixParts<'plan> {
        let Self {
            range,
            completed,
            dependencies,
            calls,
            ..
        } = self;
        ScopedPrefixParts {
            range,
            completed,
            dependencies,
            calls,
        }
    }
    pub(crate) fn completed_invocations(&self) -> u64 {
        self.completed
    }
    pub(crate) fn calls(&self) -> &[SettledCall<'_>] {
        &self.calls
    }
}

impl SettledCall<'_> {
    pub(crate) fn completions(&self) -> &[ScopedCompletion] {
        &self.completions
    }
    pub(crate) fn result(&self) -> Value {
        self.evaluated.result
    }
    pub(crate) fn invocation(&self) -> &logical::Invocation {
        &self.invocation
    }
    pub(crate) fn roles(&self) -> &[(Slot, Value)] {
        &self.roles
    }
    pub(crate) fn recovery_after(
        &self,
        at: &Point,
        occurrence: Occurrence,
    ) -> Option<[CallRecovery; 2]> {
        call_recovery(
            self.call,
            &self.caller,
            &self.invocation,
            &self.caller_values,
            CalleeValues {
                values: &self.evaluated.values,
                path: self.evaluated.path,
                captures: &self.captures,
                recovery: Some(&self.evaluated.recovery),
            },
            at,
            occurrence,
        )
    }
}

#[cfg(test)]
impl VerifiedCall {
    pub(crate) fn belongs_to_for_law(&self, source: &Arc<Source>, code: CodeId) -> bool {
        self.belongs_to(source, code)
    }
    pub(crate) fn occurrences_for_law(&self) -> &[usize] {
        self.occurrence.as_slice()
    }
    pub(crate) fn publication_plan_for_law(
        self: &Arc<Self>,
        exe: &Executable,
    ) -> Result<Arc<crate::compile::room::publication::boundary::CompiledPublication>, Refused>
    {
        use crate::compile::room::publication::{
            Successor,
            boundary::{BoundaryNeeds, CompiledPublication},
        };
        let exit = if self.at.phase == Phase::Enter {
            ExitKind::Action
        } else {
            ExitKind::Normal
        };
        let completion = self.completion(&self.caller_source, &self.at, exit)?;
        CompiledPublication::compile_scoped(
            exe,
            self.caller,
            self.paths[0].charges.caller().first().clone(),
            completion.resume().clone(),
            Successor::Normal,
            vec![Arc::clone(self)],
            BoundaryNeeds::default(),
        )
    }
}

#[cfg(test)]
impl<'plan> ScopedPrefix<'plan> {
    pub(crate) fn bind_call_for_law(
        self,
        plan: &Arc<crate::compile::room::publication::boundary::CompiledPublication>,
        frontier: &crate::compile::room::publication::Frontier,
        ledger: &Ledger,
        arena: &Arena,
        heap: &PagedHeap,
        proposed: crate::compile::room::publication::Position,
    ) -> Result<
        Option<
            crate::compile::room::publication::boundary::BoundPublication<
                crate::compile::room::publication::boundary::scoped::ScopedCharges<'plan>,
            >,
        >,
        doc::heap::HeapFault,
    > {
        use crate::compile::room::publication::boundary::BoundaryEvidence;
        if self.calls.len() != 1 {
            return Ok(None);
        }
        let completed = &self.calls[0];
        let call = completed.call;
        if frontier.position().at != call.at {
            return Ok(None);
        }
        let inputs = call
            .consumed_roles()
            .map(|slot| self.caller.owner(slot))
            .collect::<Vec<_>>();
        let mut balance = completed.room.balance;
        if let Some(callee) = call
            .callable
            .filter(|slot| matches!(slot, Slot::Operand(_)))
        {
            let Some(value) = completed
                .roles
                .iter()
                .find_map(|(slot, value)| (*slot == callee).then_some(value))
            else {
                return Ok(None);
            };
            balance.retained_delta -= i128::from(arena.logical_local(value)?);
        }
        let result = slot(&self.source, &Owner::Operand(call.at.site.clone()))
            .map_err(|_| doc::heap::HeapFault::Malformed("callback result role".into()))?;
        // The source compiler's production callers supply their checked
        // caller evidence. This law uses its independently seeded call entry.
        let evidence = BoundaryEvidence {
            invocation: self.caller.clone(),
            room: CheckedRoom {
                owner: self.source.owner,
                native: false,
                whole: false,
                native_contract: None,
                balance,
                exit: ExitKind::Normal,
            },
            dependencies: logical::Dependencies::capture(ledger, arena, &[]).unwrap(),
            inputs,
            outputs: vec![(
                self.caller.owner(result),
                logical::PublicationValue::Native(completed.result()),
            )],
            members: vec![],
        };
        plan.bind_scoped(frontier, ledger, arena, heap, evidence, self, proposed)
    }
}
