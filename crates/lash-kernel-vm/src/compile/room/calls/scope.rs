//! A loop borrows one ledger revision and owns one prospective call range.
//! Evaluating a candidate neither reserves identities nor republishes roots.
use super::super::super::placement::DenseActivation;
use super::binding::EvaluatedArguments;
use super::*;
use std::rc::Rc;

pub(crate) struct CallScope<'ledger> {
    ledger: &'ledger Ledger,
    executable: u64,
    source: Arc<Source>,
    caller: logical::Invocation,
    range: logical::InvocationRange,
    completed: u64,
    identity: Rc<()>,
}

/// The actual evaluated source call, including its saved caller operands.
/// It is an affine candidate until its containing prefix is admitted.
pub(crate) struct ScopedCall<'call> {
    call: &'call VerifiedCall,
    identity: Rc<()>,
    offset: u64,
    caller: logical::Invocation,
    invocation: logical::Invocation,
    caller_values: Vec<(Slot, Value)>,
    evaluated: EvaluatedArguments,
}

impl VerifiedCall {
    /// The activation and range authenticate once while the scope's immutable
    /// ledger borrow excludes publications, handoffs and normalization.
    pub(crate) fn bind_scope<'ledger>(
        &self,
        exe: &Executable,
        resolved_code: CodeId,
        ledger: &'ledger Ledger,
        caller: &logical::Invocation,
        activation: &DenseActivation<'_>,
        range: logical::InvocationRange,
    ) -> Option<CallScope<'ledger>> {
        if self.executable != exe.local()
            || self.callee != resolved_code
            || !activation.current(&self.caller_source, ledger, caller)
            || !range.is_current(ledger, caller)
            || range.is_empty()
        {
            return None;
        }
        Some(CallScope {
            ledger,
            executable: exe.local(),
            source: Arc::clone(&self.caller_source),
            caller: caller.clone(),
            range,
            completed: 0,
            identity: Rc::new(()),
        })
    }

    /// These source slots include the evaluated arguments and every saved
    /// caller role. Physical register order has no authority here.
    pub(crate) fn scope_roles(&self) -> impl Iterator<Item = Slot> + '_ {
        self.argument_slots
            .iter()
            .chain(self.recovery.iter())
            .copied()
    }

    /// Bind only changing scalar facts. All source topology was verified at
    /// compilation and the scope owns the actual entry revision and ID range.
    pub(crate) fn evaluate_scoped<'call>(
        &'call self,
        exe: &Executable,
        resolved_code: CodeId,
        scope: &CallScope<'_>,
        offset: u64,
        arena: &Arena,
        roles: &[(Slot, Value)],
    ) -> Result<Option<ScopedCall<'call>>, doc::heap::HeapFault> {
        if self.executable != exe.local()
            || scope.executable != exe.local()
            || self.callee != resolved_code
            || !Arc::ptr_eq(&self.caller_source, &scope.source)
            || offset < scope.completed
            || roles.iter().enumerate().any(|(index, (slot, value))| {
                !scalar(*value)
                    || roles[..index].iter().any(|(prior, _)| prior == slot)
                    || !self.scope_roles().any(|required| required == *slot)
            })
            || self
                .scope_roles()
                .any(|required| !roles.iter().any(|(slot, _)| *slot == required))
        {
            return Ok(None);
        }
        let Some(invocation) = scope.range.invocation(offset) else {
            return Ok(None);
        };
        let lookup = |required| {
            roles
                .iter()
                .find_map(|(slot, value)| (*slot == required).then_some(*value))
        };
        let Some(arguments) = self
            .argument_slots
            .iter()
            .map(|slot| lookup(*slot))
            .collect::<Option<Vec<_>>>()
        else {
            return Ok(None);
        };
        let Some(evaluated) = self.evaluate_arguments(exe, arena, arguments)? else {
            return Ok(None);
        };
        let caller_values = self
            .recovery
            .iter()
            .map(|slot| Some((*slot, lookup(*slot)?)))
            .collect::<Option<Vec<_>>>();
        let Some(caller_values) = caller_values else {
            return Ok(None);
        };
        Ok(Some(ScopedCall {
            call: self,
            identity: Rc::clone(&scope.identity),
            offset,
            caller: scope.caller.clone(),
            invocation,
            caller_values,
            evaluated,
        }))
    }
}

impl CallScope<'_> {
    pub(crate) fn completed(&self) -> u64 {
        self.completed
    }
    /// Call only after the containing iteration/prefix wins admission. A
    /// declined candidate is dropped and leaves this offset available.
    pub(crate) fn complete(&mut self, call: ScopedCall<'_>) -> Result<(), Refused> {
        if !Rc::ptr_eq(&self.identity, &call.identity)
            || call.offset != self.completed
            || call.caller != self.caller
            || self.range.invocation(self.completed).as_ref() != Some(&call.invocation)
        {
            return Err(Refused::Source);
        }
        self.completed = self.completed.checked_add(1).ok_or(Refused::Facts)?;
        Ok(())
    }
    /// Feed this pair to BoundaryTransaction::complete_invocations in the
    /// source-authenticated loop exit. Unused reserved IDs remain unconsumed.
    pub(crate) fn finish(self) -> Option<(logical::InvocationRange, u64)> {
        self.range
            .is_current(self.ledger, &self.caller)
            .then_some((self.range, self.completed))
    }
}

impl<'call> ScopedCall<'call> {
    pub(crate) fn result(&self) -> Value {
        self.evaluated.result
    }
    /// Extra room relative to the caller's already-live operand roles. Its
    /// containing source plan adds the caller prefix and continuation peaks.
    pub(crate) fn peak(&self) -> u64 {
        self.evaluated.peak
    }
    pub(crate) fn delta(&self) -> i128 {
        self.evaluated.delta
    }
    pub(crate) fn depth(&self) -> u64 {
        self.evaluated.depth
    }
    pub(crate) fn invocation(&self) -> &logical::Invocation {
        &self.invocation
    }
    pub(crate) fn occurrences(&self) -> &[usize] {
        self.call.occurrence.as_slice()
    }
    pub(crate) fn debt(&self) -> Option<ComposedDebt<'call>> {
        bind_debt(
            self.call,
            &self.evaluated.caller_formulas,
            &self.evaluated.body_formulas,
        )
    }
    pub(crate) fn recovery_after(
        &self,
        at: &Point,
        occurrence: super::super::super::publication::Occurrence,
    ) -> Option<[CallRecovery; 2]> {
        call_recovery(
            self.call,
            &self.caller,
            &self.invocation,
            &self.caller_values,
            &self.evaluated.values,
            at,
            occurrence,
        )
    }
}

fn scalar(value: Value) -> bool {
    matches!(
        value,
        Value::Null | Value::Absent | Value::Bool(_) | Value::Int(_) | Value::Float(_)
    )
}
