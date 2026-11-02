//! Pure calls are bound before physical entry. The body is source algebra,
//! not a second frame executor, and every cold role keeps its defining Source.
use super::super::charges::{
    FormulaValues,
    composition::{ComposedCharges, ComposedDebt},
};
use super::super::*;
use super::{BoundScalarRegion, Origin, ScalarRegion};
use crate::compile::{self, CodeId, Executable, LibId, LibRun, NestedCallee, NestedKind, StmtId};
use lash_kernel_doc::TaskId;
use lash_kernel_doc::Value;
use lash_kernel_exec::{Op, OpRange};
use lash_kernel_state::arena::Arena;
use lash_kernel_state::kernel_internals::{
    PagedHeap,
    logical::{self, Holding, Ledger, Slot},
};

pub(in crate::compile::room) struct Resolved {
    pub(in crate::compile::room) code: CodeId,
    library: Option<LibId>,
    at: Point,
    arguments: usize,
    occurrence: Option<usize>,
}

/// Resolution reads the actual call opcode and its admitted target. An equal
/// formula, signature or function name cannot stand in for this identity.
pub(in crate::compile::room) fn resolved(
    exe: &Executable,
    caller: CodeId,
    statement: StmtId,
    pc: u32,
) -> Result<Resolved, Refused> {
    let (stream, stmt, _) = exe.statement(statement);
    if exe.stmt_at(caller, &stmt.site) != Some(statement)
        || pc < stmt.ops.start
        || pc >= stmt.ops.end()
    {
        return Err(Refused::Source);
    }
    let site = stream
        .source_sites
        .get(pc as usize)
        .and_then(Option::as_deref)
        .ok_or(Refused::Source)?;
    let (library, declared, arguments, phase, occurrence) =
        match stream.ops.get(pc as usize).ok_or(Refused::Path)? {
            Op::Nested { act, args } => {
                let entry = stream.nested.get(act.ix()).ok_or(Refused::Source)?;
                if entry.site != *site {
                    return Err(Refused::Source);
                }
                match entry.kind {
                    NestedKind::Body(lib) => {
                        (Some(lib), None, args.len as usize, Phase::Commit, None)
                    }
                    NestedKind::Call(NestedCallee::Declared(code)) => (
                        None,
                        Some(code),
                        args.len as usize,
                        Phase::Enter,
                        entry.guest.then_some(entry.occurrence),
                    ),
                    NestedKind::Call(
                        NestedCallee::Library(lib) | NestedCallee::CheckedHelper(lib),
                    ) => (
                        Some(lib),
                        None,
                        args.len as usize,
                        Phase::Enter,
                        entry.guest.then_some(entry.occurrence),
                    ),
                    _ => return Err(Refused::Source),
                }
            }
            Op::Call { lib, args } | Op::Operator { lib, args } => (
                Some(LibId(lib.0)),
                None,
                args.len as usize,
                Phase::Commit,
                None,
            ),
            _ => return Err(Refused::Source),
        };
    let code = match (library, declared) {
        (Some(lib), None) => {
            let lib = exe.lib(lib);
            let LibRun::Body(code) = lib.run else {
                return Err(Refused::Source);
            };
            if !lib.body_arity(arguments)
                || lib.limit.is_some()
                || lib.shares_params
                || lib.specialization.resource != CallSource::Body(lib.specialization.function)
                || exe.code(code).source_room.owner
                    != doc::constant_pool::PoolOwner::Function(lib.specialization.function)
            {
                return Err(Refused::Source);
            }
            code
        }
        (None, Some(code)) => {
            if !exe.declared.values().any(|candidate| *candidate == code)
                || arguments > exe.code(code).params.len()
            {
                return Err(Refused::Source);
            }
            code
        }
        _ => return Err(Refused::Source),
    };
    let source = &exe.code(caller).source_room;
    if site.unit != source.site.unit || !site.path.starts_with(&source.site.path) {
        return Err(Refused::Source);
    }
    let mut node = doc::Node::Block(&source.body);
    for index in &site.path[source.site.path.len()..] {
        node = *node.children().get(*index as usize).ok_or(Refused::Path)?;
    }
    let (function, count) = match node {
        doc::Node::Expr(doc::Expr::Call { function, args }) => (*function, args.len()),
        doc::Node::Expr(doc::Expr::Act(act)) => {
            let doc::ActKind::Call(doc::Callee::Local(function)) = act.kind else {
                return Err(Refused::Source);
            };
            (function, act.args.len())
        }
        _ => return Err(Refused::Source),
    };
    let expected = exe.declared.get(&function).copied().or_else(|| {
        let lib = exe.library_lib(&function)?;
        match exe.lib(lib).run {
            LibRun::Body(code) => Some(code),
            _ => None,
        }
    });
    if expected != Some(code)
        || count != arguments
        || library.is_some_and(|lib| exe.lib(lib).specialization.function != function)
    {
        return Err(Refused::Source);
    }
    Ok(Resolved {
        code,
        library,
        at: Point {
            site: site.clone(),
            phase,
        },
        arguments,
        occurrence,
    })
}

