//! Caller compilers join genuine evaluated scopes to the shared resource owner.
use super::*;
use crate::compile::room::{
    charges::composition::ComposedDebt,
    region::calls::{
        VerifiedCall,
        scope::{ScopedPrefix, ScopedPrefixParts, SettledCall},
    },
};
use crate::compile::{CodeId, Executable};

pub(super) enum ScopedSchedule {
    Once(Box<[Arc<VerifiedCall>]>),
    Repeated(Box<[Arc<VerifiedCall>]>),
}
pub(crate) struct ScopedCharges<'plan>(Vec<ComposedDebt<'plan>>);
impl schedule::sealed::Sealed for ScopedCharges<'_> {}
impl BoundaryCharges for ScopedCharges<'_> {
    fn units(&self) -> Option<u64> {
        self.0
            .iter()
            .try_fold(0_u64, |sum, debt| sum.checked_add(debt.units()))
    }
    fn record(&self, tape: &crate::observe::ChargeTape) {
        for debt in &self.0 {
            debt.record(tape);
        }
    }
}

impl CompiledPublication {
    /// The enclosing source compiler verifies its caller branches, operands,
    /// root transfers and room. This pins its original ordered call programs.
    /// `needs` describes caller work only; selected callees add their own facts.
    pub(in crate::compile::room) fn compile_scoped(
        exe: &Executable,
        code: CodeId,
        first: Point,
        through: Point,
        successor: Successor,
        calls: Vec<Arc<VerifiedCall>>,
        needs: BoundaryNeeds,
    ) -> Result<Arc<Self>, Refused> {
        let source = &exe.code(code).source_room;
        if calls.is_empty() || needs.invocations != 0 {
            return Err(Refused::Source);
        }
        for call in &calls {
            call.verify(exe)?;
            if !call.belongs_to(source, code) {
                return Err(Refused::Source);
            }
        }
        let mut plan = Self::compile(exe, code, first, through, successor, vec![], needs)?;
        Arc::get_mut(&mut plan).ok_or(Refused::Source)?.callbacks =
            Some(ScopedSchedule::Once(calls.into()));
        Ok(plan)
    }

    /// Repetition is authorized by the original composed source interval,
    /// never a runtime callback count. The caller compiler still authenticates
    /// its actual loop exit and all intervening caller work in its evidence.
    pub(in crate::compile::room) fn compile_scoped_repeated(
        exe: &Executable,
        code: CodeId,
        interval: &Arc<crate::compile::room::interval::ComposedInterval>,
        calls: Vec<Arc<VerifiedCall>>,
        needs: BoundaryNeeds,
    ) -> Result<Arc<Self>, Refused> {
        if !interval.matches(exe, code, &calls) {
            return Err(Refused::Source);
        }
        let first = interval
            .events()
            .next()
            .ok_or(Refused::Source)?
            .point
            .clone();
        let through = interval
            .events()
            .last()
            .ok_or(Refused::Source)?
            .point
            .clone();
        let mut plan = Self::compile_scoped(
            exe,
            code,
            first,
            through,
            Successor::Normal,
            calls.clone(),
            needs,
        )?;
        Arc::get_mut(&mut plan).ok_or(Refused::Source)?.callbacks =
            Some(ScopedSchedule::Repeated(calls.into()));
        Ok(plan)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one caller proof and affine scope"
    )]
    pub(in crate::compile::room) fn bind_scoped<'plan>(
        self: &Arc<Self>,
        frontier: &Frontier,
        ledger: &Ledger,
        arena: &Arena,
        heap: &PagedHeap,
        mut evidence: BoundaryEvidence,
        prefix: ScopedPrefix<'plan>,
        proposed: Position,
    ) -> Result<Option<BoundPublication<ScopedCharges<'plan>>>, HeapFault> {
        let Some(schedule) = &self.callbacks else {
            return Ok(None);
        };
        let (calls, repeated) = match schedule {
            ScopedSchedule::Once(calls) => (calls, false),
            ScopedSchedule::Repeated(calls) => (calls, true),
        };
        if !Arc::ptr_eq(prefix.source(), &self.source)
            || prefix.caller() != &evidence.invocation
            || !prefix.current(ledger, arena)
            || prefix.calls().len() < calls.len()
            || (!repeated && prefix.calls().len() != calls.len())
            || !prefix.calls().len().is_multiple_of(calls.len())
            || prefix
                .calls()
                .iter()
                .zip(calls.iter().cycle())
                .any(|(actual, expected)| {
                    !std::ptr::eq(actual.call, &**expected) || !expected.owns_debt(&actual.debt)
                })
        {
            return Ok(None);
        }
        let ScopedPrefixParts {
            range,
            completed,
            dependencies,
            calls,
            ..
        } = prefix.into_parts();
        evidence.dependencies.extend(dependencies)?;
        let mut needs = self.needs.clone();
        needs.invocations = completed;
        let mut debts = Vec::with_capacity(calls.len());
        for SettledCall {
            debt,
            depth,
            occurrences,
            ..
        } in calls
        {
            needs.depth = needs.depth.max(depth);
            needs.occurrences.extend(occurrences);
            debts.push(debt);
        }
        let Some(mut bound) = self.bind_checked(
            frontier,
            ledger,
            arena,
            heap,
            evidence,
            ScopedCharges(debts),
            proposed,
        )?
        else {
            return Ok(None);
        };
        bound.needs = needs;
        Ok(bound.with_invocations(ledger, range, completed))
    }
}
