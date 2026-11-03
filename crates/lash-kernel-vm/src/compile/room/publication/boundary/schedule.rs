//! A source compiler pins statement order before evaluated multi-source debt
//! can join detached storage. Borrowed debt keeps its defining program alive.
use super::*;
use crate::compile::room::{charges::composition::StatementDebt, region::calls::VerifiedCall};
use crate::compile::{CodeId, Executable};

mod sealed {
    pub trait Sealed {}
}

/// Only canonical charge products can enter shared boundary admission.
/// Implementations cannot be supplied by runtime collection kernels.
pub(crate) trait BoundaryCharges: sealed::Sealed {
    fn units(&self) -> Option<u64>;
    fn record(&self, tape: &crate::observe::ChargeTape);
}
impl sealed::Sealed for Vec<SettledDebt> {}
impl BoundaryCharges for Vec<SettledDebt> {
    fn units(&self) -> Option<u64> {
        self.iter()
            .try_fold(0_u64, |sum, debt| sum.checked_add(debt.units()))
    }
    fn record(&self, tape: &crate::observe::ChargeTape) {
        for debt in self {
            debt.record(tape);
        }
    }
}

/// Retains the original caller, callee and completion-formula ordering.
/// Its private constructor is issued only after matching the compiled route.
pub(crate) struct StatementCharges<'plan>(Vec<StatementDebt<'plan>>);
impl sealed::Sealed for StatementCharges<'_> {}
impl BoundaryCharges for StatementCharges<'_> {
    fn units(&self) -> Option<u64> {
        self.0
            .iter()
            .try_fold(0_u64, |sum, debt| sum.checked_add(debt.units()?))
    }
    fn record(&self, tape: &crate::observe::ChargeTape) {
        for debt in &self.0 {
            debt.record(tape);
        }
    }
}

/// Source-local caller coordinates. Each entered body retains its own Source
/// through the verified call; caller coordinates never stand in for a callee.
pub(in crate::compile::room) enum StatementSite {
    Scalar(Arc<ChargeProgram>),
    Call {
        first: Point,
        through: Point,
        call: Arc<VerifiedCall>,
    },
}
impl StatementSite {
    fn first(&self) -> &Point {
        match self {
            Self::Scalar(program) => program.first(),
            Self::Call { first, .. } => first,
        }
    }
    fn through(&self) -> &Point {
        match self {
            Self::Scalar(program) => program.through(),
            Self::Call { through, .. } => through,
        }
    }
    fn call(&self) -> Option<&Arc<VerifiedCall>> {
        match self {
            Self::Scalar(_) => None,
            Self::Call { call, .. } => Some(call),
        }
    }
}
pub(super) struct StatementSchedule(Box<[StatementSite]>);

impl CompiledPublication {
    /// The owning compiler must also verify the selected source path, actual
    /// scoped candidates, branch facts, room and exact surviving roots. This
    /// method verifies their canonical debt topology independently.
    pub(in crate::compile::room) fn compile_statements(
        exe: &Executable,
        code: CodeId,
        first: Point,
        through: Point,
        successor: Successor,
        sites: Vec<StatementSite>,
        needs: BoundaryNeeds,
    ) -> Result<Arc<Self>, Refused> {
        let source = &exe.code(code).source_room;
        if sites.first().map(StatementSite::first) != Some(&first)
            || sites.last().map(StatementSite::through) != Some(&through)
            || sites.windows(2).any(|pair| {
                source
                    .exact_successor(pair[0].through(), Successor::Normal)
                    .as_ref()
                    != Ok(pair[1].first())
            })
        {
            return Err(Refused::Source);
        }
        for site in &sites {
            if !source.room.events.contains_key(site.first())
                || !source.room.events.contains_key(site.through())
            {
                return Err(Refused::Source);
            }
            if let StatementSite::Scalar(program) = site {
                program.verify(exe, code)?;
                if !program.belongs_to(source) {
                    return Err(Refused::Source);
                }
            }
            if let Some(call) = site.call() {
                if !call.belongs_to(source, code) {
                    return Err(Refused::Source);
                }
                call.verify(exe)?;
                call.compile_continuation(exe, site.through(), &[])?;
            }
        }
        let mut plan = Self::compile(exe, code, first, through, successor, vec![], needs)?;
        Arc::get_mut(&mut plan).ok_or(Refused::Source)?.statements =
            Some(StatementSchedule(sites.into()));
        Ok(plan)
    }

