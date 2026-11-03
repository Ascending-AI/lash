//! Compile a finite collection source suffix once. Every peak term keeps the
//! event at which its value is observed; a later write cannot reuse that fact.
use super::super::*;
use crate::compile::{CodeId, Executable, StmtId};
use charges::ChargeProgram;
use lash_kernel_exec::OpRange;

type Fact = (Point, Measure);
#[derive(Clone, Default, PartialEq, Eq)]
struct Polynomial {
    fixed: i128,
    terms: BTreeMap<Fact, i128>,
}
impl Polynomial {
    fn add(&mut self, at: &Point, amount: &Amount) -> Result<(), Refused> {
        self.fixed = self.fixed.checked_add(amount.fixed).ok_or(Refused::Facts)?;
        for (measure, coefficient) in &amount.terms {
            let key = (at.clone(), measure.clone());
            let value = self
                .terms
                .get(&key)
                .copied()
                .unwrap_or(0)
                .checked_add(*coefficient)
                .ok_or(Refused::Facts)?;
            if value == 0 {
                self.terms.remove(&key);
            } else {
                self.terms.insert(key, value);
            }
        }
        Ok(())
    }
    fn evaluate(&self, values: &BTreeMap<Fact, i128>) -> Result<i128, Refused> {
        self.terms
            .iter()
            .try_fold(self.fixed, |sum, (fact, coefficient)| {
                sum.checked_add(
                    values
                        .get(fact)
                        .ok_or(Refused::Facts)?
                        .checked_mul(*coefficient)
                        .ok_or(Refused::Facts)?,
                )
                .ok_or(Refused::Facts)
            })
    }
    fn dominates(&self, other: &Self) -> bool {
        if self.fixed < other.fixed {
            return false;
        }
        self.terms.keys().chain(other.terms.keys()).all(|key| {
            let left = self.terms.get(key).copied().unwrap_or(0);
            let right = other.terms.get(key).copied().unwrap_or(0);
            if matches!(key.1, Measure::MemberDelta(_) | Measure::CallDelta { .. }) {
                left == right
            } else {
                left >= right
            }
        })
    }
}
struct ChargeSpan {
    statement: StmtId,
    range: OpRange,
    program: ChargeProgram,
}

/// Immutable source authority for one selected finite exit. This authenticates
/// topology and reduced formulas. Actual values, native/call/member facts and
/// dependencies must still bind at the common affine publication boundary.
pub(crate) struct CollectionProgram {
    source: Arc<Source>,
    code: CodeId,
    first: Point,
    through: Point,
    choices: Box<[(Site, bool)]>,
    successors: Box<[(Option<bool>, Point)]>,
    events: Box<[Arc<CompiledInterval>]>,
    charges: Box<[ChargeSpan]>,
    peaks: Box<[Polynomial]>,
    retained: Polynomial,
    scopes: Box<[Point]>,
    allocations: Box<[(Point, Owner)]>,
}

struct Route<'a> {
    source: &'a Arc<Source>,
    first: &'a Point,
    through: &'a Point,
    choices: &'a BTreeMap<Site, bool>,
    selected: Vec<(Site, bool)>,
    events: Vec<Arc<CompiledInterval>>,
    missing: Option<Site>,
    started: bool,
    finished: bool,
}
impl evaluate::Events for Route<'_> {
    fn completed(&mut self, site: &Site) -> Option<ExitKind> {
        (!self.started && !self.first.site.path.starts_with(&site.path)).then_some(ExitKind::Normal)
    }
    fn step(&mut self, site: &Site, phase: Phase) -> Result<ExitKind, Refused> {
        let at = Point {
            site: site.clone(),
            phase,
        };
        if !self.started {
            if at != *self.first {
                return Ok(ExitKind::Normal);
            }
            self.started = true;
        }
        let event = self.source.compile_event(&at, ExitKind::Normal)?;
        self.events.push(event);
        self.finished = at == *self.through;
        Ok(ExitKind::Normal)
    }
    fn choose(&mut self, site: &Site) -> Result<bool, Refused> {
        if !self.started {
            let index = *self
                .first
                .site
                .path
                .get(site.path.len())
                .ok_or(Refused::Path)?;
            return match index {
                1 => Ok(true),
                2 => Ok(false),
                _ => Err(Refused::Path),
            };
        }
        let Some(&taken) = self.choices.get(site) else {
            self.missing = Some(site.clone());
            return Err(Refused::Path);
        };
        self.selected.push((site.clone(), taken));
        Ok(taken)
    }
    fn selected_end(&self) -> bool {
        self.finished
    }
}

