//! A whole iteration composes cached caller spans with actual verified calls.
//! Each source keeps its own path; a call never becomes a native opcode.
use super::super::*;
use super::{ScopedCall, scalar};
use crate::compile::room::charges::{ChargeProgram, FormulaValues, composition::StatementDebt};
use crate::compile::room::region::{ScalarInput, ScalarRegionValues};
use std::collections::BTreeSet;

pub(crate) struct VerifiedIteration {
    executable: u64,
    source: Arc<Source>,
    interval: Arc<CompiledInterval>,
    pieces: Box<[Piece]>,
    entries: Box<[Slot]>,
    vacancies: Box<[Slot]>,
    survivors: Box<[Slot]>,
    charges: Box<[DebtPlan]>,
}
enum Piece {
    Scalar {
        region: Box<ScalarRegion>,
        natives: Box<[Native]>,
    },
    Call {
        call: Arc<VerifiedCall>,
        atoms: Option<Box<[ScalarInput]>>,
    },
}

enum DebtPlan {
    Scalar(Box<ChargeProgram>),
    Call {
        index: usize,
        prefix: Option<Box<ChargeProgram>>,
        suffix: Box<ChargeProgram>,
    },
}

pub(crate) struct IterationValues<'values, 'plan> {
    pub(crate) entry: &'values [(Slot, Value)],
    pub(crate) scalars: &'values [ScalarRegionValues<'values>],
    pub(crate) calls: &'values [&'values ScopedCall<'plan>],
}
struct Topology {
    entries: Box<[Slot]>,
    vacancies: Box<[Slot]>,
    survivors: Box<[Slot]>,
}

/// These effects can only be produced from the cached source spans and the
/// same actual scoped candidates that the containing iteration will accept.
pub(crate) struct IterationEffects<'plan> {
    debts: Vec<StatementDebt<'plan>>,
    balance: Balance,
    survivors: Vec<(Slot, Value)>,
    first_offset: Option<u64>,
    completed: u64,
    depth: u64,
    occurrences: Vec<usize>,
}
impl IterationEffects<'_> {
    pub(crate) fn debts(&self) -> &[StatementDebt<'_>] {
        &self.debts
    }
    pub(crate) fn room(&self) -> Balance {
        self.balance
    }
    pub(crate) fn survivors(&self) -> &[(Slot, Value)] {
        &self.survivors
    }
    pub(crate) fn invocation_offsets(&self) -> Option<(u64, u64)> {
        self.first_offset.map(|first| (first, self.completed))
    }
    pub(crate) fn depth(&self) -> u64 {
        self.depth
    }
    pub(crate) fn occurrences(&self) -> &[usize] {
        &self.occurrences
    }
}

