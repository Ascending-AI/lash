//! Pure calls are bound before physical entry. The body is source algebra,
//! not a second frame executor, and every cold role keeps its defining Source.
#[path = "calls/binding.rs"]
mod binding;
#[path = "calls/nested.rs"]
mod nested;
#[path = "calls/paths.rs"]
mod paths;
#[path = "calls/region_isa.rs"]
pub(crate) mod region_isa;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "loop lane wires the scope call API next")
)]
#[path = "calls/scope.rs"]
pub(crate) mod scope;
#[path = "calls/target.rs"]
pub(in crate::compile::room) mod target;
use super::super::charges::{
    FormulaValues,
    composition::{ComposedCharges, ComposedDebt},
};
use super::super::*;
use super::{BoundScalarRegion, Origin, ScalarInput, ScalarRegion};
use crate::compile::region_isa::Role;
use crate::compile::{self, CodeId, Executable, LibId, LibRun, NestedCallee, NestedKind, StmtId};
use lash_kernel_doc::TaskId;
use lash_kernel_doc::Value;
use lash_kernel_exec::{Op, OpRange};
use lash_kernel_state::arena::Arena;
use lash_kernel_state::kernel_internals::{
    PagedHeap,
    logical::{self, Ledger, Slot},
};

pub(in crate::compile::room) struct Resolved {
    pub(in crate::compile::room) code: CodeId,
    pub(in crate::compile::room) library: Option<LibId>,
    at: Point,
    arguments: usize,
    occurrence: Option<usize>,
    rooted: bool,
    pub(in crate::compile::room) dynamic: bool,
    /// A dialect helper whose arguments must fit its signature at the
    /// call (`function_argument`), as an export's do.
    pub(in crate::compile::room) checked: bool,
}

fn caller_span(stmt: &compile::Stmt, pc: u32) -> Result<OpRange, Refused> {
    let range = match stmt.form {
        compile::Form::While { test, .. } if pc >= test.start && pc < test.end() => test,
        _ => stmt.ops,
    };
    if pc >= range.start && pc < range.end() {
        Ok(range)
    } else {
        Err(Refused::Source)
    }
}

/// Resolution reads the actual call opcode and its admitted target. An equal
/// formula, signature or function name cannot stand in for this identity.
pub(in crate::compile::room) fn resolved(
    exe: &Executable,
    caller: CodeId,
    statement: StmtId,
    pc: u32,
) -> Result<Resolved, Refused> {
    target::resolve(exe, caller, statement, pc, None)
}