impl CollectionProgram {
    /// Enumerate source branches, never family tags or benchmark spellings.
    /// The same source ceilings as CompiledInterval bound this compilation.
    pub(crate) fn compile_paths(
        exe: &Executable,
        code: CodeId,
        first: Point,
        through: Point,
    ) -> Result<Vec<Self>, Refused> {
        let source = &exe.code(code).source_room;
        source.verify()?;
        if first.site.unit != through.site.unit
            || first.site.unit != source.site.unit
            || !first.site.path.starts_with(&source.site.path)
            || !through.site.path.starts_with(&source.site.path)
        {
            return Err(Refused::Source);
        }
        let common = first
            .site
            .path
            .iter()
            .zip(&through.site.path)
            .take_while(|(left, right)| left == right)
            .count();
        let mut context = first.site.clone();
        context.path.truncate(common);
        let mut pending = vec![BTreeMap::new()];
        let mut paths = Vec::new();
        let mut remaining = 128_usize;
        while let Some(choices) = pending.pop() {
            remaining = remaining.checked_sub(1).ok_or(Refused::Path)?;
            let mut route = Route {
                source,
                first: &first,
                through: &through,
                choices: &choices,
                selected: Vec::new(),
                events: Vec::new(),
                missing: None,
                started: false,
                finished: false,
            };
            let result = evaluate::walk_to(source, &context, &mut route, 4096);
            if let Some(site) = route.missing {
                for taken in [false, true] {
                    let mut next = choices.clone();
                    next.insert(site.clone(), taken);
                    pending.push(next);
                }
                continue;
            }
            // An action, error, break or a different source exit is a genuine
            // handoff. It cannot be relabeled as this completed normal prefix.
            if result != Ok(ExitKind::Normal) || !route.started || !route.finished {
                continue;
            }
            paths.push(Self::from_route(
                exe,
                code,
                first.clone(),
                through.clone(),
                route,
            )?);
        }
        if paths.is_empty() {
            return Err(Refused::Path);
        }
        Ok(paths)
    }