impl VerifiedIteration {
    /// Compile exactly one true while test and its completed body. The next
    /// condition remains unpaid. Caller controls inside that body hand off.
    pub(crate) fn compile_loop(
        exe: &Executable,
        code: CodeId,
        statement: StmtId,
        calls: &[Arc<VerifiedCall>],
    ) -> Result<Self, Refused> {
        let stmt = exe.stmt(statement);
        if exe.stmt_at(code, &stmt.site) != Some(statement)
            || !matches!(stmt.form, compile::Form::While { .. })
        {
            return Err(Refused::Source);
        }
        let source = &exe.code(code).source_room;
        let through = Point {
            site: stmt.site.child(1),
            phase: Phase::Leave,
        };
        struct LoopRoute<'a> {
            source: &'a Source,
            branch: &'a Site,
            through: &'a Point,
            selected: Vec<Selection>,
            stop: bool,
        }
        impl evaluate::Events for LoopRoute<'_> {
            fn step(&mut self, site: &Site, phase: Phase) -> Result<ExitKind, Refused> {
                let at = Point {
                    site: site.clone(),
                    phase,
                };
                let exit = self
                    .source
                    .room
                    .exits
                    .iter()
                    .find(|exit| exit.point == at)
                    .ok_or(Refused::Exit)?
                    .kind;
                self.stop = at == *self.through;
                self.selected.push(Selection::Event { at, exit });
                Ok(exit)
            }
            fn choose(&mut self, site: &Site) -> Result<bool, Refused> {
                if site != self.branch {
                    return Err(Refused::Path);
                }
                self.selected.push(Selection::Branch {
                    at: site.clone(),
                    taken: true,
                });
                Ok(true)
            }
            fn selected_end(&self) -> bool {
                self.stop
            }
        }
        let mut route = LoopRoute {
            source,
            branch: &stmt.site,
            through: &through,
            selected: vec![],
            stop: false,
        };
        evaluate::walk_to(source, &stmt.site, &mut route, 4096)?;
        let first = source.first_event(&stmt.site)?;
        let interval = source.compile_between(&first, &through, &route.selected, 4096)?;
        Self::compile(exe, code, interval, calls)
    }
    pub(crate) fn compile(
        exe: &Executable,
        code: CodeId,
        interval: Arc<CompiledInterval>,
        calls: &[Arc<VerifiedCall>],
    ) -> Result<Self, Refused> {
        let source = Arc::clone(&exe.code(code).source_room);
        if interval.owner() != source.owner
            || interval.defining_source() != &source.site
            || interval.exit() != ExitKind::Normal
            || calls.is_empty()
        {
            return Err(Refused::Source);
        }
        for call in calls {
            call.verify(exe)?;
            if call.caller != code || !Arc::ptr_eq(&call.caller_source, &source) {
                return Err(Refused::Source);
            }
        }
        let route = interval.selections();
        let mut pieces = Vec::new();
        let mut start = 0;
        let mut used = BTreeSet::new();
        for (index, selection) in route.iter().enumerate() {
            let Selection::Event { at, .. } = selection else {
                continue;
            };
            let Some((call_index, call)) =
                calls.iter().enumerate().find(|(_, call)| call.at == *at)
            else {
                continue;
            };
            if !used.insert(call_index) {
                return Err(Refused::Source);
            }
            if start < index {
                pieces.push(scalar_piece(exe, &source, route, start, index)?);
            }
            let atoms = call.root_atoms.clone();
            pieces.push(Piece::Call {
                call: Arc::clone(call),
                atoms,
            });
            start = index + 1;
        }
        if used.len() != calls.len() {
            return Err(Refused::Source);
        }
        if start < route.len() {
            pieces.push(scalar_piece(exe, &source, route, start, route.len())?);
        }
        let Topology {
            entries,
            vacancies,
            survivors,
        } = topology(&source, &pieces)?;
        if entries != survivors {
            return Err(Refused::Retained);
        }
        let charges = charge_plans(exe, code, &source, &interval, &pieces)?;
        Ok(Self {
            charges,
            executable: exe.local(),
            source,
            interval,
            pieces: pieces.into(),
            entries,
            vacancies,
            survivors,
        })
    }
    pub(crate) fn entries(&self) -> &[Slot] {
        &self.entries
    }
    pub(crate) fn vacancies(&self) -> &[Slot] {
        &self.vacancies
    }
    pub(crate) fn source(&self) -> &Arc<Source> {
        &self.source
    }
    pub(crate) fn interval(&self) -> &Arc<CompiledInterval> {
        &self.interval
    }
    pub(crate) fn scalars(&self) -> impl Iterator<Item = &ScalarRegion> {
        self.pieces.iter().filter_map(|piece| match piece {
            Piece::Scalar { region, .. } => Some(region.as_ref()),
            _ => None,
        })
    }
    pub(crate) fn calls(&self) -> impl Iterator<Item = &VerifiedCall> {
        self.pieces.iter().filter_map(|piece| match piece {
            Piece::Call { call, .. } => Some(call.as_ref()),
            _ => None,
        })
    }

    /// The loop's persistent activation supplies entry values once. Each
    /// candidate then verifies cross-span values without a ledger rebind,
    /// source walk, invocation reservation or journal reconstruction.
    pub(crate) fn measure<'plan>(
        &'plan self,
        exe: &Executable,
        arena: &Arena,
        ledger: &Ledger,
        caller: &logical::Invocation,
        facts: IterationValues<'_, 'plan>,
    ) -> Result<Option<IterationEffects<'plan>>, doc::heap::HeapFault> {
        let IterationValues {
            entry,
            scalars,
            calls,
        } = facts;
        if exe.local() != self.executable
            || entry.len() != self.entries.len()
            || entry
                .iter()
                .zip(&self.entries)
                .any(|((actual, value), expected)| actual != expected || !scalar(*value))
            || scalars.len() != self.scalars().count()
            || calls.len() != self.calls().count()
            || calls.first().is_some_and(|call| !call.is_next())
        {
            return Ok(None);
        }
        let mut live: BTreeMap<_, _> = entry.iter().copied().collect();
        let mut delta = 0_i128;
        let mut peak = 0_i128;
        let mut scalar_index = 0;
        let mut call_index = 0;
        let mut depth = 0;
        let mut occurrences = Vec::new();
        let mut formulas = BTreeMap::new();
        for piece in &self.pieces {
            match piece {
                Piece::Scalar { region, natives } => {
                    let facts = &scalars[scalar_index];
                    scalar_index += 1;
                    if facts.values.len() != region.fact_count()
                        || region.entries().any(|(slot, fact)| {
                            live.get(&slot)
                                .is_none_or(|value| !value.same(&facts.values[fact.index()]))
                        })
                        || region.vacant.iter().any(|slot| live.contains_key(slot))
                    {
                        return Ok(None);
                    }
                    for native in natives {
                        let Some((input, answer)) =
                            paths::evaluate_native(native, exe, arena, facts.values)?
                        else {
                            return Ok(None);
                        };
                        if !answer.same(&facts.values[native.result.index()]) {
                            return Ok(None);
                        }
                        depth = depth.max(u64::from(exe.lib(native.lib).operation()));
                        formulas.insert(native.at.clone(), (input, answer));
                    }
                    let Some(balance) = scalar_balance(region, arena, facts)? else {
                        return Ok(None);
                    };
                    peak = peak.max(delta + i128::from(balance.peak_extra));
                    delta += balance.retained_delta;
                    for (slot, _) in region.entries() {
                        live.remove(&slot);
                    }
                    for (slot, fact) in &region.survivors {
                        live.insert(*slot, facts.values[fact.fact.index()]);
                    }
                }
                Piece::Call { call, atoms } => {
                    let candidate = calls[call_index];
                    if !candidate.current(ledger)
                        || !candidate.matches_call(call, caller)
                        || calls.first().is_some_and(|first| {
                            !candidate.same_scope(first)
                                || first.offset().checked_add(call_index as u64)
                                    != Some(candidate.offset())
                        })
                    {
                        return Ok(None);
                    }
                    call_index += 1;
                    let mut atom_bytes = 0_i128;
                    if let Some(atoms) = atoms {
                        for (index, input) in atoms.iter().enumerate() {
                            let value = match input {
                                ScalarInput::Literal(value) => *value,
                                ScalarInput::Binding(slot) => match live.get(slot) {
                                    Some(value) => *value,
                                    None => return Ok(None),
                                },
                            };
                            atom_bytes += i128::from(arena.logical_local(&value)?);
                            live.insert(call.argument_slots[index], value);
                        }
                        peak = peak.max(delta + atom_bytes);
                        delta += atom_bytes;
                    }
                    if candidate.roles().any(|(slot, value)| {
                        live.get(&slot).is_none_or(|actual| !actual.same(&value))
                    }) {
                        return Ok(None);
                    }
                    let envelope = candidate.room();
                    let Some(checked) = envelope.checked(
                        ledger,
                        &self.source,
                        &call.at,
                        caller,
                        candidate.invocation(),
                    ) else {
                        return Ok(None);
                    };
                    let balance = checked.balance();
                    peak = peak.max(delta + i128::from(balance.peak_extra));
                    delta += balance.retained_delta;
                    for slot in &call.argument_slots {
                        live.remove(slot);
                    }
                    live.insert(
                        slot(&self.source, &Owner::Operand(call.at.site.clone()))
                            .map_err(|_| invalid())?,
                        candidate.result(),
                    );
                    depth = depth.max(candidate.depth());
                    occurrences.extend_from_slice(candidate.occurrences());
                }
            }
        }
        if live.keys().copied().ne(self.survivors.iter().copied()) {
            return Ok(None);
        }
        let mut debts = Vec::new();
        for plan in &self.charges {
            let debt = match plan {
                DebtPlan::Scalar(program) => {
                    StatementDebt::Scalar(bind_formulas(program, &formulas).ok_or_else(invalid)?)
                }
                DebtPlan::Call {
                    index,
                    prefix,
                    suffix,
                } => {
                    let call = calls[*index].debt().ok_or_else(invalid)?;
                    let suffix = bind_formulas(suffix, &formulas).ok_or_else(invalid)?;
                    if let Some(prefix) = prefix {
                        StatementDebt::PrefixCall {
                            prefix: bind_formulas(prefix, &formulas).ok_or_else(invalid)?,
                            call,
                            suffix,
                        }
                    } else {
                        StatementDebt::Call { call, suffix }
                    }
                }
            };
            debts.push(debt);
        }
        Ok(Some(IterationEffects {
            debts,
            balance: Balance {
                peak_extra: u64::try_from(peak.max(delta).max(0)).map_err(|_| invalid())?,
                retained_delta: delta,
            },
            survivors: live.into_iter().collect(),
            first_offset: calls.first().map(|call| call.offset()),
            completed: calls.len() as u64,
            depth,
            occurrences,
        }))
    }
}