struct Native {
    at: Point,
    lib: LibId,
    pair: doc::scalar::Pair,
    inputs: Box<[super::ScalarFact]>,
    result: super::ScalarFact,
}

/// The caller's exact continuation roles are compiled separately from the
/// callee's frame/parameter/temporary lifetime algebra.
pub(crate) struct VerifiedCall {
    executable: u64,
    caller: CodeId,
    statement: StmtId,
    pc: u32,
    callee: CodeId,
    caller_source: Arc<Source>,
    source: Arc<Source>,
    at: Point,
    argument_slots: Box<[Slot]>,
    recovery: Box<[Slot]>,
    parameters: Box<[Slot]>,
    region: ScalarRegion,
    natives: Box<[Native]>,
    charges: ComposedCharges,
    occurrence: Option<usize>,
    library: Option<LibId>,
}

/// All values in these cold maps were evaluated once. A handoff can restore
/// earlier caller operands without reading their source expressions again.
pub(crate) struct CallRecovery {
    pub(crate) source: Arc<Source>,
    pub(crate) position: super::super::publication::Position,
    pub(crate) roles: Vec<(Slot, Value)>,
}

pub(crate) struct BoundCall<'a> {
    call: &'a VerifiedCall,
    stamp: logical::Stamp,
    caller_invocation: logical::Invocation,
    reservation: logical::InvocationRange,
    result: Value,
    peak: u64,
    delta: i128,
    caller_values: Vec<(Slot, Value)>,
    values: Vec<Value>,
    caller_formulas: Vec<(Vec<Value>, Value)>,
    body_formulas: Vec<Vec<(Vec<Value>, Value)>>,
}

fn slot(source: &Source, owner: &Owner) -> Result<Slot, Refused> {
    match source.owner_slots()?.root(owner, TaskId::MAIN, 0) {
        Some(logical::Owner::Value { slot, .. }) => Ok(slot),
        _ => Err(Refused::Source),
    }
}

impl VerifiedCall {
    pub(crate) fn compile(
        exe: &Executable,
        caller: CodeId,
        statement: StmtId,
        pc: u32,
    ) -> Result<Self, Refused> {
        let (_, stmt, _) = exe.statement(statement);
        Self::compile_from(exe, caller, statement, pc, stmt.ops.start)
    }

