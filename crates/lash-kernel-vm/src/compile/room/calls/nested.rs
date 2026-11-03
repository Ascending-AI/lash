//! Finite source bodies compose verified callees without entering frames.
use super::binding::EvaluatedArguments;
use super::scope::iteration::scalar_balance;
use super::*;
use std::collections::BTreeSet;

pub(super) struct CompileContext {
    active: BTreeSet<CodeId>,
    remaining: usize,
}
impl CompileContext {
    pub(super) fn new(caller: CodeId) -> Self {
        Self {
            active: [caller].into(),
            remaining: 4096,
        }
    }
    pub(super) fn enter(&mut self, code: CodeId) -> Result<(), Refused> {
        self.remaining = self.remaining.checked_sub(1).ok_or(Refused::Path)?;
        if !self.active.insert(code) {
            return Err(Refused::Source);
        }
        Ok(())
    }
    pub(super) fn leave(&mut self, code: CodeId) {
        self.active.remove(&code);
    }
}

pub(super) enum BodyRegion {
    Scalar(ScalarRegion),
    Composed(Box<Body>),
}
impl BodyRegion {
    pub(super) fn scalar(&self) -> Option<&ScalarRegion> {
        if let Self::Scalar(region) = self {
            Some(region)
        } else {
            None
        }
    }
    pub(super) fn source(&self) -> &Arc<Source> {
        match self {
            Self::Scalar(region) => region.source(),
            Self::Composed(body) => &body.source,
        }
    }
    pub(super) fn selections(&self) -> &[Selection] {
        match self {
            Self::Scalar(region) => region.interval.selections(),
            Self::Composed(body) => body.interval.selections(),
        }
    }
    pub(super) fn last_point(&self) -> Option<&Point> {
        match self {
            Self::Scalar(region) => region.interval.events().last().map(|e| &e.point),
            Self::Composed(body) => body.interval.events().last().map(|e| &e.point),
        }
    }
    pub(super) fn exit(&self) -> ExitKind {
        match self {
            Self::Scalar(region) => region.interval.exit(),
            Self::Composed(body) => body.exit,
        }
    }
    pub(super) fn verify(&self) -> Result<(), Refused> {
        match self {
            Self::Scalar(region) => region.verify(),
            Self::Composed(body) => {
                for part in &body.parts {
                    if let Part::Scalar { region, .. } = part {
                        region.verify()?;
                    }
                }
                Ok(())
            }
        }
    }
    pub(super) fn width(&self) -> Option<u64> {
        match self {
            Self::Scalar(_) => Some(1),
            Self::Composed(body) => body
                .calls
                .iter()
                .try_fold(1u64, |n, call| n.checked_add(call.reservation_width()?)),
        }
    }
    #[cfg(test)]
    pub(super) fn understate_peak(&mut self) {
        match self {
            Self::Scalar(region) => region.peaks = Box::new([]),
            Self::Composed(body) => {
                if let Some(Part::Scalar { region, .. }) = body.parts.first_mut() {
                    region.peaks = Box::new([]);
                }
            }
        }
    }
}

pub(super) fn resolve(
    exe: &Executable,
    code: CodeId,
    at: &Point,
    context: &mut CompileContext,
) -> Result<Option<Arc<VerifiedCall>>, Refused> {
    let source = &exe.code(code).source_room;
    let mut site = at.site.clone();
    loop {
        if let Some(statement) = exe.stmt_at(code, &site) {
            let (stream, stmt, _) = exe.statement(statement);
            let ranges = [
                Some(stmt.ops),
                match stmt.form {
                    compile::Form::While { test, .. } => Some(test),
                    _ => None,
                },
            ];
            for range in ranges.into_iter().flatten() {
                for pc in range.start..range.end() {
                    if stream
                        .source_sites
                        .get(pc as usize)
                        .and_then(Option::as_deref)
                        != Some(&at.site)
                    {
                        continue;
                    }
                    if resolved(exe, code, statement, pc).is_ok_and(|target| target.at == *at) {
                        let first = (range.start..pc)
                            .rev()
                            .find(|p| {
                                let op = stream.ops[*p as usize];
                                !op.kind().in_region() && !matches!(op, Op::Region { .. })
                            })
                            .map_or(range.start, |p| p + 1);
                        return VerifiedCall::compile_in(
                            exe, code, statement, pc, first, None, context,
                        )
                        .map(|call| Some(Arc::new(call)));
                    }
                }
            }
            return Ok(None);
        }
        if site.path.len() <= source.site.path.len() {
            return Ok(None);
        }
        site.path.pop();
    }
}

