//! One authenticated activation carries the composed source iteration across
//! backedges. Only its final survivors enter a boundary transaction.
use crate::compile::room::{
    self,
    placement::DenseActivation,
    region::calls::scope::{
        CallScope, ScopedCall,
        iteration::{IterationEffects, VerifiedIteration},
    },
};
use lash_kernel_doc::{Value, heap::HeapFault};
use lash_kernel_state::{
    arena::{Arena, Registration},
    kernel_internals::{
        PagedHeap,
        logical::{self, Invocation, Ledger, RootValue, Slot},
    },
};

pub(crate) struct Recurrence {
    pub(crate) iteration: VerifiedIteration,
}
pub(crate) struct BoundRecurrence<'plan, 'ledger> {
    plan: &'plan Recurrence,
    ledger: &'ledger Ledger,
    invocation: Invocation,
    current: Vec<(Slot, Value)>,
    initial_bytes: u64,
    delta: i128,
    peak: u64,
    iterations: u64,
    identity: std::rc::Rc<()>,
    pub(crate) calls: Option<CallScope<'ledger>>,
}
pub(crate) struct Iteration {
    balance: room::Balance,
    before: i128,
    occurrence: u64,
    identity: std::rc::Rc<()>,
    survivors: Vec<(Slot, Value)>,
    peak: u64,
}
impl Recurrence {
    pub(crate) fn entry_values(
        &self,
        activation: &DenseActivation<'_>,
    ) -> Option<Vec<(Slot, Value)>> {
        self.iteration
            .entries()
            .iter()
            .map(|slot| Some((*slot, activation.value(*slot)?)))
            .collect()
    }
    pub(crate) fn bind<'plan, 'ledger>(
        &'plan self,
        ledger: &'ledger Ledger,
        arena: &Arena,
        heap: &PagedHeap,
        invocation: &Invocation,
        activation: &DenseActivation<'_>,
        calls: Option<CallScope<'ledger>>,
    ) -> Result<Option<BoundRecurrence<'plan, 'ledger>>, HeapFault> {
        if !activation.current(self.iteration.source(), ledger, invocation) {
            return Ok(None);
        }
        let Some(current) = self.entry_values(activation) else {
            return Ok(None);
        };
        for (slot, value) in &current {
            if !matches!(
                value,
                Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Null | Value::Absent
            ) {
                return Ok(None);
            }
            let Some(holding) = ledger.holding(invocation.owner(*slot), heap.pages())? else {
                return Ok(None);
            };
            let mut scalar = true;
            holding.visit_edges(heap.pages(), &mut |_, _| {
                scalar = false;
                Ok(())
            })?;
            if !scalar || holding.bytes() != arena.logical_local(value)? {
                return Ok(None);
            }
        }
        for slot in self.iteration.vacancies() {
            if ledger
                .holding(invocation.owner(*slot), heap.pages())?
                .is_some()
            {
                return Ok(None);
            }
        }
        Ok(Some(BoundRecurrence {
            plan: self,
            ledger,
            invocation: invocation.clone(),
            current,
            initial_bytes: ledger.balance()?.total()?,
            delta: 0,
            peak: 0,
            iterations: 0,
            identity: std::rc::Rc::new(()),
            calls,
        }))
    }
}
impl BoundRecurrence<'_, '_> {
    pub(crate) fn entries(&self) -> &[(Slot, Value)] {
        &self.current
    }
    pub(crate) fn evaluate(
        &self,
        effects: &IterationEffects<'_>,
        memory_bound: u64,
    ) -> Option<Iteration> {
        let balance = effects.room();
        let peak = u64::try_from(
            self.delta
                .checked_add(i128::from(balance.peak_extra))?
                .max(0),
        )
        .ok()?;
        if self.initial_bytes.checked_add(peak)? > memory_bound {
            return None;
        }
        Some(Iteration {
            balance,
            before: self.delta,
            occurrence: self.iterations,
            identity: std::rc::Rc::clone(&self.identity),
            survivors: effects.survivors().to_vec(),
            peak,
        })
    }
    pub(crate) fn advance(
        &mut self,
        iteration: Iteration,
        candidates: Vec<ScopedCall<'_>>,
    ) -> Result<(), HeapFault> {
        if iteration.before != self.delta
            || iteration.occurrence != self.iterations
            || !std::rc::Rc::ptr_eq(&iteration.identity, &self.identity)
        {
            return Err(invalid("scalar loop iteration is stale"));
        }
        for candidate in candidates {
            self.calls
                .as_mut()
                .ok_or_else(|| invalid("loop call has no scope"))?
                .complete(candidate)
                .map_err(|_| invalid("loop call completion is stale"))?;
        }
        self.delta = self
            .delta
            .checked_add(iteration.balance.retained_delta)
            .ok_or_else(|| invalid("scalar loop balance overflow"))?;
        self.peak = self.peak.max(iteration.peak);
        self.current = iteration.survivors;
        self.iterations = self
            .iterations
            .checked_add(1)
            .ok_or_else(|| invalid("scalar loop occurrence overflow"))?;
        Ok(())
    }
    pub(crate) fn iterations(&self) -> u64 {
        self.iterations
    }
    pub(crate) fn prepare<'a>(
        self,
        registration: Registration<'a>,
        heap: &PagedHeap,
        entry: Option<room::publication::boundary::CallEntry<'_>>,
    ) -> Result<Option<logical::BoundaryTransaction<'a>>, HeapFault> {
        if self.iterations == 0 {
            return Ok(None);
        }
        let inputs: Vec<_> = self
            .plan
            .iteration
            .entries()
            .iter()
            .map(|slot| self.invocation.owner(*slot))
            .collect();
        let outputs: Vec<_> = self
            .current
            .iter()
            .map(|(slot, value)| (self.invocation.owner(*slot), RootValue::Value(value)))
            .collect();
        let mut transaction =
            self.ledger
                .prepare_source_boundary(registration, heap, &inputs, &outputs)?;
        if i128::from(transaction.balance()?.total()?) - i128::from(self.initial_bytes)
            != self.delta
        {
            return Ok(None);
        }
        if let Some(calls) = self.calls {
            let (range, completed) = calls
                .finish(entry.ok_or_else(|| invalid("loop lost its source entry"))?)
                .ok_or_else(|| invalid("loop invocation range changed"))?;
            transaction = transaction.complete_invocations(range, completed, heap.pages())?;
        }
        Ok(transaction.require_source_room(self.peak))
    }
}
fn invalid(reason: &str) -> HeapFault {
    HeapFault::Malformed(reason.into())
}