    fn from_route(
        exe: &Executable,
        code: CodeId,
        first: Point,
        through: Point,
        route: Route<'_>,
    ) -> Result<Self, Refused> {
        let source = Arc::clone(route.source);
        let mut balance = Polynomial::default();
        let mut peaks: Vec<Polynomial> = vec![Polynomial::default()];
        let mut scopes = Vec::new();
        let mut allocations = Vec::new();
        let mut charges = Vec::new();
        for interval in &route.events {
            let mut events = interval.events();
            let event = events.next().ok_or(Refused::Source)?;
            if events.next().is_some() {
                return Err(Refused::Source);
            }
            let mut peak = balance.clone();
            peak.add(&event.point, &event.peak)?;
            Self::peak(&mut peaks, peak);
            balance.add(&event.point, &event.delta)?;
            Self::peak(&mut peaks, balance.clone());
            if matches!(event.point.phase, Phase::Enter | Phase::Leave) {
                scopes.push(event.point.clone());
            }
            allocations.extend(
                event
                    .entities
                    .iter()
                    .cloned()
                    .map(|owner| (event.point.clone(), owner)),
            );
            // Immutable tuples are not mutable ledger Entity roots. They
            // still have source construction order and interior peak, including
            // the hole test's empty tuple. Actual boundary binding retains its
            // fresh/cached provenance without granting a mutable entity ordinal.
            if matches!(
                interval::construction::ConstructionEvent::compile(&source, &event.point)?,
                Some(interval::construction::ConstructionEvent::Tuple)
            ) {
                allocations.push((
                    event.point.clone(),
                    event.output.clone().ok_or(Refused::Source)?,
                ));
            }
            if matches!(event.point.phase, Phase::Commit | Phase::Test)
                && let Some(statement) = exe.stmt_at(code, &event.point.site)
            {
                let stmt = exe.stmt(statement);
                // If closes its charge at Test. Its final Commit joins the
                // selected scopes but never repays the already-closed guard.
                if event.point.phase == Phase::Commit
                    && matches!(stmt.form, crate::compile::Form::If { .. })
                {
                    continue;
                }
                if charges
                    .iter()
                    .any(|span: &ChargeSpan| span.statement == statement)
                {
                    return Err(Refused::Path);
                }
                charges.push(ChargeSpan {
                    statement,
                    range: stmt.ops,
                    program: ChargeProgram::compile(exe, code, statement, stmt.ops)?,
                });
            }
        }
        let successors = match through.phase {
            Phase::Commit => vec![(None, source.next_after_commit(&through.site)?)],
            Phase::Test => vec![
                (
                    Some(false),
                    source.next_after_if_test(&through.site, false)?,
                ),
                (Some(true), source.next_after_if_test(&through.site, true)?),
            ],
            _ => return Err(Refused::Exit),
        };
        Ok(Self {
            source,
            code,
            first,
            through,
            choices: route.selected.into(),
            successors: successors.into(),
            events: route.events.into(),
            charges: charges.into(),
            peaks: peaks.into(),
            retained: balance,
            scopes: scopes.into(),
            allocations: allocations.into(),
        })
    }
    fn peak(peaks: &mut Vec<Polynomial>, next: Polynomial) {
        if peaks.iter().any(|peak| peak.dominates(&next)) {
            return;
        }
        peaks.retain(|peak| !next.dominates(peak));
        peaks.push(next);
    }

