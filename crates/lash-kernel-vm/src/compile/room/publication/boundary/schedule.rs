//! Compiler-owned statement order of one boundary's scalar statements.
use super::*;
use crate::compile::room::charges::composition::StatementDebt;
use crate::compile::{CodeId, Executable};
pub(super) mod sealed {
    pub trait Sealed {}
}
pub(crate) trait BoundaryCharges: sealed::Sealed {
    fn units(&self) -> Option<u64>;
    fn record(&self, tape: &crate::observe::ChargeTape);
}
impl sealed::Sealed for Vec<SettledDebt> {}
impl BoundaryCharges for Vec<SettledDebt> {
    fn units(&self) -> Option<u64> {
        self.iter()
            .try_fold(0u64, |sum, debt| sum.checked_add(debt.units()))
    }
    fn record(&self, tape: &crate::observe::ChargeTape) {
        for debt in self {
            debt.record(tape);
        }
    }
}
pub(crate) struct StatementCharges<'plan>(Vec<StatementDebt<'plan>>);
impl sealed::Sealed for StatementCharges<'_> {}
impl BoundaryCharges for StatementCharges<'_> {
    fn units(&self) -> Option<u64> {
        self.0
            .iter()
            .try_fold(0u64, |sum, debt| sum.checked_add(debt.units()?))
    }
    fn record(&self, tape: &crate::observe::ChargeTape) {
        for debt in &self.0 {
            debt.record(tape);
        }
    }
}
pub(in crate::compile::room) enum StatementSite {
    Scalar(Arc<ChargeProgram>),
}
impl StatementSite {
    fn first(&self) -> &Point {
        match self {
            Self::Scalar(p) => p.first(),
        }
    }
    fn through(&self) -> &Point {
        match self {
            Self::Scalar(p) => p.through(),
        }
    }
}
pub(in crate::compile::room) enum StatementWork<'a> {
    Scalar(ChargeDebt<'a>),
}
pub(super) struct StatementSchedule(Box<[StatementSite]>);
impl CompiledPublication {
    #[expect(
        clippy::too_many_arguments,
        reason = "one verified source route and canonical statement schedule"
    )]
    pub(in crate::compile::room) fn compile_statements(
        exe: &Executable,
        code: CodeId,
        first: Point,
        through: Point,
        successor: Successor,
        sites: Vec<StatementSite>,
        needs: BoundaryNeeds,
        branches: &[(Site, bool)],
    ) -> Result<Arc<Self>, Refused> {
        let source = &exe.code(code).source_room;
        if !branches.is_empty()
            || sites.first().map(StatementSite::first) != Some(&first)
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
            match site {
                StatementSite::Scalar(program) => {
                    program.verify(exe, code)?;
                    if !program.belongs_to(source) {
                        return Err(Refused::Source);
                    }
                }
            }
        }
        let mut plan = Self::compile(exe, code, first, through, successor, vec![], needs)?;
        Arc::get_mut(&mut plan).ok_or(Refused::Source)?.statements =
            Some(StatementSchedule(sites.into()));
        Ok(plan)
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "one source proof, current cursor and actual affine prefix"
    )]
    pub(in crate::compile::room) fn bind_statements<'plan>(
        self: &Arc<Self>,
        frontier: &Frontier,
        ledger: &Ledger,
        arena: &Arena,
        heap: &PagedHeap,
        evidence: BoundaryEvidence,
        work: Vec<StatementWork<'plan>>,
        proposed: Position,
    ) -> Result<Option<BoundPublication<StatementCharges<'plan>>>, HeapFault> {
        let Some(schedule) = &self.statements else {
            return Ok(None);
        };
        if work.len() != schedule.0.len() {
            return Ok(None);
        }
        let needs = self.needs.clone();
        let mut debts = Vec::new();
        for (site, work) in schedule.0.iter().zip(work) {
            let debt = match (site, work) {
                (StatementSite::Scalar(program), StatementWork::Scalar(debt))
                    if program.owns_debt(&debt) =>
                {
                    StatementDebt::Scalar(debt)
                }
                _ => return Ok(None),
            };
            debts.push(debt);
        }
        let Some(mut bound) = self.bind_checked(
            frontier,
            ledger,
            arena,
            heap,
            evidence,
            StatementCharges(debts),
            proposed,
        )?
        else {
            return Ok(None);
        };
        bound.needs = needs;
        Ok(Some(bound))
    }
}
impl StatementSchedule {
    #[cfg(test)]
    fn matches(&self, source: &Arc<Source>, debts: &[StatementDebt<'_>]) -> bool {
        self.0.len() == debts.len()
            && self
                .0
                .iter()
                .zip(debts)
                .all(|(site, debt)| match (site, debt) {
                    (StatementSite::Scalar(program), StatementDebt::Scalar(debt)) => {
                        program.owns_debt(debt)
                            && debt.matches(source, site.first(), site.through())
                    }
                })
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
            &[],
        )
        .map_err(|error| HeapFault::Malformed(format!("owning law statement route: {error:?}")))?;
        let debts = programs
            .iter()
            .map(|program| program.bind(&[]).map(StatementWork::Scalar))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| fault("owning law statement debt"))?;
        let schedule = plan.statements.as_ref().unwrap();
        let check_debts: Vec<_> = programs
            .iter()
            .map(|p| StatementDebt::Scalar(p.bind(&[]).unwrap()))
            .collect();
        assert!(schedule.matches(&plan.source, &check_debts));
        assert!(
            !schedule.matches(&plan.source, &check_debts[1..]),
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