fn scalar_piece(
    exe: &Executable,
    source: &Arc<Source>,
    route: &[Selection],
    start: usize,
    end: usize,
) -> Result<Piece, Refused> {
    let region = span(source, route, start, end)?;
    let natives = paths::natives(exe, source, &region)?;
    Ok(Piece::Scalar {
        region: Box::new(region),
        natives,
    })
}

fn span(
    source: &Arc<Source>,
    route: &[Selection],
    start: usize,
    end: usize,
) -> Result<ScalarRegion, Refused> {
    let first = route[start..end]
        .iter()
        .find_map(|selection| match selection {
            Selection::Event { at, .. } => Some(at),
            _ => None,
        })
        .ok_or(Refused::Path)?;
    let last = route[..end]
        .iter()
        .rev()
        .find_map(|selection| match selection {
            Selection::Event { at, .. } => Some(at),
            _ => None,
        })
        .ok_or(Refused::Path)?;
    let mut context = first.site.clone();
    let common = first
        .site
        .path
        .iter()
        .zip(&last.site.path)
        .take_while(|(a, b)| a == b)
        .count();
    context.path.truncate(common);
    let opening = source.first_event(&context)?;
    let begin = route[..end]
        .iter()
        .position(|selection| matches!(selection, Selection::Event { at, .. } if *at == opening))
        .ok_or(Refused::Path)?;
    source.compile_scalar_region(source.compile_between(first, last, &route[begin..end], 4096)?)
}