    /// Re-derive against original source and stream identities. Equal totals
    /// do not authenticate a dropped close, source scope, allocation or peak.
    pub(crate) fn verify(&self, exe: &Executable) -> Result<(), Refused> {
        if !Arc::ptr_eq(&self.source, &exe.code(self.code).source_room) {
            return Err(Refused::Source);
        }
        let expected =
            Self::compile_paths(exe, self.code, self.first.clone(), self.through.clone())?
                .into_iter()
                .find(|path| path.choices == self.choices)
                .ok_or(Refused::Path)?;
        if self.events.len() != expected.events.len()
            || self
                .events
                .iter()
                .zip(&expected.events)
                .any(|(a, b)| !Arc::ptr_eq(a, b))
            || self.scopes != expected.scopes
            || self.allocations != expected.allocations
            || self.successors != expected.successors
        {
            return Err(Refused::Source);
        }
        if self.peaks != expected.peaks || self.retained != expected.retained {
            return Err(Refused::Peak);
        }
        if self.charges.len() != expected.charges.len() {
            return Err(Refused::Exit);
        }
        for (actual, expected) in self.charges.iter().zip(&expected.charges) {
            if actual.statement != expected.statement
                || actual.range != expected.range
                || actual.program.first() != expected.program.first()
            {
                return Err(Refused::Exit);
            }
            actual.program.verify(exe, self.code)?;
        }
        Ok(())
    }
    pub(crate) fn matches(&self, exit: &publication::Position, branches: &[(Site, bool)]) -> bool {
        if exit.at != self.through {
            return false;
        }
        let mut actual = Vec::new();
        let mut successor = None;
        for (site, taken) in branches {
            if self.choices.iter().any(|(required, _)| required == site) {
                actual.push((site.clone(), *taken));
            } else if self.through.phase == Phase::Test
                && *site == self.through.site
                && successor.is_none()
            {
                successor = Some(*taken);
            } else {
                return false;
            }
        }
        if actual.as_slice() != self.choices.as_ref() {
            return false;
        }
        self.successors
            .iter()
            .any(|(choice, next)| *choice == successor && Some(next) == exit.unpaid.as_ref())
    }
    pub(crate) fn source(&self) -> &Arc<Source> {
        &self.source
    }
    /// Only the common room publisher may evaluate this certificate. Its
    /// checker authenticates each event's actual roots, members, native/call
    /// contract and Boolean condition before any measurement is accepted.
    pub(in crate::compile::room) fn evaluate_checked(
        &self,
        exit: &publication::Position,
        branches: &[(Site, bool)],
        execution: &mut impl Execution,
        mut check: impl FnMut(
            &Event,
            &interval::PublicationSite,
            &dyn Facts,
        ) -> Result<Option<bool>, Refused>,
    ) -> Result<CheckedRoom, Refused> {
        if !self.matches(exit, branches) {
            return Err(Refused::Path);
        }
        let mut values = BTreeMap::new();
        for interval in &self.events {
            interval.evaluate_checked(execution, |event, site, facts| {
                let condition = check(event, site, facts)?;
                if event.point.phase == Phase::Test
                    && let Some((_, expected)) =
                        branches.iter().find(|(at, _)| at == &event.point.site)
                    && condition != Some(*expected)
                {
                    return Err(Refused::Facts);
                }
                for measure in event.peak.measures().chain(event.delta.measures()) {
                    let fact = (event.point.clone(), measure.clone());
                    if let std::collections::btree_map::Entry::Vacant(entry) = values.entry(fact) {
                        entry.insert(Amount::measure(measure, facts).ok_or(Refused::Facts)?);
                    }
                }
                Ok(())
            })?;
        }
        let peak = self.peaks.iter().try_fold(0_i128, |peak, expression| {
            Ok::<_, Refused>(peak.max(expression.evaluate(&values)?))
        })?;
        Ok(CheckedRoom {
            owner: self.source.owner,
            native: false,
            whole: false,
            native_contract: None,
            balance: Balance {
                peak_extra: u64::try_from(peak).map_err(|_| Refused::Peak)?,
                retained_delta: self.retained.evaluate(&values)?,
            },
            exit: ExitKind::Normal,
        })
    }

    pub(crate) fn first(&self) -> &Point {
        &self.first
    }
    pub(crate) fn through(&self) -> &Point {
        &self.through
    }
    pub(crate) fn choices(&self) -> &[(Site, bool)] {
        &self.choices
    }
    pub(crate) fn events(&self) -> &[Arc<CompiledInterval>] {
        &self.events
    }
    pub(crate) fn charges(&self) -> impl Iterator<Item = &ChargeProgram> {
        self.charges.iter().map(|span| &span.program)
    }
    pub(crate) fn scopes(&self) -> &[Point] {
        &self.scopes
    }
    pub(crate) fn allocations(&self) -> &[(Point, Owner)] {
        &self.allocations
    }

    #[cfg(test)]
    pub(crate) fn replace_peak_with_final_for_law(&mut self) {
        self.peaks = vec![self.retained.clone()].into();
    }
    #[cfg(test)]
    pub(crate) fn omit_scope_for_law(&mut self) {
        self.scopes = Box::default();
    }
    #[cfg(test)]
    pub(crate) fn omit_allocation_for_law(&mut self) {
        self.allocations = Box::default();
    }
    #[cfg(test)]
    pub(crate) fn corrupt_charge_for_law(&mut self) {
        if self.charges.len() > 1 {
            self.charges.swap(0, 1)
        } else {
            self.charges = Box::default();
        }
    }
}