    /// The first unpaid fence belongs to the caller's original stream. The
    /// charge compiler rejects a suffix which drops any unpaid source event.
    pub(crate) fn compile_from(
        exe: &Executable,
        caller: CodeId,
        statement: StmtId,
        pc: u32,
        first_unpaid: u32,
    ) -> Result<Self, Refused> {
        let target = resolved(exe, caller, statement, pc)?;
        let source = Arc::clone(&exe.code(target.code).source_room);
        source.verify()?;
        let body = exe.code(target.code);
        if target.code == caller
            || !body.free.is_empty()
            || !body.captures.is_empty()
            || !body.capture_names.is_empty()
            || body.slots.iter().any(|slot| slot.shared)
            || source
                .bindings
                .iter()
                .any(|binding| binding.shared || binding.borrowed)
        {
            return Err(Refused::Source);
        }
        // Source syntax is the eligibility rule. Unsupported controls and
        // calls retain the canonical source handoff, without executing them.
        let mut pending = vec![doc::Node::Block(&source.body)];
        while let Some(node) = pending.pop() {
            match node {
                doc::Node::Block(_) => {}
                doc::Node::Stmt(
                    doc::Stmt::Let {
                        value: doc::Rhs::Expr(_),
                        ..
                    }
                    | doc::Stmt::Assign {
                        place: doc::Place::Variable(_),
                        value: doc::Rhs::Expr(_),
                        ..
                    }
                    | doc::Stmt::Return { .. }
                    | doc::Stmt::Do { .. },
                ) => {}
                doc::Node::Expr(
                    doc::Expr::Literal(_) | doc::Expr::Variable(_) | doc::Expr::Call { .. },
                ) => {}
                _ => return Err(Refused::Source),
            }
            pending.extend(node.children());
        }
        struct Route<'a> {
            source: &'a Source,
            selected: Vec<Selection>,
        }
        impl evaluate::Events for Route<'_> {
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
                self.selected.push(Selection::Event { at, exit });
                Ok(exit)
            }
            fn choose(&mut self, _: &Site) -> Result<bool, Refused> {
                Err(Refused::Path)
            }
        }
        let mut route = Route {
            source: &source,
            selected: Vec::new(),
        };
        evaluate::walk(&source, &source.site, &mut route, 4096)?;
        let interval = source.compile_interval(&source.site, &route.selected, 4096)?;
        let region = source.compile_scalar(interval, true)?;
        let mut parameters = Vec::new();
        for (index, name) in source.params.iter().enumerate() {
            let parameter = body.params.get(index).ok_or(Refused::Source)?;
            let declared = body
                .slots
                .get(parameter.slot as usize)
                .ok_or(Refused::Source)?;
            if parameter.shared
                || declared.name != *name
                || declared.declared != source.site
                || body.slots_by_at.get(parameter.at as usize) != Some(&parameter.slot)
            {
                return Err(Refused::Source);
            }
            parameters.push(slot(
                &source,
                &Owner::Binding {
                    declared: source.site.clone(),
                    name: name.clone(),
                },
            )?);
        }
        if parameters.len() != body.params.len() {
            return Err(Refused::Source);
        }
        let mut natives = Vec::new();
        for result in &region.results {
            if !matches!(result.origin, Origin::Computed) {
                continue;
            }
            let mut node = doc::Node::Block(&source.body);
            for at in &result.at.site.path[source.site.path.len()..] {
                node = *node.children().get(*at as usize).ok_or(Refused::Path)?;
            }
            let doc::Node::Expr(doc::Expr::Call { function, args }) = node else {
                return Err(Refused::Source);
            };
            let lib = exe.library_lib(function).ok_or(Refused::Source)?;
            let function = exe.lib(lib);
            if !matches!(function.run, LibRun::Native { body: None, .. })
                || function.limit.is_some()
                || function.specialization.resource
                    != CallSource::Native(function.specialization.function)
                || args.len() != 2
                || !function.body_arity(args.len())
            {
                return Err(Refused::Source);
            }
            let pair = compile::numeric_pair(function, true).ok_or(Refused::Source)?;
            let prior = region
                .recovery
                .iter()
                .find(|map| map.after == result.at)
                .and_then(|_| {
                    region
                        .interval
                        .events()
                        .position(|event| event.point == result.at)
                })
                .and_then(|at| at.checked_sub(1))
                .and_then(|at| region.recovery.get(at))
                .ok_or(Refused::Exit)?;
            let inputs = (0..args.len())
                .map(|at| {
                    let operand = slot(&source, &Owner::Operand(result.at.site.child(at as u32)))?;
                    prior
                        .roles
                        .iter()
                        .find(|(candidate, _)| *candidate == operand)
                        .map(|(_, fact)| *fact)
                        .ok_or(Refused::Exit)
                })
                .collect::<Result<Box<[_]>, _>>()?;
            natives.push(Native {
                at: result.at.clone(),
                lib,
                pair,
                inputs,
                result: result.live.fact,
            });
        }
        let caller_source = Arc::clone(&exe.code(caller).source_room);
        let event = caller_source
            .room
            .events
            .get(&target.at)
            .ok_or(Refused::Source)?;
        let argument_slots = event
            .inputs
            .iter()
            .map(|owner| slot(&caller_source, owner))
            .collect::<Result<Box<[_]>, _>>()?;
        if argument_slots.len() != target.arguments {
            return Err(Refused::Source);
        }
        let restore = caller_source.compile_event(
            &Point {
                site: target.at.site.clone(),
                phase: Phase::Restore,
            },
            ExitKind::Normal,
        )?;
        let recovery = restore
            .restored_roles()
            .ok_or(Refused::Source)?
            .iter()
            .map(|at| slot(&caller_source, &Owner::Operand(at.clone())))
            .collect::<Result<Box<[_]>, _>>()?;
        let len = pc
            .checked_add(1)
            .and_then(|end| end.checked_sub(first_unpaid))
            .ok_or(Refused::Path)?;
        let charges = ComposedCharges::compile(
            exe,
            caller,
            statement,
            OpRange {
                start: first_unpaid,
                len,
            },
            target.code,
        )?;
        let call = Self {
            executable: exe.local(),
            caller,
            statement,
            pc,
            callee: target.code,
            caller_source,
            source,
            at: target.at,
            argument_slots,
            recovery,
            parameters: parameters.into(),
            region,
            natives: natives.into(),
            charges,
            occurrence: target.occurrence,
            library: target.library,
        };
        call.verify(exe)?;
        Ok(call)
    }

    pub(crate) fn verify(&self, exe: &Executable) -> Result<(), Refused> {
        let target = resolved(exe, self.caller, self.statement, self.pc)?;
        if self.executable != exe.local()
            || self.callee != target.code
            || self.at != target.at
            || self.occurrence != target.occurrence
            || self.library != target.library
            || !Arc::ptr_eq(&self.source, &exe.code(self.callee).source_room)
            || !Arc::ptr_eq(&self.caller_source, &exe.code(self.caller).source_room)
        {
            return Err(Refused::Source);
        }
        let event = self
            .caller_source
            .room
            .events
            .get(&self.at)
            .ok_or(Refused::Source)?;
        let expected = event
            .inputs
            .iter()
            .map(|owner| slot(&self.caller_source, owner))
            .collect::<Result<Vec<_>, _>>()?;
        let restore = self.caller_source.compile_event(
            &Point {
                site: self.at.site.clone(),
                phase: Phase::Restore,
            },
            ExitKind::Normal,
        )?;
        let recovery = restore
            .restored_roles()
            .ok_or(Refused::Source)?
            .iter()
            .map(|at| slot(&self.caller_source, &Owner::Operand(at.clone())))
            .collect::<Result<Vec<_>, _>>()?;
        if expected.as_slice() != self.argument_slots.as_ref()
            || recovery.as_slice() != self.recovery.as_ref()
        {
            return Err(Refused::Exit);
        }
        self.region.verify()?;
        self.charges.verify(exe)
    }

    pub(crate) fn callee(&self) -> CodeId {
        self.callee
    }
    pub(crate) fn at(&self) -> &Point {
        &self.at
    }
    pub(crate) fn body(&self) -> &ScalarRegion {
        &self.region
    }

    pub(crate) fn bind<'a>(
        &'a self,
        exe: &Executable,
        resolved_code: CodeId,
        ledger: &Ledger,
        arena: &mut Arena,
        heap: &PagedHeap,
        caller: &BoundScalarRegion<'_>,
    ) -> Result<Option<BoundCall<'a>>, doc::heap::HeapFault> {
        if self.executable != exe.local()
            || resolved_code != self.callee
            || !caller.region.calls.is_empty()
            || !Arc::ptr_eq(&self.caller_source, caller.region.source())
            || !ledger.is_current(&caller.stamp)
        {
            return Ok(None);
        }
        let Some(last) = caller.region.recovery.last() else {
            return Ok(None);
        };
        if caller
            .region
            .interval
            .events()
            .next()
            .map(|event| &event.point)
            != Some(self.charges.caller().first())
        {
            return Ok(None);
        }
        if self
            .caller_source
            .next_after_commit(&last.after.site)
            .ok()
            .as_ref()
            != Some(&self.at)
        {
            return Ok(None);
        }
        let lookup = |slot| {
            last.roles
                .iter()
                .find(|(candidate, _)| *candidate == slot)
                .and_then(|(_, fact)| caller.values.get(fact.index()))
                .copied()
        };
        let Some(arguments) = self
            .argument_slots
            .iter()
            .map(|slot| lookup(*slot))
            .collect::<Option<Vec<_>>>()
        else {
            return Ok(None);
        };
        if self.recovery.iter().any(|slot| lookup(*slot).is_none()) {
            return Ok(None);
        }
        let caller_values = last
            .roles
            .iter()
            .filter(|(slot, _)| !self.argument_slots.contains(slot))
            .map(|(slot, fact)| (*slot, caller.values[fact.index()]))
            .collect();
        let Some(mut bound) = self.bind_arguments(
            exe,
            ledger,
            arena,
            heap,
            &caller.invocation,
            arguments,
            caller_values,
        )?
        else {
            return Ok(None);
        };
        // The same prefix owns both the debt and the memory envelope. Caller
        // operands stay live while the callee reaches its transient peak.
        let Some(peak) = caller
            .delta
            .checked_add(i128::from(bound.peak))
            .map(|peak| peak.max(i128::from(caller.peak)))
            .and_then(|peak| u64::try_from(peak).ok())
        else {
            return Ok(None);
        };
        let Some(delta) = caller.delta.checked_add(bound.delta) else {
            return Ok(None);
        };
        bound.peak = peak;
        bound.delta = delta;
        Ok(Some(bound))
    }

    /// A zero-argument call has no preceding operand region to bind. Its
    /// actual entered caller and genuine unpaid call point supply entry.
    #[expect(
        clippy::too_many_arguments,
        reason = "actual executable, source activation and ledger views bind together"
    )]
    pub(crate) fn bind_entry<'a>(
        &'a self,
        exe: &Executable,
        resolved_code: CodeId,
        ledger: &Ledger,
        arena: &mut Arena,
        heap: &PagedHeap,
        invocation: &logical::Invocation,
        activation: &super::super::placement::DenseActivation,
        position: &super::super::publication::Position,
    ) -> Result<Option<BoundCall<'a>>, doc::heap::HeapFault> {
        if self.executable != exe.local()
            || resolved_code != self.callee
            || !self.argument_slots.is_empty()
            || !self.recovery.is_empty()
            || !activation.current(&self.caller_source, ledger, invocation)
            || position.occurrence.invocation != invocation.id()
            || position.unpaid.as_ref() != Some(&self.at)
            || self.charges.caller().first() != &self.at
        {
            return Ok(None);
        }
        self.bind_arguments(exe, ledger, arena, heap, invocation, vec![], vec![])
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one authenticated call entry supplies values and logical context"
    )]
    fn bind_arguments<'a>(
        &'a self,
        exe: &Executable,
        ledger: &Ledger,
        arena: &mut Arena,
        heap: &PagedHeap,
        caller_invocation: &logical::Invocation,
        arguments: Vec<Value>,
        caller_values: Vec<(Slot, Value)>,
    ) -> Result<Option<BoundCall<'a>>, doc::heap::HeapFault> {
        let mut values = vec![Value::Null; self.region.fact_count()];
        let mut prices = vec![0_i128; self.region.prices];
        let price = |value: &Value, arena: &mut Arena| -> Result<i128, doc::heap::HeapFault> {
            Ok(i128::from(
                Holding::from_native(arena, heap.pages(), value)?.bytes(),
            ))
        };
        let mut given = 0_i128;
        let mut padded = 0_i128;
        for (index, parameter) in self.parameters.iter().enumerate() {
            let value = arguments.get(index).copied().unwrap_or(Value::Absent);
            if let Some(lib) = self.library {
                let param = &exe.lib(lib).header.signature.params[index];
                if !(param.optional && matches!(value, Value::Absent)
                    || compile::import_result_fits(&param.ty, value))
                {
                    return Ok(None);
                }
            }
            let Some(entry) = self
                .region
                .entries
                .iter()
                .find(|entry| entry.slot == *parameter)
            else {
                return Ok(None);
            };
            values[entry.live.fact.index()] = value;
            let amount = price(&value, arena)?;
            prices[entry.live.price] = amount;
            if index < arguments.len() {
                given += amount;
            } else {
                padded += amount;
            }
        }
        let mut computed = BTreeMap::new();
        for result in &self.region.results {
            let value = match result.origin {
                Origin::Literal(value) => value,
                Origin::Read(input) => values[input.fact.index()],
                Origin::Computed => {
                    let Some(native) = self
                        .natives
                        .iter()
                        .find(|native| native.result == result.live.fact)
                    else {
                        return Ok(None);
                    };
                    let input: Vec<_> = native
                        .inputs
                        .iter()
                        .map(|fact| values[fact.index()])
                        .collect();
                    let answer = match input.as_slice() {
                        [Value::Int(a), Value::Int(b)] => native
                            .pair
                            .ints
                            .and_then(|op| doc::scalar::ints(op, *a, *b)),
                        [Value::Float(a), Value::Float(b)] => native
                            .pair
                            .floats
                            .and_then(|op| doc::scalar::floats(op, *a, *b)),
                        _ => None,
                    };
                    let Some(answer) = answer else {
                        return Ok(None);
                    };
                    computed.insert(native.at.clone(), (input, answer));
                    answer
                }
            };
            if !matches!(
                value,
                Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Null | Value::Absent
            ) {
                return Ok(None);
            }
            values[result.live.fact.index()] = value;
            prices[result.live.price] = price(&value, arena)?;
        }
        for call in &self.region.calls {
            // Installed immediate scalar kernels have no scratch or graph
            // allocation. The canonical result holding is their exact peak.
            prices[call.price] = prices[call.result.price];
        }
        let Some(peak) = self
            .region
            .peaks
            .iter()
            .try_fold(padded, |peak, polynomial| {
                Some(peak.max(padded.checked_add(polynomial.evaluate(&prices)?)?))
            })
            .and_then(|peak| u64::try_from(peak.max(0)).ok())
        else {
            return Ok(None);
        };
        let Some(delta) = self
            .region
            .delta
            .evaluate(&prices)
            .and_then(|delta| delta.checked_add(padded))
        else {
            return Ok(None);
        };
        let Some((_, result)) = self
            .region
            .survivors
            .first()
            .filter(|_| self.region.survivors.len() == 1)
        else {
            return Ok(None);
        };
        let result = values[result.fact.index()];
        if delta != price(&result, arena)? - given {
            return Ok(None);
        }
        if let Some(lib) = self.library
            && !compile::import_result_fits(&exe.lib(lib).header.signature.result, result)
        {
            return Ok(None);
        }
        let caller_formulas = self
            .charges
            .caller()
            .formulas()
            .map(|(at, _, _)| {
                if *at == self.at {
                    Some((arguments.clone(), result))
                } else {
                    None
                }
            })
            .collect::<Option<Vec<_>>>();
        let Some(caller_formulas) = caller_formulas else {
            return Ok(None);
        };
        let body_formulas = self
            .charges
            .body()
            .iter()
            .map(|program| {
                program
                    .formulas()
                    .map(|(at, _, _)| computed.get(at).cloned())
                    .collect::<Option<Vec<_>>>()
            })
            .collect::<Option<Vec<_>>>();
        let Some(body_formulas) = body_formulas else {
            return Ok(None);
        };
        let logical::Owner::Frame { task, .. } = caller_invocation.frame() else {
            return Ok(None);
        };
        let Some(reservation) = ledger.reserve_invocations(task, 1) else {
            return Ok(None);
        };
        Ok(Some(BoundCall {
            call: self,
            stamp: ledger.stamp(),
            caller_invocation: caller_invocation.clone(),
            reservation,
            result,
            peak,
            delta,
            caller_values,
            values,
            caller_formulas,
            body_formulas,
        }))
    }

    #[cfg(test)]
    pub(crate) fn understate_frame_for_law(&mut self) {
        self.region.peaks = Box::new([]);
    }
    #[cfg(test)]
    pub(crate) fn omit_caller_role_for_law(&mut self) {
        self.recovery = Box::new([]);
    }
    #[cfg(test)]
    pub(crate) fn reorder_closes_for_law(&mut self) {
        self.charges.reorder_for_law();
    }
}

