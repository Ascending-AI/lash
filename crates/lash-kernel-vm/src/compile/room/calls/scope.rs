//! A loop borrows one ledger revision and owns one prospective call range.
//! Evaluating a candidate neither reserves identities nor republishes roots.
use super::super::super::placement::DenseActivation;
use super::binding::EvaluatedArguments;
use super::*;
use std::{cell::Cell, rc::Rc};
#[path = "../region/composition.rs"]
pub(crate) mod iteration;
#[path = "scope/settlement.rs"]
mod settlement;
pub(in crate::compile::room) use settlement::ScopedPrefixParts;
pub(crate) use settlement::{ScopedPrefix, SettledCall};

pub(crate) struct CallScope<'ledger> {
    ledger: &'ledger Ledger,
    executable: u64,
    source: Arc<Source>,
    caller: logical::Invocation,
    range: logical::InvocationRange,
    identity: Rc<Cell<u64>>,
    target: Option<Rc<BoundCallable>>,
}

struct BoundCallable {
    value: Value,
    code: CodeId,
    identity: lash_kernel_value::CallableId,
    captures: Vec<(Slot, doc::arena::ArenaHandle, Value)>,
    dependencies: logical::Dependencies,
}

/// The actual evaluated source call, including its saved caller operands.
/// It is an affine candidate until its containing prefix is admitted.
pub(crate) struct ScopedCall<'call> {
    call: &'call VerifiedCall,
    identity: Rc<Cell<u64>>,
    offset: u64,
    caller: logical::Invocation,
    invocation: logical::Invocation,
    stamp: logical::Stamp,
    dependencies: logical::Dependencies,
    callable: Option<(Slot, Value, lash_kernel_value::CallableId)>,
    target: Option<Rc<BoundCallable>>,
    caller_values: Vec<(Slot, Value)>,
    evaluated: EvaluatedArguments,
    completions: Vec<ScopedCompletion>,
    occurrences: Vec<usize>,
}