enum Part {
    Scalar {
        region: Box<ScalarRegion>,
        natives: Box<[Native]>,
    },
    Call(usize),
}
pub(super) struct Body {
    source: Arc<Source>,
    interval: Arc<super::super::super::interval::ComposedInterval>,
    exit: ExitKind,
    parts: Vec<Part>,
    pub(super) calls: Vec<Arc<VerifiedCall>>,
}
pub(super) struct Evaluated {
    pub(super) index: usize,
    pub(super) offset: u64,
    pub(super) saved: Vec<(Slot, Value)>,
    pub(super) body: Box<EvaluatedArguments>,
}
impl EvaluatedArguments {
    pub(super) fn completed(&self) -> Option<u64> {
        self.nested
            .iter()
            .try_fold(1u64, |n, child| n.checked_add(child.body.completed()?))
    }
}
impl Body {
    pub(super) fn compile(
        exe: &Executable,
        code: CodeId,
        selected: &[Selection],
        calls: &[Arc<VerifiedCall>],
    ) -> Result<Self, Refused> {
        let source = Arc::clone(&exe.code(code).source_room);
        let (interval, exit) = source.compile_composed_body(exe, code, selected, calls)?;
        let mut parts = Vec::new();
        let mut live: BTreeSet<_> = source
            .params
            .iter()
            .map(|name| {
                slot(
                    &source,
                    &Owner::Binding {
                        declared: source.site.clone(),
                        name: name.clone(),
                    },
                )
            })
            .collect::<Result<_, _>>()?;
        let mut start = 0;
        let mut cursor = 0;
        while cursor < selected.len() {
            let call_index = match &selected[cursor] {
                Selection::Event { at, .. } => calls.iter().position(|call| call.at == *at),
                _ => None,
            };
            if let Some(index) = call_index {
                if start < cursor {
                    parts.push(Self::scalar(
                        exe, &source, &interval, start, cursor, &mut live,
                    )?);
                }
                let call = &calls[index];
                if let Some(atoms) = &call.root_atoms {
                    for atom in atoms {
                        if let ScalarInput::Binding(slot) = atom
                            && !live.contains(slot)
                        {
                            return Err(Refused::Retained);
                        }
                    }
                } else {
                    for slot in &call.argument_slots {
                        if !live.remove(slot) {
                            return Err(Refused::Retained);
                        }
                    }
                }
                if call.recovery.iter().any(|slot| !live.contains(slot)) {
                    return Err(Refused::Retained);
                }
                live.insert(slot(&source, &Owner::Operand(call.at.site.clone()))?);
                parts.push(Part::Call(index));
                let boundary = interval
                    .calls()
                    .iter()
                    .find(|boundary| boundary.entry().0 == &call.at)
                    .ok_or(Refused::Source)?;
                if !matches!(selected.get(cursor + 1), Some(Selection::Event { at, exit: ExitKind::Normal }) if at == boundary.resume())
                {
                    return Err(Refused::Source);
                }
                cursor += 2;
                start = cursor;
            } else {
                cursor += 1;
            }
        }
        if start < selected.len() {
            parts.push(Self::scalar(
                exe,
                &source,
                &interval,
                start,
                selected.len(),
                &mut live,
            )?);
        }
        if live.len() != 1 {
            return Err(Refused::Retained);
        }
        Ok(Self {
            source,
            interval,
            exit,
            parts,
            calls: calls.to_vec(),
        })
    }
    fn scalar(
        exe: &Executable,
        source: &Arc<Source>,
        interval: &super::super::super::interval::ComposedInterval,
        start: usize,
        end: usize,
        live: &mut BTreeSet<Slot>,
    ) -> Result<Part, Refused> {
        let incoming: Vec<_> = live.iter().copied().collect();
        let region = source.compile_scalar_region_from(interval.scalar(start, end)?, &incoming)?;
        for (slot, _) in region.entries() {
            if !live.remove(&slot) {
                return Err(Refused::Retained);
            }
        }
        for (slot, _) in &region.survivors {
            live.insert(*slot);
        }
        let natives = paths::natives(exe, source, &region)?;
        Ok(Part::Scalar {
            region: Box::new(region),
            natives,
        })
    }
}