fn topology(source: &Arc<Source>, pieces: &[Piece]) -> Result<Topology, Refused> {
    let mut live = BTreeSet::new();
    let mut entries = BTreeSet::new();
    let mut vacant = BTreeSet::new();
    let mut touched = BTreeSet::new();
    let require = |slot,
                   live: &mut BTreeSet<Slot>,
                   entries: &mut BTreeSet<Slot>,
                   touched: &BTreeSet<Slot>|
     -> Result<(), Refused> {
        if !live.contains(&slot) {
            if touched.contains(&slot) {
                return Err(Refused::Retained);
            }
            entries.insert(slot);
            live.insert(slot);
        }
        Ok(())
    };
    for piece in pieces {
        match piece {
            Piece::Scalar { region, .. } => {
                for (slot, _) in region.entries() {
                    require(slot, &mut live, &mut entries, &touched)?;
                }
                for slot in &region.vacant {
                    if live.contains(slot) {
                        return Err(Refused::Retained);
                    }
                    if !touched.contains(slot) {
                        vacant.insert(*slot);
                    }
                    touched.insert(*slot);
                }
                for (slot, _) in region.entries() {
                    live.remove(&slot);
                    touched.insert(slot);
                }
                for (slot, _) in &region.survivors {
                    live.insert(*slot);
                }
            }
            Piece::Call { call, atoms } => {
                if let Some(atoms) = atoms {
                    for input in atoms {
                        if let ScalarInput::Binding(slot) = input {
                            require(*slot, &mut live, &mut entries, &touched)?;
                        }
                    }
                } else {
                    for slot in &call.argument_slots {
                        require(*slot, &mut live, &mut entries, &touched)?;
                        live.remove(slot);
                        touched.insert(*slot);
                    }
                }
                for slot in &call.recovery {
                    require(*slot, &mut live, &mut entries, &touched)?;
                }
                let output = slot(source, &Owner::Operand(call.at.site.clone()))?;
                if live.contains(&output) {
                    return Err(Refused::Retained);
                }
                if !touched.contains(&output) {
                    vacant.insert(output);
                }
                touched.insert(output);
                live.insert(output);
            }
        }
    }
    Ok(Topology {
        entries: entries.into_iter().collect(),
        vacancies: vacant.into_iter().collect(),
        survivors: live.into_iter().collect(),
    })
}