pub(crate) struct ScopedCompletion {
    source: Arc<Source>,
    leave: Point,
    invocation: logical::Invocation,
    caller: logical::Invocation,
    offset: u64,
    saved: Vec<(Slot, Value)>,
    occurrence: Option<usize>,
}
impl ScopedCompletion {
    pub(crate) fn source(&self) -> &Arc<Source> {
        &self.source
    }
    pub(crate) fn leave(&self) -> &Point {
        &self.leave
    }
    pub(crate) fn invocation(&self) -> &logical::Invocation {
        &self.invocation
    }
    pub(crate) fn caller(&self) -> &logical::Invocation {
        &self.caller
    }
    pub(crate) fn offset(&self) -> u64 {
        self.offset
    }
    pub(crate) fn saved_roles(&self) -> &[(Slot, Value)] {
        &self.saved
    }
}
fn completed_calls(
    call: &VerifiedCall,
    evaluated: &EvaluatedArguments,
    range: &logical::InvocationRange,
    offset: u64,
    caller: &logical::Invocation,
    saved: &[(Slot, Value)],
    out: &mut Vec<ScopedCompletion>,
) -> Option<()> {
    let invocation = range.invocation(offset)?;
    if let nested::BodyRegion::Composed(body) = &call.paths.get(evaluated.path)?.region {
        for child in &evaluated.nested {
            completed_calls(
                body.calls.get(child.index)?,
                &child.body,
                range,
                offset.checked_add(child.offset)?,
                &invocation,
                &child.saved,
                out,
            )?;
        }
    } else if !evaluated.nested.is_empty() {
        return None;
    }
    out.push(ScopedCompletion {
        source: Arc::clone(&call.source),
        leave: Point {
            site: call.source.site.clone(),
            phase: Phase::Leave,
        },
        invocation,
        caller: caller.clone(),
        offset,
        saved: saved.to_vec(),
        occurrence: call.occurrence,
    });
    Some(())
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
        if self.callable.is_some() {
            return None;
        }
        self.scope_entry(exe, resolved_code, ledger, caller, activation, range)
    }

    /// The actual callable and defining origin authenticate once for this
    /// scope. Iterations only match that identity and changing scalar facts.
    #[expect(
        clippy::too_many_arguments,
        reason = "one source activation, actual callable and affine reservation"
    )]
    pub(crate) fn bind_callback_scope<'ledger>(
        &self,
        exe: &Executable,
        ledger: &'ledger Ledger,
        arena: &Arena,
        caller: &logical::Invocation,
        activation: &DenseActivation<'_>,
        range: logical::InvocationRange,
        callable: Value,
    ) -> Option<CallScope<'ledger>> {
        self.callable?;
        let mut handles = vec![callable.handle()?];
        let actual = match activation.state(self.callable_origin?)? {
            crate::machine::SlotState::Value(value) => value,
            crate::machine::SlotState::Cell(cell) => {
                handles.push(cell);
                arena.cell(cell).ok()?
            }
            crate::machine::SlotState::Empty => return None,
        };
        if !actual.same(&callable) {
            return None;
        }
        let snapshot = self.authenticate_callable(exe, arena, callable)?;
        handles.extend(snapshot.captures.iter().map(|(_, cell, _)| *cell));
        let dependencies = logical::Dependencies::capture(ledger, arena, &handles)?;
        let mut scope = self.scope_entry(exe, self.callee, ledger, caller, activation, range)?;
        scope.target = Some(Rc::new(BoundCallable {
            value: callable,
            code: self.callee,
            identity: snapshot.identity,
            captures: snapshot.captures,
            dependencies,
        }));
        Some(scope)
    }

    fn scope_entry<'ledger>(
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
            identity: Rc::new(Cell::new(0)),
            target: None,
        })
    }

    /// These source slots include the evaluated arguments and every saved
    /// caller role. Physical register order has no authority here.
    pub(crate) fn scope_roles(&self) -> impl Iterator<Item = Slot> + '_ {
        self.argument_slots
            .iter()
            .chain(self.recovery.iter())
            .copied()
            .chain(self.callable)
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
            || offset < scope.identity.get()
            || roles.iter().enumerate().any(|(index, (slot, value))| {
                (self.argument_slots.contains(slot) && !scalar(*value))
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
        let callable = if let Some(slot) = self.callable {
            let Some(value) = lookup(slot) else {
                return Ok(None);
            };
            let Some(target) = scope.target.as_ref().filter(|target| {
                target.code == self.callee
                    && target.value.same(&value)
                    && target.dependencies.is_current(scope.ledger, arena)
            }) else {
                return Ok(None);
            };
            Some((slot, value, target.identity))
        } else {
            None
        };
        let handles: Vec<_> = roles
            .iter()
            .filter_map(|(_, value)| value.handle())
            .collect();
        let Some(dependencies) = logical::Dependencies::capture(scope.ledger, arena, &handles)
        else {
            return Ok(None);
        };
        let Some(arguments) = self
            .argument_slots
            .iter()
            .map(|slot| lookup(*slot))
            .collect::<Option<Vec<_>>>()
        else {
            return Ok(None);
        };
        if self.root_atoms.as_ref().is_some_and(|atoms| atoms.iter().zip(&arguments).any(|(atom, actual)| matches!(atom, ScalarInput::Literal(expected) if !expected.same(actual)))) { return Ok(None); }
        let captures = scope
            .target
            .as_ref()
            .filter(|_| self.callable.is_some())
            .map_or(&[][..], |target| target.captures.as_slice());
        let Some(evaluated) = self.evaluate_arguments(exe, arena, arguments, captures)? else {
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
        let mut completions = Vec::new();
        if completed_calls(
            self,
            &evaluated,
            &scope.range,
            offset,
            &scope.caller,
            &caller_values,
            &mut completions,
        )
        .is_none()
        {
            return Ok(None);
        }
        Ok(Some(ScopedCall {
            call: self,
            identity: Rc::clone(&scope.identity),
            offset,
            caller: scope.caller.clone(),
            invocation,
            stamp: scope.ledger.stamp(),
            dependencies,
            callable,
            target: self.callable.and(scope.target.as_ref().map(Rc::clone)),
            caller_values,
            evaluated,
            occurrences: completions
                .iter()
                .filter_map(|completion| completion.occurrence)
                .collect(),
            completions,
        }))
    }
}

impl CallScope<'_> {
    pub(crate) fn completed(&self) -> u64 {
        self.identity.get()
    }
    /// Call only after the containing iteration/prefix wins admission. A
    /// declined candidate is dropped and leaves this offset available.
    pub(crate) fn complete(&mut self, call: ScopedCall<'_>) -> Result<(), Refused> {
        if !Rc::ptr_eq(&self.identity, &call.identity)
            || call.offset != self.identity.get()
            || call.caller != self.caller
            || self.range.invocation(self.identity.get()).as_ref() != Some(&call.invocation)
        {
            return Err(Refused::Source);
        }
        self.identity.set(
            self.identity
                .get()
                .checked_add(call.completed_invocations())
                .ok_or(Refused::Facts)?,
        );
        Ok(())
    }
    /// Feed this pair to BoundaryTransaction::complete_invocations in the
    /// source-authenticated loop exit. Unused reserved IDs remain unconsumed.
    pub(crate) fn finish(self) -> Option<(logical::InvocationRange, u64)> {
        self.range
            .is_current(self.ledger, &self.caller)
            .then_some((self.range, self.identity.get()))
    }
}