    /// Consumes the genuine measured schedule. Neither a total nor a tape
    /// reconstructed by a consumer can satisfy this binding point.
    #[expect(
        clippy::too_many_arguments,
        reason = "one source proof and canonical debts"
    )]
    pub(in crate::compile::room) fn bind_statements<'plan>(
        self: &Arc<Self>,
        frontier: &Frontier,
        ledger: &Ledger,
        arena: &Arena,
        heap: &PagedHeap,
        evidence: BoundaryEvidence,
        debts: Vec<StatementDebt<'plan>>,
        proposed: Position,
    ) -> Result<Option<BoundPublication<StatementCharges<'plan>>>, HeapFault> {
        let Some(schedule) = &self.statements else {
            return Ok(None);
        };
        if !schedule.matches(&self.source, &debts) {
            return Ok(None);
        }
        self.bind_checked(
            frontier,
            ledger,
            arena,
            heap,
            evidence,
            StatementCharges(debts),
            proposed,
        )
    }
}

impl StatementSchedule {
    fn matches(&self, source: &Arc<Source>, debts: &[StatementDebt<'_>]) -> bool {
        !(debts.len() != self.0.len()
            || debts.iter().zip(&self.0).any(|(debt, site)| {
                let scalar_identity = match (site, debt) {
                    (StatementSite::Scalar(program), StatementDebt::Scalar(debt)) => {
                        program.owns_debt(debt)
                    }
                    (
                        StatementSite::Call { .. },
                        StatementDebt::Call { .. } | StatementDebt::PrefixCall { .. },
                    ) => true,
                    _ => false,
                };
                !scalar_identity
                    || !debt.matches(
                        source,
                        site.first(),
                        site.through(),
                        site.call().map(|call| call.at()),
                        site.call().map(|call| call.source()),
                    )
            }))
    }
}

#[cfg(test)]
impl BoundPublication {
    /// Reuse the independently checked scalar roots/room in the owning law;
    /// only the canonical debt representation changes before admission.
    #[expect(
        clippy::too_many_arguments,
        reason = "the owning law's checked source proof"
    )]
    pub(crate) fn statements_for_law<'plan>(
        self,
        exe: &Executable,
        code: CodeId,
        frontier: &Frontier,
        ledger: &Ledger,
        arena: &Arena,
        heap: &PagedHeap,
        programs: &'plan [Arc<ChargeProgram>],
    ) -> Result<Option<BoundPublication<StatementCharges<'plan>>>, HeapFault> {
        let sites = programs
            .iter()
            .cloned()
            .map(StatementSite::Scalar)
            .collect();
        let plan = CompiledPublication::compile_statements(
            exe,
            code,
            self.plan.first.clone(),
            self.plan.through.clone(),
            Successor::Normal,
            sites,
            BoundaryNeeds::default(),
        )
        .map_err(|_| fault("owning law statement route"))?;
        let debts = programs
            .iter()
            .map(|program| program.bind(&[]).map(StatementDebt::Scalar))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| fault("owning law statement debt"))?;
        let schedule = plan.statements.as_ref().unwrap();
        assert!(schedule.matches(&plan.source, &debts));
        assert!(
            !schedule.matches(&plan.source, &debts[1..]),
            "missing original close"
        );
        let reordered: Vec<_> = programs
            .iter()
            .rev()
            .map(|program| StatementDebt::Scalar(program.bind(&[]).unwrap()))
            .collect();
        assert!(
            !schedule.matches(&plan.source, &reordered),
            "reordered original closes"
        );
        let foreign = programs[0]
            .recompile_for_boundary_law(exe, code)
            .map_err(|_| fault("owning law foreign charge"))?;
        let substituted = vec![
            StatementDebt::Scalar(foreign.bind(&[]).unwrap()),
            StatementDebt::Scalar(programs[1].bind(&[]).unwrap()),
        ];
        assert!(
            !schedule.matches(&plan.source, &substituted),
            "equal-price foreign program"
        );
        plan.bind_statements(
            frontier,
            ledger,
            arena,
            heap,
            self.evidence,
            debts,
            self.cursor.next,
        )
    }
}
