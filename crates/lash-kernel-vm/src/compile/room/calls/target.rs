//! Dynamic alternatives retain the actual source read and admitted callable.
use super::*;

pub(in crate::compile::room) fn resolve(
    exe: &Executable,
    caller: CodeId,
    statement: StmtId,
    pc: u32,
    selected: Option<CodeId>,
) -> Result<Resolved, Refused> {
    let (stream, stmt, _) = exe.statement(statement);
    if exe.stmt_at(caller, &stmt.site) != Some(statement)
        || pc < stmt.ops.start
        || pc >= stmt.ops.end()
    {
        return Err(Refused::Source);
    }
    let op = stream.ops.get(pc as usize).ok_or(Refused::Source)?;
    let dynamic = match op {
        Op::Nested { act, .. } => matches!(
            stream.nested.get(act.ix()).map(|entry| &entry.kind),
            Some(NestedKind::Call(NestedCallee::Read))
        ),
        Op::Act => matches!(
            &stmt.form,
            compile::Form::Action(compile::Action {
                kind: compile::ActionKind::Call {
                    callee: compile::Callee::Value(_),
                    ..
                },
                ..
            })
        ),
        _ => false,
    };
    if !dynamic {
        let target = resolved_static(exe, caller, statement, pc)?;
        return if selected.is_none_or(|code| code == target.code) {
            Ok(target)
        } else {
            Err(Refused::Source)
        };
    }
    let code = selected.ok_or(Refused::Source)?;
    if code.0 as usize >= exe.code_count() {
        return Err(Refused::Source);
    }
    let site = stream
        .source_sites
        .get(pc as usize)
        .and_then(Option::as_deref)
        .ok_or(Refused::Source)?;
    let source = &exe.code(caller).source_room;
    let node = node(source, site)?;
    let (arguments, occurrence, rooted) = match (op, node) {
        (Op::Nested { act, args }, doc::Node::Expr(doc::Expr::Act(action))) => {
            let entry = stream.nested.get(act.ix()).ok_or(Refused::Source)?;
            if entry.site != *site
                || !matches!(action.kind, doc::ActKind::Call(doc::Callee::Value(_)))
                || args.len as usize != action.args.len() + 1
            {
                return Err(Refused::Source);
            }
            (
                action.args.len(),
                entry.guest.then_some(entry.occurrence),
                false,
            )
        }
        (
            Op::Act,
            doc::Node::Action(doc::Action::Call {
                callee: doc::Callee::Value(_),
                args,
            }),
        ) => {
            let compile::Form::Action(action) = &stmt.form else {
                return Err(Refused::Source);
            };
            let compile::ActionKind::Call { args: compiled, .. } = &action.kind else {
                return Err(Refused::Source);
            };
            if pc != stmt.ops.start || action.site != *site || compiled.len() != args.len() {
                return Err(Refused::Source);
            }
            (args.len(), action.guest.then_some(action.occurrence), true)
        }
        _ => return Err(Refused::Source),
    };
    let origin = root_slot(
        source,
        &Point {
            site: site.clone(),
            phase: Phase::Enter,
        },
    )?;
    let placement = super::super::super::placement::BindingPlacement::compile(exe, caller)?;
    let physical = placement.physical(origin).ok_or(Refused::Source)?;
    let valid_read = match op {
        Op::Nested { args, .. } => (stmt.ops.start..pc)
            .rev()
            .find_map(|read| {
                let op = &stream.ops[read as usize];
                (op.writes() == Some(args.start)).then(|| {
                    matches!(op,
                        Op::Local { local, .. } if local.at == physical && exe.code(caller).slots_by_at.get(physical as usize) == Some(&local.slot)
                    ) && stream.source_sites[read as usize].as_deref() == Some(site)
                })
            })
            .unwrap_or(false),
        Op::Act => matches!(&stmt.form, compile::Form::Action(compile::Action {
            kind: compile::ActionKind::Call { callee: compile::Callee::Value(compile::Var::Local(local)), .. }, ..
        }) if local.at == physical && exe.code(caller).slots_by_at.get(physical as usize) == Some(&local.slot)),
        _ => false,
    };
    if !valid_read {
        return Err(Refused::Source);
    }
    let library = match exe.code(code).source_room.owner {
        doc::constant_pool::PoolOwner::Function(function) => exe.library_lib(&function).filter(|lib| {
            matches!(exe.lib(*lib).run, LibRun::Body(actual) | LibRun::Native { body: Some(actual), .. } if actual == code)
        }),
        doc::constant_pool::PoolOwner::Document(_) => None,
    };
    if arguments > exe.code(code).params.len() {
        return Err(Refused::Source);
    }
    if let Some(lib) = library {
        let function = exe.lib(lib);
        if !function.body_arity(arguments)
            || function.limit.is_some()
            || function.shares_params
            || function.specialization.resource
                != CallSource::Body(function.specialization.function)
        {
            return Err(Refused::Source);
        }
    }
    Ok(Resolved {
        code,
        library,
        at: Point {
            site: site.clone(),
            phase: Phase::Enter,
        },
        arguments,
        occurrence,
        rooted,
        dynamic: true,
    })
}

fn node<'a>(source: &'a Source, at: &Site) -> Result<doc::Node<'a>, Refused> {
    if at.unit != source.site.unit || !at.path.starts_with(&source.site.path) {
        return Err(Refused::Source);
    }
    let mut node = doc::Node::Block(&source.body);
    for index in &at.path[source.site.path.len()..] {
        node = *node.children().get(*index as usize).ok_or(Refused::Path)?;
    }
    Ok(node)
}

pub(super) fn root_slot(source: &Source, at: &Point) -> Result<Slot, Refused> {
    let name = match node(source, &at.site)? {
        doc::Node::Action(doc::Action::Call {
            callee: doc::Callee::Value(name),
            ..
        }) => name,
        doc::Node::Expr(doc::Expr::Act(action)) => {
            let doc::ActKind::Call(doc::Callee::Value(name)) = &action.kind else {
                return Err(Refused::Source);
            };
            name
        }
        _ => return Err(Refused::Source),
    };
    let slots = source.owner_slots()?;
    let owner = root_owners::visible(&slots.scopes, &at.site, name).ok_or(Refused::Source)?;
    slot(source, owner)
}

impl VerifiedCall {
    pub(super) fn authenticate_callable(
        &self,
        exe: &Executable,
        arena: &Arena,
        value: Value,
    ) -> Option<lash_kernel_value::CallableId> {
        match value {
            Value::Closure(handle) => {
                let (identity, cells, _) = arena.closure(handle).ok()?;
                if !cells.is_empty()
                    || identity.capture_names != exe.code(self.callee).capture_names
                {
                    return None;
                }
                let code = crate::machine::project::closure_code(
                    exe,
                    &identity.callable,
                    &identity.site.site,
                    &identity.capture_names,
                )
                .ok()?;
                (code == self.callee).then_some(identity.callable)
            }
            Value::Function(handle) => {
                let callable = arena.callable(handle).ok()?;
                if !exe.admits_callable(&callable) {
                    return None;
                }
                let code = exe.declared.get(&callable.function).copied().or_else(|| {
                    let lib = exe.library_lib(&callable.function)?;
                    match exe.lib(lib).run {
                        LibRun::Body(code)
                        | LibRun::Native {
                            body: Some(code), ..
                        } => Some(code),
                        _ => None,
                    }
                })?;
                (code == self.callee).then_some(callable)
            }
            _ => None,
        }
    }
}