fn resolved_static(
    exe: &Executable,
    caller: CodeId,
    statement: StmtId,
    pc: u32,
) -> Result<Resolved, Refused> {
    let (stream, stmt, _) = exe.statement(statement);
    if exe.stmt_at(caller, &stmt.site) != Some(statement) || caller_span(stmt, pc).is_err() {
        return Err(Refused::Source);
    }
    let site = stream
        .source_sites
        .get(pc as usize)
        .and_then(Option::as_deref)
        .ok_or(Refused::Source)?;
    let mut checked = false;
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
                        {
                            checked = matches!(
                                entry.kind,
                                NestedKind::Call(NestedCallee::CheckedHelper(_))
                            );
                            Some(lib)
                        },
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
            Op::Act => {
                let compile::Form::Action(action) = &stmt.form else {
                    return Err(Refused::Source);
                };
                if action.site != *site || pc != stmt.ops.start {
                    return Err(Refused::Source);
                }
                let compile::ActionKind::Call { callee, args } = &action.kind else {
                    return Err(Refused::Source);
                };
                let (library, declared) = match callee {
                    compile::Callee::Declared(code) => (None, Some(*code)),
                    compile::Callee::Library(lib) => (Some(*lib), None),
                    compile::Callee::CheckedHelper(lib) => {
                        checked = true;
                        (Some(*lib), None)
                    }
                    _ => return Err(Refused::Source),
                };
                (
                    library,
                    declared,
                    args.len(),
                    Phase::Enter,
                    action.guest.then_some(action.occurrence),
                )
            }
            _ => return Err(Refused::Source),
        };
    let code = match (library, declared) {
        (Some(lib), None) => {
            let lib = exe.lib(lib);
            let (LibRun::Body(code)
            | LibRun::Native {
                body: Some(code), ..
            }) = lib.run
            else {
                return Err(Refused::Source);
            };
            if !lib.body_arity(arguments)
                || lib.limit.is_some()
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
        doc::Node::Expr(doc::Expr::Act(act)) => (
            *super::act_local_callee(act).ok_or(Refused::Source)?,
            act.args.len(),
        ),
        doc::Node::Action(doc::Action::Call {
            callee: doc::Callee::Local(function),
            args,
        }) => (*function, args.len()),
        // An await calls its policy with the awaited value (`K-TASK-026`).
        doc::Node::Action(doc::Action::Await {
            policy: doc::FunctionTarget::Local(function),
            ..
        }) => (*function, 1),
        _ => return Err(Refused::Source),
    };
    let expected = exe.declared.get(&function).copied().or_else(|| {
        let lib = exe.library_lib(&function)?;
        match exe.lib(lib).run {
            LibRun::Body(code)
            | LibRun::Native {
                body: Some(code), ..
            } => Some(code),
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
        rooted: matches!(stream.ops[pc as usize], Op::Act),
        dynamic: false,
        checked,
    })
}

struct CallerRoles {
    arguments: Box<[Slot]>,
    recovery: Box<[Slot]>,
    /// The callee's role: a statement call's named slot or session cell,
    /// an expression call's operand.
    callable: Option<Role>,
}
/// The caller-side facts of a call site, which never name its target.
#[derive(Clone, Copy)]
pub(in crate::compile::room) struct Shape<'a> {
    at: &'a Point,
    arguments: usize,
    rooted: bool,
    dynamic: bool,
}
impl Resolved {
    fn shape(&self) -> Shape<'_> {
        Shape {
            at: &self.at,
            arguments: self.arguments,
            rooted: self.rooted,
            dynamic: self.dynamic,
        }
    }
}
fn caller_roles(source: &Source, target: Shape<'_>) -> Result<CallerRoles, Refused> {
    let event = source.room.events.get(target.at).ok_or(Refused::Source)?;
    if target.rooted {
        // Root actions evaluate ordered atoms at their own Enter. They have
        // no enclosing expression operands to park across the call.
        if !event.inputs.is_empty() {
            return Err(Refused::Source);
        }
        let arguments = (0..target.arguments)
            .map(|index| {
                slot(
                    source,
                    &Owner::Atom {
                        at: target.at.site.clone(),
                        index: index as u32,
                    },
                )
            })
            .collect::<Result<Box<[_]>, _>>()?;
        return Ok(CallerRoles {
            arguments,
            recovery: Box::new([]),
            callable: if target.dynamic {
                Some(target::root_role(source, target.at)?)
            } else {
                None
            },
        });
    }
    let mut arguments = event
        .inputs
        .iter()
        .map(|owner| slot(source, owner))
        .collect::<Result<Vec<_>, _>>()?;
    let callable = if target.dynamic {
        if arguments.is_empty() {
            return Err(Refused::Source);
        }
        Some(Role::Slot(arguments.remove(0)))
    } else {
        None
    };
    if arguments.len() != target.arguments {
        return Err(Refused::Source);
    }
    let restore = source.compile_event(
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
        .map(|at| slot(source, &Owner::Operand(at.clone())))
        .collect::<Result<Box<[_]>, _>>()?;
    Ok(CallerRoles {
        arguments: arguments.into(),
        recovery,
        callable,
    })
}

struct Native {
    at: Point,
    lib: LibId,
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
    callable: Option<Slot>,
    callable_origin: Option<Slot>,
    recovery: Box<[Slot]>,
    parameters: Box<[Slot]>,
    paths: Box<[paths::BodyPath]>,
    rooted: bool,
    root_atoms: Option<Box<[ScalarInput]>>,
    first_unpaid: u32,
    occurrence: Option<usize>,
    library: Option<LibId>,
}

/// All values in these cold maps were evaluated once. A handoff can restore
/// earlier caller operands without reading their source expressions again.
pub(crate) struct CallRecovery {
    pub(crate) source: Arc<Source>,
    pub(crate) position: super::super::publication::Position,
    pub(crate) roles: Vec<(Slot, Value)>,
    pub(crate) captures: Vec<(Slot, doc::arena::ArenaHandle)>,
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
    path: usize,
    caller_formulas: Vec<(Vec<Value>, Value)>,
    body_formulas: Vec<Vec<(Vec<Value>, Value)>>,
    pub(super) inputs: Vec<Slot>,
    pub(super) live: Vec<(Slot, Value, Option<Slot>)>,
    depth: u64,
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
        Self::compile_from(exe, caller, statement, pc, caller_span(stmt, pc)?.start)
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
        Self::compile_selected(exe, caller, statement, pc, first_unpaid, None)
    }

    /// A dynamic site's alternatives are compiled once. Binding still needs
    /// the actual admitted callable, its defining Link and environment.
    pub(crate) fn compile_callback(
        exe: &Executable,
        caller: CodeId,
        statement: StmtId,
        pc: u32,
        first_unpaid: u32,
        callee: CodeId,
    ) -> Result<Self, Refused> {
        Self::compile_selected(exe, caller, statement, pc, first_unpaid, Some(callee))
    }

    fn compile_selected(
        exe: &Executable,
        caller: CodeId,
        statement: StmtId,
        pc: u32,
        first_unpaid: u32,
        selected: Option<CodeId>,
    ) -> Result<Self, Refused> {
        let mut context = nested::CompileContext::new(caller);
        Self::compile_in(
            exe,
            caller,
            statement,
            pc,
            first_unpaid,
            selected,
            &mut context,
        )
    }

    fn compile_in(
        exe: &Executable,
        caller: CodeId,
        statement: StmtId,
        pc: u32,
        first_unpaid: u32,
        selected: Option<CodeId>,
        context: &mut nested::CompileContext,
    ) -> Result<Self, Refused> {
        let target = target::resolve(exe, caller, statement, pc, selected)?;
        let source = Arc::clone(&exe.code(target.code).source_room);
        source.verify()?;
        let body = exe.code(target.code);
        let captures = target.dynamic && !body.captures.is_empty();
        if target.code == caller
            || target.library.is_some_and(|lib| exe.lib(lib).shares_params)
            || !body.free.is_empty()
            || (!captures && (!body.captures.is_empty() || !body.capture_names.is_empty()))
            || source.bindings.iter().any(|binding| {
                binding.borrowed
                    && !(captures
                        && binding.declared == source.site
                        && body.capture_names.contains(&binding.name))
            })
        {
            return Err(Refused::Source);
        }
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
        let caller_source = Arc::clone(&exe.code(caller).source_room);
        let CallerRoles {
            arguments: argument_slots,
            recovery,
            callable,
        } = caller_roles(&caller_source, target.shape())?;
        // This statement-executor plan reads callees from frame slots only.
        let callable = callable
            .map(|role| role.slot().ok_or(Refused::Source))
            .transpose()?;
        let root_atoms = if target.rooted {
            Some(root_arguments(&caller_source, &target.at)?)
        } else {
            None
        };
        let len = pc
            .checked_add(1)
            .and_then(|end| end.checked_sub(first_unpaid))
            .ok_or(Refused::Path)?;
        context.enter(target.code)?;
        let paths = paths::compile(
            exe,
            caller,
            statement,
            OpRange {
                start: first_unpaid,
                len,
            },
            target.code,
            &parameters,
            context,
        );
        context.leave(target.code);
        let paths = paths?;
        let callable_origin = if target.dynamic {
            Some(target::root_slot(&caller_source, &target.at)?)
        } else {
            None
        };
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
            callable,
            callable_origin,
            recovery,
            parameters: parameters.into(),
            paths,
            rooted: target.rooted,
            root_atoms,
            first_unpaid,
            occurrence: target.occurrence,
            library: target.library,
        };
        call.verify(exe)?;
        Ok(call)
    }

    pub(crate) fn verify(&self, exe: &Executable) -> Result<(), Refused> {
        let target = target::resolve(exe, self.caller, self.statement, self.pc, Some(self.callee))?;
        if self.executable != exe.local()
            || self.callee != target.code
            || self.at != target.at
            || self.occurrence != target.occurrence
            || self.library != target.library
            || self.rooted != target.rooted
            || !Arc::ptr_eq(&self.source, &exe.code(self.callee).source_room)
            || !Arc::ptr_eq(&self.caller_source, &exe.code(self.caller).source_room)
        {
            return Err(Refused::Source);
        }
        let CallerRoles {
            arguments: expected,
            recovery,
            callable,
        } = caller_roles(&self.caller_source, target.shape())?;
        // This statement-executor plan reads callees from frame slots only.
        let callable = callable
            .map(|role| role.slot().ok_or(Refused::Source))
            .transpose()?;
        if target.rooted {
            let expected = root_arguments(&self.caller_source, &target.at)?;
            if self.root_atoms.as_ref().is_none_or(|actual| {
                actual.len() != expected.len()
                    || actual
                        .iter()
                        .zip(expected.iter())
                        .any(|(a, b)| match (a, b) {
                            (ScalarInput::Literal(a), ScalarInput::Literal(b)) => !a.same(b),
                            (ScalarInput::Binding(a), ScalarInput::Binding(b)) => a != b,
                            _ => true,
                        })
            }) {
                return Err(Refused::Source);
            }
        } else if self.root_atoms.is_some() {
            return Err(Refused::Source);
        }
        let origin = if target.dynamic {
            Some(target::root_slot(&self.caller_source, &target.at)?)
        } else {
            None
        };
        if origin != self.callable_origin
            || callable != self.callable
            || expected.as_ref() != self.argument_slots.as_ref()
            || recovery.as_ref() != self.recovery.as_ref()
        {
            return Err(Refused::Exit);
        }
        for path in &self.paths {
            if !path
                .charges
                .covers(self.caller, self.statement, self.first_unpaid, self.pc)
            {
                return Err(Refused::Exit);
            }
            path.region.verify()?;
            path.charges.verify(exe)?;
        }
        paths::verify_coverage(self)?;
        Ok(())
    }

    pub(crate) fn compile_continuation(
        &self,
        exe: &Executable,
        through: &Point,
        selected: &[Selection],
    ) -> Result<Arc<CompiledInterval>, Refused> {
        self.verify(exe)?;
        if through.site != exe.stmt(self.statement).site || through.phase != Phase::Commit {
            return Err(Refused::Source);
        }
        self.caller_source
            .compile_call_continuation(&self.at, through, selected)
    }
    pub(in crate::compile::room) fn belongs_to(&self, source: &Arc<Source>, code: CodeId) -> bool {
        self.caller == code && Arc::ptr_eq(source, &self.caller_source)
    }
    pub(in crate::compile::room) fn owns_debt(&self, debt: &ComposedDebt<'_>) -> bool {
        self.paths.iter().any(|path| debt.belongs_to(&path.charges))
    }
    pub(in crate::compile::room) fn argument_roles(&self) -> impl Iterator<Item = Slot> + '_ {
        self.argument_slots.iter().copied()
    }
    pub(in crate::compile::room) fn caller_statement(&self) -> StmtId {
        self.statement
    }
    pub(in crate::compile::room) fn root_arguments(&self) -> Option<&[ScalarInput]> {
        self.root_atoms.as_deref()
    }
    pub(crate) fn callee(&self) -> CodeId {
        self.callee
    }
    pub(crate) fn at(&self) -> &Point {
        &self.at
    }
    pub(crate) fn source(&self) -> &Arc<Source> {
        &self.source
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
        if self.callable.is_some()
            || self.executable != exe.local()
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
            != Some(self.paths[0].charges.caller().first())
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
        bound.inputs = caller
            .region
            .entries
            .iter()
            .map(|entry| entry.slot)
            .collect();
        bound.live = caller
            .region
            .survivors
            .iter()
            .filter(|(slot, _)| !self.argument_slots.contains(slot))
            .map(|(slot, live)| (*slot, caller.values[live.fact.index()], live.entry))
            .collect();
        bound.live.push((
            slot(&self.caller_source, &Owner::Operand(self.at.site.clone()))
                .map_err(|_| doc::heap::HeapFault::Malformed("call result role".into()))?,
            bound.result,
            None,
        ));
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
        if self.callable.is_some()
            || self.executable != exe.local()
            || resolved_code != self.callee
            || !self.argument_slots.is_empty()
            || !self.recovery.is_empty()
            || !activation.current(&self.caller_source, ledger, invocation)
            || position.occurrence.invocation != invocation.id()
            || position.unpaid.as_ref() != Some(&self.at)
            || self.paths[0].charges.caller().first() != &self.at
        {
            return Ok(None);
        }
        self.bind_arguments(exe, ledger, arena, heap, invocation, vec![], vec![])
    }

    #[cfg(test)]
    pub(crate) fn understate_frame_for_law(&mut self) {
        self.paths[0].region.understate_peak();
    }
    #[cfg(test)]
    pub(crate) fn omit_caller_role_for_law(&mut self) {
        self.recovery = Box::new([]);
    }
    #[cfg(test)]
    pub(crate) fn reorder_closes_for_law(&mut self) {
        self.paths[0].charges.reorder_for_law();
    }
}