fn scalar_balance(
    region: &ScalarRegion,
    arena: &Arena,
    facts: &ScalarRegionValues<'_>,
) -> Result<Option<Balance>, doc::heap::HeapFault> {
    if facts.values.len() != region.facts || facts.calls.len() != region.calls.len() || facts.values.iter().any(|value| !scalar(*value))
        || region.guards.iter().any(|(fact, taken)| !matches!(facts.values[fact.index()], Value::Bool(value) if value == *taken)) { return Ok(None); }
    let mut prices = vec![0; region.prices];
    for entry in &region.entries {
        prices[entry.live.price] =
            i128::from(arena.logical_local(&facts.values[entry.live.fact.index()])?);
    }
    for result in &region.results {
        let value = facts.values[result.live.fact.index()];
        let expected = match result.origin {
            Origin::Literal(value) => Some(value),
            Origin::Read(input) => Some(facts.values[input.fact.index()]),
            Origin::Computed => None,
        };
        if expected.is_some_and(|expected| !value.same(&expected)) {
            return Ok(None);
        }
        prices[result.live.price] = i128::from(arena.logical_local(&value)?);
    }
    for (call, room) in region.calls.iter().zip(facts.calls) {
        let Some(balance) = room.for_call(call.callee, None) else {
            return Ok(None);
        };
        if balance.retained_delta != prices[call.result.price] {
            return Ok(None);
        }
        prices[call.price] = i128::from(balance.peak_extra);
    }
    let Some(peak) = region.peaks.iter().try_fold(0_i128, |peak, expression| {
        Some(peak.max(expression.evaluate(&prices)?))
    }) else {
        return Ok(None);
    };
    let Some(delta) = region.delta.evaluate(&prices) else {
        return Ok(None);
    };
    Ok(Some(Balance {
        peak_extra: u64::try_from(peak).map_err(|_| invalid())?,
        retained_delta: delta,
    }))
}
fn invalid() -> doc::heap::HeapFault {
    doc::heap::HeapFault::Malformed("composed scalar iteration source facts".into())
}

fn bind_formulas<'a>(
    program: &'a ChargeProgram,
    facts: &BTreeMap<Point, (Vec<Value>, Value)>,
) -> Option<crate::compile::room::charges::ChargeDebt<'a>> {
    let values = program
        .formulas()
        .map(|(at, _, _)| {
            let (arguments, result) = facts.get(at)?;
            Some(FormulaValues {
                arguments,
                result: *result,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    program.bind(&values)
}

fn charge_plans(
    exe: &Executable,
    code: CodeId,
    source: &Arc<Source>,
    interval: &CompiledInterval,
    pieces: &[Piece],
) -> Result<Box<[DebtPlan]>, Refused> {
    let calls: Vec<_> = pieces
        .iter()
        .filter_map(|piece| match piece {
            Piece::Call { call, .. } => Some(call),
            _ => None,
        })
        .collect();
    let mut plans = Vec::new();
    for event in interval.events() {
        let Some(id) = exe.stmt_at(code, &event.point.site) else {
            continue;
        };
        let stmt = exe.stmt(id);
        if !matches!(event.point.phase, Phase::Commit | Phase::Test) {
            continue;
        }
        let range = match stmt.form {
            compile::Form::While { test, .. } if event.point.phase == Phase::Test => test,
            compile::Form::Plain | compile::Form::Action(_)
                if event.point.phase == Phase::Commit =>
            {
                stmt.ops
            }
            _ => return Err(Refused::Source),
        };
        if let Some(index) = calls.iter().position(|call| call.statement == id) {
            let call = calls[index];
            let first = OpRange {
                start: call.first_unpaid,
                len: call
                    .pc
                    .checked_add(1)
                    .and_then(|end| end.checked_sub(call.first_unpaid))
                    .ok_or(Refused::Path)?,
            };
            let prefix = if first.start == range.start {
                None
            } else {
                Some(ChargeProgram::compile(
                    exe,
                    code,
                    id,
                    OpRange {
                        start: range.start,
                        len: first.start.checked_sub(range.start).ok_or(Refused::Path)?,
                    },
                )?)
            };
            let suffix = ChargeProgram::compile(
                exe,
                code,
                id,
                OpRange {
                    start: call.pc + 1,
                    len: range.end().checked_sub(call.pc + 1).ok_or(Refused::Path)?,
                },
            )?;
            plans.push(DebtPlan::Call {
                index,
                prefix: prefix.map(Box::new),
                suffix: Box::new(suffix),
            });
        } else {
            plans.push(DebtPlan::Scalar(Box::new(ChargeProgram::compile(
                exe, code, id, range,
            )?)));
        }
    }
    if calls.len()
        != plans
            .iter()
            .filter(|plan| matches!(plan, DebtPlan::Call { .. }))
            .count()
        || !Arc::ptr_eq(source, &exe.code(code).source_room)
    {
        return Err(Refused::Source);
    }
    Ok(plans.into())
}