impl<'call> ScopedCall<'call> {
    /// Only this evaluated source call can mint its body envelope. Borrowing
    /// the candidate retains its exact source/function and reserved invocation.
    pub(crate) fn room(&self) -> SourceCallRoom<'_> {
        SourceCallRoom::new(self)
    }
    pub(in crate::compile::room) fn checked_room(&self) -> CheckedRoom {
        CheckedRoom {
            owner: self
                .callable
                .as_ref()
                .map_or(self.call.source.owner, |(_, _, identity)| {
                    doc::constant_pool::PoolOwner::Function(identity.function)
                }),
            native: false,
            whole: true,
            native_contract: None,
            balance: Balance {
                peak_extra: self.evaluated.peak,
                retained_delta: self.evaluated.delta,
            },
            exit: self.call.paths[self.evaluated.path].region.exit(),
        }
    }

    pub(in crate::compile::room) fn authenticates_source(
        &self,
        source: &Arc<Source>,
        at: &Point,
        caller: &logical::Invocation,
        invocation: &logical::Invocation,
    ) -> bool {
        Arc::ptr_eq(&self.call.caller_source, source)
            && self.call.at == *at
            && self.caller == *caller
            && self.invocation == *invocation
    }

    pub(in crate::compile::room) fn current(&self, ledger: &Ledger, arena: &Arena) -> bool {
        ledger.is_current(&self.stamp)
            && self.dependencies.is_current(ledger, arena)
            && self
                .target
                .as_ref()
                .is_none_or(|target| target.dependencies.is_current(ledger, arena))
            && self.offset >= self.identity.get()
    }
    pub(super) fn is_next(&self) -> bool {
        self.offset == self.identity.get()
    }
    pub(super) fn matches_call(&self, call: &VerifiedCall, caller: &logical::Invocation) -> bool {
        std::ptr::eq(self.call, call) && self.caller == *caller
    }
    pub(super) fn same_scope(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.identity, &other.identity)
    }
    pub(super) fn offset(&self) -> u64 {
        self.offset
    }
    pub(super) fn roles(&self) -> impl Iterator<Item = (Slot, Value)> + '_ {
        let arguments = self
            .call
            .argument_slots
            .iter()
            .copied()
            .zip(self.evaluated.arguments.iter().copied());
        arguments
            .chain(self.caller_values.iter().copied())
            .chain(self.callable.iter().map(|(slot, value, _)| (*slot, *value)))
    }
    pub(crate) fn completed_invocations(&self) -> u64 {
        self.completions.len() as u64
    }
    pub(crate) fn next_offset(&self) -> Option<u64> {
        self.offset.checked_add(self.completed_invocations())
    }
    pub(crate) fn completions(&self) -> &[ScopedCompletion] {
        &self.completions
    }
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
        &self.occurrences
    }
    pub(crate) fn debt(&self) -> Option<ComposedDebt<'call>> {
        bind_debt(
            &self.call.paths[self.evaluated.path],
            &self.evaluated.caller_formulas,
            &self.evaluated.body_formulas,
            &self.evaluated.nested,
        )
    }
    pub(crate) fn recovery_after(
        &self,
        at: &Point,
        occurrence: super::super::super::publication::Occurrence,
    ) -> Option<[CallRecovery; 2]> {
        let captures: Vec<_> = self
            .target
            .as_ref()
            .map_or(&[][..], |target| target.captures.as_slice())
            .iter()
            .map(|(slot, cell, _)| (*slot, *cell))
            .collect();
        call_recovery(
            self.call,
            &self.caller,
            &self.invocation,
            &self.caller_values,
            CalleeValues {
                values: &self.evaluated.values,
                path: self.evaluated.path,
                captures: &captures,
                recovery: Some(&self.evaluated.recovery),
            },
            at,
            occurrence,
        )
    }
}

pub(super) fn scalar(value: Value) -> bool {
    matches!(
        value,
        Value::Null
            | Value::Absent
            | Value::Bool(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::Text(_)
    )
}