impl<'a> BoundCall<'a> {
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
        self.depth
    }
    pub(crate) fn continuation_values(&self) -> impl Iterator<Item = (Slot, Value)> + '_ {
        self.live.iter().map(|(slot, value, _)| (*slot, *value))
    }
    pub(super) fn caller_source(&self) -> &Arc<Source> {
        &self.call.caller_source
    }
    pub(super) fn callee_source(&self) -> &Arc<Source> {
        &self.call.source
    }
    pub(super) fn point(&self) -> &Point {
        &self.call.at
    }

    pub(crate) fn occurrences(&self) -> &[usize] {
        self.call.occurrence.as_slice()
    }
    pub(crate) fn current(&self, ledger: &Ledger, caller: &logical::Invocation) -> bool {
        ledger.is_current(&self.stamp) && caller == &self.caller_invocation
    }
    pub(crate) fn debt(&self) -> Option<ComposedDebt<'a>> {
        bind_debt(
            &self.call.paths[self.path],
            &self.caller_formulas,
            &self.body_formulas,
            &[],
        )
    }
    pub(crate) fn recovery_after(
        &self,
        at: &Point,
        caller_occurrence: super::super::publication::Occurrence,
    ) -> Option<[CallRecovery; 2]> {
        call_recovery(
            self.call,
            &self.caller_invocation,
            &self.reservation.invocation(0)?,
            &self.caller_values,
            CalleeValues {
                values: &self.values,
                path: self.path,
                captures: &[],
                recovery: None,
            },
            at,
            caller_occurrence,
        )
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

fn bind_debt<'a>(
    path: &'a paths::BodyPath,
    caller_formulas: &[(Vec<Value>, Value)],
    body_formulas: &[Vec<(Vec<Value>, Value)>],
    nested: &[nested::Evaluated],
) -> Option<ComposedDebt<'a>> {
    let caller: Vec<_> = caller_formulas
        .iter()
        .map(|(arguments, result)| FormulaValues {
            arguments,
            result: *result,
        })
        .collect();
    let body: Vec<Vec<_>> = body_formulas
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
    let mut children = Vec::new();
    if let nested::BodyRegion::Composed(plan) = &path.region {
        for child in nested {
            let call = plan.calls.get(child.index)?;
            let evaluated = &child.body;
            let debt = bind_debt(
                call.paths.get(evaluated.path)?,
                &evaluated.caller_formulas,
                &evaluated.body_formulas,
                &evaluated.nested,
            )?;
            children.push((call.at.clone(), debt));
        }
    } else if !nested.is_empty() {
        return None;
    }
    path.charges.bind_nested(&caller, &body, children)
}