impl VerifiedCall {
    pub(crate) fn reservation_width(&self) -> Option<u64> {
        self.paths
            .iter()
            .try_fold(1u64, |width, path| Some(width.max(path.region.width()?)))
    }

    pub(super) fn evaluate_nested_path(
        &self,
        exe: &Executable,
        arena: &Arena,
        arguments: &[Value],
        path: &paths::BodyPath,
        index: usize,
    ) -> Result<Option<EvaluatedArguments>, doc::heap::HeapFault> {
        let BodyRegion::Composed(body) = &path.region else {
            return Ok(None);
        };
        let mut live = BTreeMap::new();
        let mut given = 0i128;
        let mut delta = 0i128;
        for (index, slot) in self.parameters.iter().enumerate() {
            let value = arguments.get(index).copied().unwrap_or(Value::Absent);
            if let Some(lib) = self.library {
                let param = &exe.lib(lib).header.signature.params[index];
                if !(param.optional && matches!(value, Value::Absent)
                    || compile::import_result_fits(&param.ty, value))
                {
                    return Ok(None);
                }
            }
            let bytes = i128::from(arena.logical_local(&value)?);
            if index < arguments.len() {
                given += bytes;
            } else {
                delta += bytes;
            }
            live.insert(*slot, value);
        }
        let mut peak = delta;
        let mut depth = 1u64;
        let mut computed = BTreeMap::new();
        let mut recovery = Vec::new();
        let mut nested = Vec::new();
        let mut offset = 1u64;
        for part in &body.parts {
            match part {
                Part::Scalar { region, natives } => {
                    let mut values = vec![Value::Null; region.fact_count()];
                    for (slot, fact) in region.entries() {
                        let Some(value) = live.get(&slot) else {
                            return Ok(None);
                        };
                        values[fact.index()] = *value;
                    }
                    let mut rooms = Vec::new();
                    for result in &region.results {
                        let value = match result.origin {
                            Origin::Literal(value) => value,
                            Origin::Read(input) => values[input.fact.index()],
                            Origin::Computed => {
                                let native = natives
                                    .iter()
                                    .find(|native| native.result == result.live.fact)
                                    .ok_or_else(invalid)?;
                                let Some((input, answer, room)) =
                                    paths::evaluate_native_room(native, exe, arena, &values)?
                                else {
                                    return Ok(None);
                                };
                                let function = exe.lib(native.lib);
                                rooms.push(room);
                                computed.insert(native.at.clone(), (input, answer));
                                depth = depth.max(1 + u64::from(function.operation()));
                                answer
                            }
                        };
                        values[result.live.fact.index()] = value;
                    }
                    let Some(balance) = scalar_balance(
                        region,
                        arena,
                        &super::super::ScalarRegionValues {
                            values: &values,
                            calls: &rooms,
                        },
                    )?
                    else {
                        return Ok(None);
                    };
                    peak = peak.max(delta + i128::from(balance.peak_extra));
                    delta += balance.retained_delta;
                    recovery.extend(region.recovery.iter().map(|map| {
                        (
                            map.after.clone(),
                            map.roles
                                .iter()
                                .map(|(slot, fact)| (*slot, values[fact.index()]))
                                .collect(),
                        )
                    }));
                    for (slot, _) in region.entries() {
                        live.remove(&slot);
                    }
                    for (slot, fact) in &region.survivors {
                        live.insert(*slot, values[fact.fact.index()]);
                    }
                }
                Part::Call(index) => {
                    let call = &body.calls[*index];
                    if let Some(atoms) = &call.root_atoms {
                        let mut bytes = 0i128;
                        for (slot, atom) in call.argument_slots.iter().zip(atoms.iter()) {
                            let value = match atom {
                                ScalarInput::Literal(value) => *value,
                                ScalarInput::Binding(slot) => {
                                    let Some(value) = live.get(slot) else {
                                        return Ok(None);
                                    };
                                    *value
                                }
                            };
                            bytes += i128::from(arena.logical_local(&value)?);
                            live.insert(*slot, value);
                        }
                        peak = peak.max(delta + bytes);
                        delta += bytes;
                    }
                    let Some(input) = call
                        .argument_slots
                        .iter()
                        .map(|slot| live.get(slot).copied())
                        .collect::<Option<Vec<_>>>()
                    else {
                        return Ok(None);
                    };
                    if call.recovery.iter().any(|slot| !live.contains_key(slot)) {
                        return Ok(None);
                    }
                    let saved = live
                        .iter()
                        .filter(|(slot, _)| !call.argument_slots.contains(slot))
                        .map(|(slot, value)| (*slot, *value))
                        .collect();
                    recovery.push((
                        call.at.clone(),
                        live.iter().map(|(slot, value)| (*slot, *value)).collect(),
                    ));
                    let Some(evaluated) = call.evaluate_arguments(exe, arena, input.clone())?
                    else {
                        return Ok(None);
                    };
                    peak = peak.max(delta + i128::from(evaluated.peak));
                    delta += evaluated.delta;
                    depth = depth.max(1 + evaluated.depth);
                    offset = offset
                        .checked_add(evaluated.completed().ok_or_else(invalid)?)
                        .ok_or_else(invalid)?;
                    computed.insert(call.at.clone(), (input, evaluated.result));
                    for slot in &call.argument_slots {
                        live.remove(slot);
                    }
                    live.insert(
                        slot(&body.source, &Owner::Operand(call.at.site.clone()))
                            .map_err(|_| invalid())?,
                        evaluated.result,
                    );
                    let resume = body
                        .interval
                        .calls()
                        .iter()
                        .find(|edge| edge.entry().0 == &call.at)
                        .ok_or_else(invalid)?
                        .resume()
                        .clone();
                    recovery.push((
                        resume,
                        live.iter().map(|(slot, value)| (*slot, *value)).collect(),
                    ));
                    nested.push(Evaluated {
                        index: *index,
                        offset: offset - evaluated.completed().ok_or_else(invalid)?,
                        saved,
                        body: Box::new(evaluated),
                    });
                }
            }
        }
        let Some(result) = live.values().next().copied().filter(|_| live.len() == 1) else {
            return Ok(None);
        };
        if delta != i128::from(arena.logical_local(&result)?) - given {
            return Ok(None);
        }
        if !self.validate_result(exe, arena, arguments, result)? {
            return Ok(None);
        }
        let Some(caller_formulas) = path
            .charges
            .caller()
            .formulas()
            .map(|(at, _, _)| (*at == self.at).then(|| (arguments.to_vec(), result)))
            .collect::<Option<Vec<_>>>()
        else {
            return Ok(None);
        };
        let Some(body_formulas) = path
            .charges
            .body()
            .iter()
            .map(|program| {
                program
                    .formulas()
                    .map(|(at, _, _)| computed.get(at).cloned())
                    .collect::<Option<Vec<_>>>()
            })
            .collect::<Option<Vec<_>>>()
        else {
            return Ok(None);
        };
        Ok(Some(EvaluatedArguments {
            result,
            arguments: arguments.to_vec(),
            recovery,
            nested,
            peak: u64::try_from(peak.max(0)).map_err(|_| invalid())?,
            delta,
            values: Vec::new(),
            path: index,
            caller_formulas,
            body_formulas,
            depth,
        }))
    }
}
fn invalid() -> doc::heap::HeapFault {
    doc::heap::HeapFault::Malformed("nested source call facts".into())
}