impl BoundCall<'_> {
    pub(crate) fn result(&self) -> Value {
        self.result
    }
    pub(crate) fn peak(&self) -> u64 {
        self.peak
    }
    pub(crate) fn delta(&self) -> i128 {
        self.delta
    }
    pub(crate) fn depth(&self) -> u64 {
        1
    }
    pub(crate) fn occurrences(&self) -> &[usize] {
        self.call.occurrence.as_slice()
    }
    pub(crate) fn current(&self, ledger: &Ledger, caller: &logical::Invocation) -> bool {
        ledger.is_current(&self.stamp) && caller == &self.caller_invocation
    }
    pub(crate) fn debt(&self) -> Option<ComposedDebt<'_>> {
        let caller: Vec<_> = self
            .caller_formulas
            .iter()
            .map(|(arguments, result)| FormulaValues {
                arguments,
                result: *result,
            })
            .collect();
        let body: Vec<Vec<_>> = self
            .body_formulas
            .iter()
            .map(|formulas| {
                formulas
                    .iter()
                    .map(|(arguments, result)| FormulaValues {
                        arguments,
                        result: *result,
                    })
                    .collect()
            })
            .collect();
        self.call.charges.bind(&caller, &body)
    }
    pub(crate) fn recovery_after(
        &self,
        at: &Point,
        caller_occurrence: super::super::publication::Occurrence,
    ) -> Option<[CallRecovery; 2]> {
        use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};
        if caller_occurrence.invocation != self.caller_invocation.id()
            || !matches!(self.caller_invocation.frame(), logical::Owner::Frame { task, .. } if task == caller_occurrence.task)
        {
            return None;
        }
        let invocation = self.reservation.invocation(0)?;
        let index = self
            .call
            .region
            .recovery
            .iter()
            .position(|map| &map.after == at)?;
        let map = &self.call.region.recovery[index];
        let digest = |at: &Point, roles: &[(Slot, Value)]| {
            let mut encoding = TokenWriter::new();
            encoding.seq(3);
            encoding.text("lash.source.composed-call");
            at.site.encode(&mut encoding);
            encoding.seq(roles.len() as u64);
            for (slot, _) in roles {
                slot.encode(&mut encoding);
            }
            logical_digest(&encoding.into_bytes())
        };
        let caller = CallRecovery {
            source: Arc::clone(&self.call.caller_source),
            position: super::super::publication::Position {
                at: self.call.at.clone(),
                unpaid: Some(Point {
                    site: self.call.at.site.clone(),
                    phase: Phase::Restore,
                }),
                recovery: digest(&self.call.at, &self.caller_values),
                occurrence: caller_occurrence.clone(),
            },
            roles: self.caller_values.clone(),
        };
        let roles: Vec<_> = map
            .roles
            .iter()
            .map(|(slot, fact)| (*slot, self.values[fact.index()]))
            .collect();
        let callee = CallRecovery {
            source: Arc::clone(&self.call.source),
            position: super::super::publication::Position {
                at: map.after.clone(),
                unpaid: self
                    .call
                    .region
                    .recovery
                    .get(index + 1)
                    .map(|map| map.after.clone()),
                recovery: digest(&map.after, &roles),
                occurrence: super::super::publication::Occurrence {
                    task: caller_occurrence.task,
                    invocation: invocation.id(),
                    loops: vec![],
                    action: None,
                },
            },
            roles,
        };
        Some([caller, callee])
    }
    /// Only core's final source-authenticated BoundaryTransaction may consume
    /// this reservation. Dropping a bound call does not advance the ledger.
    pub(crate) fn finish(
        self,
        ledger: &Ledger,
        caller: &logical::Invocation,
    ) -> Option<logical::InvocationRange> {
        self.current(ledger, caller).then_some(self.reservation)
    }
}