/// A verified return edge, distinct from the caller's original action exit.
/// All admitted callee paths end at this source-local Leave; binding selects
/// and authenticates the actual guarded path before this edge can publish.
pub(crate) struct CallCompletion {
    call: Arc<VerifiedCall>,
    entry_exit: ExitKind,
    leave: Point,
    resume: Point,
}
impl CallCompletion {
    pub(crate) fn entry(&self) -> (&Point, ExitKind) {
        (&self.call.at, self.entry_exit)
    }
    pub(crate) fn callee(&self) -> &Arc<Source> {
        &self.call.source
    }
    pub(crate) fn leave(&self) -> &Point {
        &self.leave
    }
    pub(crate) fn resume(&self) -> &Point {
        &self.resume
    }
    pub(in crate::compile::room) fn call(&self) -> &Arc<VerifiedCall> {
        &self.call
    }
    pub(in crate::compile::room) fn after_return(&self) -> ExitKind {
        // This is the caller's control result after callee Leave and the
        // caller's resume event. The stored Enter still exits as Action.
        ExitKind::Normal
    }
}
impl VerifiedCall {
    pub(in crate::compile::room) fn completion(
        self: &Arc<Self>,
        source: &Source,
        at: &Point,
        exit: ExitKind,
    ) -> Result<CallCompletion, Refused> {
        if !std::ptr::eq(source, self.caller_source.as_ref())
            || self.at != *at
            || !source
                .room
                .exits
                .iter()
                .any(|candidate| candidate.point == *at && candidate.kind == exit)
            || exit
                != if self.at.phase == Phase::Enter {
                    ExitKind::Action
                } else {
                    ExitKind::Normal
                }
        {
            return Err(Refused::Source);
        }
        let leave = Point {
            site: self.source.site.clone(),
            phase: Phase::Leave,
        };
        if self.paths.is_empty()
            || self.paths.iter().any(|path| {
                !Arc::ptr_eq(path.region.source(), &self.source)
                    || !matches!(path.region.exit(), ExitKind::Return | ExitKind::Normal)
                    || path.region.last_point() != Some(&leave)
            })
        {
            return Err(Refused::Source);
        }
        let resume = Point {
            site: at.site.clone(),
            phase: if self.rooted {
                Phase::Commit
            } else {
                Phase::Restore
            },
        };
        source.compile_event(&resume, ExitKind::Normal)?;
        Ok(CallCompletion {
            call: Arc::clone(self),
            entry_exit: exit,
            leave,
            resume,
        })
    }
}

