//! Source call placement; region execution owns values, frames and debt.
#[path = "calls/region_isa.rs"]
pub(crate) mod region_isa;
#[path = "calls/target.rs"]
pub(in crate::compile::room) mod target;
use super::super::*;
use crate::compile::region_isa::Role;
use crate::compile::{self, CodeId, Executable, LibId, LibRun, NestedCallee, NestedKind, StmtId};
use lash_kernel_doc::TaskId;
use lash_kernel_doc::Value;
use lash_kernel_exec::{Op, OpRange};
use lash_kernel_state::kernel_internals::logical::{self, Slot};
#[cfg(test)]
use lash_kernel_state::{arena::Arena, kernel_internals::PagedHeap};

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

/// A protocol `for`'s hook at `phase` of statement `statement`, resolved
/// to the library body it calls (`K-ITER-005`), and the pc of its `for`.
/// The loop holds what `next`, `check` and `item` take; `open` moves the
/// iterated value into its callee. Only a library body whose parameters
/// take what the loop gives without a check is a region call.
pub(in crate::compile::room) fn resolved_hook(
    exe: &Executable,
    caller: CodeId,
    statement: StmtId,
    phase: Phase,
) -> Result<(Resolved, u32), Refused> {
    use lash_kernel_value::Type;
    let (stream, stmt, _) = exe.statement(statement);
    if exe.stmt_at(caller, &stmt.site) != Some(statement) {
        return Err(Refused::Source);
    }
    // A `return via` policy takes the departing value; a protocol `for`'s
    // hooks take what the loop gives them.
    let (hook, kinds, anchor): (_, &[Type], fn(&Op) -> bool) = match (&stmt.form, phase) {
        (compile::Form::ReturnVia { policy, .. }, Phase::Policy) => {
            (Some(*policy), &[Type::Any], |op| {
                matches!(op, Op::ReturnVia { .. })
            })
        }
        (compile::Form::For { via: Some(via), .. }, _) => {
            let (hook, kinds): (_, &[Type]) = match phase {
                Phase::Open => (Some(via.open), &[Type::Any]),
                Phase::Next => (via.next, &[Type::Any]),
                Phase::Check => (via.check, &[Type::Any, Type::Int, Type::Int, Type::Bool]),
                Phase::Item => (via.item, &[Type::Any, Type::Any]),
                _ => return Err(Refused::Source),
            };
            (hook, kinds, |op| matches!(op, Op::ForIn { .. }))
        }
        _ => return Err(Refused::Source),
    };
    let pc = (stmt.ops.start..stmt.ops.end())
        .find(|pc| stream.ops.get(*pc as usize).is_some_and(anchor))
        .ok_or(Refused::Source)?;
    let Some(compile::LinkedHook::Library(id)) = hook else {
        return Err(Refused::Source);
    };
    let lib = exe.lib(id);
    let LibRun::Body(code) = lib.run else {
        return Err(Refused::Source);
    };
    let signature = &lib.header.signature;
    if !lib.body_arity(kinds.len())
        || lib.limit.is_some()
        || lib.specialization.resource != CallSource::Body(lib.specialization.function)
        || exe.code(code).source_room.owner
            != doc::constant_pool::PoolOwner::Function(lib.specialization.function)
        || signature
            .params
            .iter()
            .zip(kinds)
            .any(|(param, kind)| param.ty != Type::Any && param.ty != *kind)
    {
        return Err(Refused::Source);
    }
    Ok((
        Resolved {
            code,
            library: Some(id),
            at: Point {
                site: stmt.site.clone(),
                phase,
            },
            arguments: kinds.len(),
            occurrence: None,
            rooted: !matches!(phase, Phase::Open | Phase::Policy),
            dynamic: false,
            checked: false,
        },
        pc,
    ))
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

fn slot(source: &Source, owner: &Owner) -> Result<Slot, Refused> {
    match source.owner_slots()?.root(owner, TaskId::MAIN, 0) {
        Some(logical::Owner::Value { slot, .. }) => Ok(slot),
        _ => Err(Refused::Source),
    }
}
