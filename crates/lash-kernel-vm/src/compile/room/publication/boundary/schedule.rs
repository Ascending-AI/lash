//! Compiler-owned statement order joins one actual affine call prefix.
use super::*;
use crate::compile::room::{
    charges::composition::StatementDebt,
    region::calls::{
        VerifiedCall,
        scope::{ScopedPrefix, ScopedPrefixParts},
    },
};
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
    Call {
        first: Point,
        through: Point,
        call: Arc<VerifiedCall>,
        continuation: Box<[Selection]>,
        prefix: Option<Arc<ChargeProgram>>,
        suffix: Option<Arc<ChargeProgram>>,
    },
}
impl StatementSite {
    fn first(&self) -> &Point {
        match self {
            Self::Scalar(p) => p.first(),
            Self::Call { first, .. } => first,
        }
    }
    fn through(&self) -> &Point {
        match self {
            Self::Scalar(p) => p.through(),
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
pub(in crate::compile::room) enum StatementWork<'a> {
    Scalar(ChargeDebt<'a>),
    Call {
        prefix: Option<ChargeDebt<'a>>,
        suffix: Option<ChargeDebt<'a>>,
    },
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
        let calls: Vec<_> = sites
            .iter()
            .filter_map(StatementSite::call)
            .cloned()
            .collect();
        if calls.is_empty() {
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
        } else {
            let route = super::super::sort::caller::source_route(
                exe,
                code,
                first.clone(),
                through.clone(),
                branches,
                &calls,
            )?;
            let closes: Vec<_> = route
                .events()
                .filter_map(|event| {
                    if !matches!(event.point.phase, Phase::Commit | Phase::Test)
                        && !(event.point == through
                            && event.point.phase == Phase::Restore
                            && calls.iter().any(|call| call.at().site == event.point.site))
                    {
                        return None;
                    }
                    if event.point.phase == Phase::Restore {
                        return Some(&event.point);
                    }
                    if event.point == through
                        && calls.iter().any(|call| {
                            call.root_arguments().is_some() && call.at().site == event.point.site
                        })
                    {
                        return Some(&event.point);
                    }
                    let stmt = exe.stmt(exe.stmt_at(code, &event.point.site)?);
                    if event.point.phase == Phase::Commit
                        && matches!(stmt.form, crate::compile::Form::If { .. })
                    {
                        return None;
                    }
                    Some(&event.point)
                })
                .collect();
            if closes.len() != sites.len()
                || closes
                    .iter()
                    .zip(&sites)
                    .any(|(at, site)| *at != site.through())
            {
                return Err(Refused::Source);
            }
        }
        for site in &sites {
            match site {
                StatementSite::Scalar(program) => {
                    program.verify(exe, code)?;
                    if !program.belongs_to(source) {
                        return Err(Refused::Source);
                    }
                }
                StatementSite::Call {
                    call,
                    through,
                    continuation,
                    prefix,
                    suffix,
                    ..
                } => {
                    call.verify(exe)?;
                    if !call.belongs_to(source, code) {
                        return Err(Refused::Source);
                    }
                    if through.phase == Phase::Restore || call.root_arguments().is_some() {
                        let completion = call.completion(
                            source,
                            call.at(),
                            if call.at().phase == Phase::Enter {
                                ExitKind::Action
                            } else {
                                ExitKind::Normal
                            },
                        )?;
                        if completion.resume() == through {
                            if !continuation.is_empty() {
                                return Err(Refused::Source);
                            }
                        } else if call.root_arguments().is_some() {
                            if through.phase != Phase::Commit
                                || exe.stmt_at(code, &through.site) != Some(call.caller_statement())
                                || !call.at().site.path.starts_with(&through.site.path)
                            {
                                return Err(Refused::Source);
                            }
                            let route = super::super::sort::caller::source_route(
                                exe,
                                code,
                                first.clone(),
                                through.clone(),
                                branches,
                                &calls,
                            )?;
                            let resume = route.selections().iter().position(|selection| matches!(selection, Selection::Event { at, .. } if at == completion.resume())).ok_or(Refused::Source)?;
                            let end = route.selections().iter().position(|selection| matches!(selection, Selection::Event { at, .. } if at == through)).ok_or(Refused::Source)?;
                            if route.selections().get(resume + 1..=end) != Some(&**continuation) {
                                return Err(Refused::Source);
                            }
                            route.scalar(resume + 1, end + 1)?;
                        } else {
                            return Err(Refused::Source);
                        }
                    } else {
                        call.compile_continuation(exe, through, continuation)?;
                    }
                    for program in prefix.iter().chain(suffix.iter()) {
                        program.verify(exe, code)?;
                        if !program.belongs_to(source) {
                            return Err(Refused::Source);
                        }
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
        mut evidence: BoundaryEvidence,
        work: Vec<StatementWork<'plan>>,
        prefix: Option<ScopedPrefix<'plan>>,
        proposed: Position,
    ) -> Result<Option<BoundPublication<StatementCharges<'plan>>>, HeapFault> {
        let Some(schedule) = &self.statements else {
            return Ok(None);
        };
        if work.len() != schedule.0.len() {
            return Ok(None);
        }
        let mut needs = self.needs.clone();
        let (range, mut calls) = if let Some(prefix) = prefix {
            if !Arc::ptr_eq(prefix.source(), &self.source)
                || prefix.caller() != &evidence.invocation
                || !prefix.current(ledger, arena, frontier)
            {
                return Ok(None);
            }
            let ScopedPrefixParts {
                range,
                completed,
                dependencies,
                calls,
            } = prefix.into_parts();
            evidence.dependencies.extend(dependencies)?;
            needs.invocations = completed;
            (Some((range, completed)), calls.into_iter())
        } else {
            (None, Vec::new().into_iter())
        };
        let mut debts = Vec::new();
        for (site, work) in schedule.0.iter().zip(work) {
            let debt = match (site, work) {
                (StatementSite::Scalar(program), StatementWork::Scalar(debt))
                    if program.owns_debt(&debt) =>
                {
                    StatementDebt::Scalar(debt)
                }
                (
                    StatementSite::Call {
                        call,
                        prefix,
                        suffix,
                        ..
                    },
                    StatementWork::Call {
                        prefix: before,
                        suffix: after,
                    },
                ) => {
                    if !matches_optional(prefix, &before) || !matches_optional(suffix, &after) {
                        return Ok(None);
                    }
                    let Some(actual) = calls.next() else {
                        return Ok(None);
                    };
                    if !std::ptr::eq(actual.call, &**call) || !call.owns_debt(&actual.debt) {
                        return Ok(None);
                    }
                    needs.depth = needs.depth.max(actual.depth);
                    needs.occurrences.extend(actual.occurrences);
                    match (before, after) {
                        (Some(prefix), Some(suffix)) => StatementDebt::PrefixCall {
                            prefix,
                            call: actual.debt,
                            suffix,
                        },
                        (None, Some(suffix)) => StatementDebt::Call {
                            call: actual.debt,
                            suffix,
                        },
                        (prefix, None) => StatementDebt::ThroughCall {
                            prefix,
                            call: actual.debt,
                        },
                    }
                }
                _ => return Ok(None),
            };
            debts.push(debt);
        }
        if calls.next().is_some() {
            return Ok(None);
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
        Ok(match range {
            Some((range, completed)) => bound.with_invocations(ledger, range, completed),
            None => Some(bound),
        })
    }
}
fn matches_optional(program: &Option<Arc<ChargeProgram>>, debt: &Option<ChargeDebt<'_>>) -> bool {
    match (program, debt) {
        (None, None) => true,
        (Some(program), Some(debt)) => program.owns_debt(debt),
        _ => false,
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
                    _ => false,
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
            None,
            self.cursor.next,
        )
    }
}