type RecoveryState = (Point, Vec<(Slot, Value)>);

struct CalleeValues<'a> {
    values: &'a [Value],
    path: usize,
    captures: &'a [(Slot, doc::arena::ArenaHandle)],
    recovery: Option<&'a [RecoveryState]>,
}
fn call_recovery(
    call: &VerifiedCall,
    caller_invocation: &logical::Invocation,
    invocation: &logical::Invocation,
    caller_values: &[(Slot, Value)],
    body: CalleeValues<'_>,
    at: &Point,
    caller_occurrence: super::super::publication::Occurrence,
) -> Option<[CallRecovery; 2]> {
    use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};
    if caller_occurrence.invocation != caller_invocation.id()
        || !matches!(caller_invocation.frame(), logical::Owner::Frame { task, .. } if task == caller_occurrence.task)
    {
        return None;
    }
    let (after, unpaid, roles) = if let Some(recovery) = body.recovery {
        let index = recovery.iter().position(|(point, _)| point == at)?;
        (
            recovery[index].0.clone(),
            recovery.get(index + 1).map(|(point, _)| point.clone()),
            recovery[index].1.clone(),
        )
    } else {
        let region = call.paths.get(body.path)?.region.scalar()?;
        let index = region.recovery.iter().position(|map| &map.after == at)?;
        let map = &region.recovery[index];
        (
            map.after.clone(),
            region.recovery.get(index + 1).map(|map| map.after.clone()),
            map.roles
                .iter()
                .map(|(slot, fact)| (*slot, body.values[fact.index()]))
                .collect(),
        )
    };
    let digest =
        |at: &Point, roles: &[(Slot, Value)], captures: &[(Slot, doc::arena::ArenaHandle)]| {
            let mut encoding = TokenWriter::new();
            encoding.seq(4);
            encoding.text("lash.source.composed-call");
            at.site.encode(&mut encoding);
            encoding.seq(roles.len() as u64);
            for (slot, _) in roles {
                slot.encode(&mut encoding);
            }
            encoding.seq(captures.len() as u64);
            for (slot, _) in captures {
                slot.encode(&mut encoding);
            }
            logical_digest(&encoding.into_bytes())
        };
    let caller = CallRecovery {
        source: Arc::clone(&call.caller_source),
        position: super::super::publication::Position {
            at: call.at.clone(),
            unpaid: Some(Point {
                site: call.at.site.clone(),
                phase: if call.rooted {
                    Phase::Commit
                } else {
                    Phase::Restore
                },
            }),
            recovery: digest(&call.at, caller_values, &[]),
            occurrence: caller_occurrence.clone(),
        },
        roles: caller_values.to_vec(),
        captures: Vec::new(),
    };
    let captures = if after.site == call.source.site && after.phase == Phase::Leave {
        &[][..]
    } else {
        body.captures
    };
    let callee = CallRecovery {
        source: Arc::clone(&call.source),
        position: super::super::publication::Position {
            at: after.clone(),
            unpaid,
            recovery: digest(&after, &roles, captures),
            occurrence: super::super::publication::Occurrence {
                task: caller_occurrence.task,
                invocation: invocation.id(),
                loops: vec![],
                action: None,
            },
        },
        roles,
        captures: captures.to_vec(),
    };
    Some([caller, callee])
}

fn root_arguments(source: &Source, at: &Point) -> Result<Box<[ScalarInput]>, Refused> {
    let mut node = doc::Node::Block(&source.body);
    for index in &at.site.path[source.site.path.len()..] {
        node = *node.children().get(*index as usize).ok_or(Refused::Path)?;
    }
    let doc::Node::Action(doc::Action::Call { args, .. }) = node else {
        return Err(Refused::Source);
    };
    let slots = source.owner_slots()?;
    args.iter()
        .map(|atom| match atom {
            doc::Atom::Variable(name) => {
                let owner =
                    root_owners::visible(&slots.scopes, &at.site, name).ok_or(Refused::Source)?;
                if let Owner::Binding { declared, name } = owner
                    && source.shared.get(&(declared.clone(), name.clone())) == Some(&true)
                {
                    return Err(Refused::Source);
                }
                Ok(ScalarInput::Binding(slot(source, owner)?))
            }
            doc::Atom::Literal(value) => Ok(ScalarInput::Literal(match value {
                doc::Literal::Null => Value::Null,
                doc::Literal::Absent => Value::Absent,
                doc::Literal::Bool(value) => Value::Bool(*value),
                doc::Literal::Int(value) => Value::Int(value.word().ok_or(Refused::Facts)?),
                doc::Literal::Float(value) => Value::Float(*value),
                doc::Literal::Text(value) => {
                    Value::Text(doc::arena::ShortText::new(value).ok_or(Refused::Facts)?)
                }
                _ => return Err(Refused::Source),
            })),
        })
        .collect()
}
