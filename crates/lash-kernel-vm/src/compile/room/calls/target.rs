//! Dynamic alternatives retain the actual source read and admitted callable.
use super::*;

pub(in crate::compile::room) fn resolve(
    exe: &Executable,
    caller: CodeId,
    statement: StmtId,
    pc: u32,
    selected: Option<CodeId>,
) -> Result<Resolved, Refused> {
    match call_site(exe, caller, statement, pc)? {
        CallSite::Static(target) => {
            if selected.is_none_or(|code| code == target.code) {
                Ok(target)
            } else {
                Err(Refused::Source)
            }
        }
        CallSite::Dynamic(site) => site.admit(exe, selected.ok_or(Refused::Source)?),
    }
}

/// A call site's facts. A static site names its one target; a dynamic one
/// names only what the caller holds, and admits each target by its code.
pub(in crate::compile::room) enum CallSite {
    Static(Resolved),
    Dynamic(DynamicSite),
}

pub(in crate::compile::room) struct DynamicSite {
    at: Point,
    arguments: usize,
    occurrence: Option<usize>,
    rooted: bool,
}

pub(in crate::compile::room) fn call_site(
    exe: &Executable,
    caller: CodeId,
    statement: StmtId,
    pc: u32,
) -> Result<CallSite, Refused> {
    let (stream, stmt, _) = exe.statement(statement);
    let span = caller_span(stmt, pc)?;
    if exe.stmt_at(caller, &stmt.site) != Some(statement) {
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
        return resolved_static(exe, caller, statement, pc).map(CallSite::Static);
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
    let origin = root_role(
        source,
        &Point {
            site: site.clone(),
            phase: Phase::Enter,
        },
    )?;
    // The callee is read from the frame slot or the session cell its name
    // resolves to, and from nowhere else.
    let physical = match origin {
        Role::Slot(slot) => Some(
            super::super::super::placement::BindingPlacement::compile(exe, caller)?
                .physical(slot)
                .ok_or(Refused::Source)?,
        ),
        Role::Session(_) => None,
    };
    let local_read = |at, slot| {
        physical == Some(at) && exe.code(caller).slots_by_at.get(at as usize) == Some(&slot)
    };
    let session_read = |name: &lash_kernel_value::Name| matches!(origin, Role::Session(digest) if logical::Owner::session(name) == logical::Owner::Session(digest));
    let valid_read = match op {
        Op::Nested { args, .. } => (span.start..pc)
            .rev()
            .find_map(|read| {
                let op = &stream.ops[read as usize];
                (op.writes() == Some(args.start)).then(|| {
                    let reads = match op {
                        Op::Local { local, .. } => local_read(local.at, local.slot),
                        Op::Session { name, .. } => stream
                            .names
                            .get(name.0 as usize)
                            .is_some_and(|binding| session_read(&binding.name)),
                        _ => false,
                    };
                    reads && stream.source_sites[read as usize].as_deref() == Some(site)
                })
            })
            .unwrap_or(false),
        Op::Act => match &stmt.form {
            compile::Form::Action(compile::Action {
                kind:
                    compile::ActionKind::Call {
                        callee: compile::Callee::Value(var),
                        ..
                    },
                ..
            }) => match var {
                compile::Var::Local(local) => local_read(local.at, local.slot),
                compile::Var::Session(binding) => session_read(&binding.name),
                compile::Var::Unbound(_) => false,
            },
            _ => false,
        },
        _ => false,
    };
    if !valid_read {
        return Err(Refused::Source);
    }
    Ok(CallSite::Dynamic(DynamicSite {
        at: Point {
            site: site.clone(),
            phase: Phase::Enter,
        },
        arguments,
        occurrence,
        rooted,
    }))
}

impl DynamicSite {
    pub(in crate::compile::room) fn occurrence(&self) -> Option<usize> {
        self.occurrence
    }
    pub(in crate::compile::room) fn shape(&self) -> Shape<'_> {
        Shape {
            at: &self.at,
            arguments: self.arguments,
            rooted: self.rooted,
            dynamic: true,
        }
    }

    /// Admits `code` as this site's target: a body-backed library takes the
    /// site's arguments within its arity, and no code takes more arguments
    /// than it has parameters.
    pub(in crate::compile::room) fn admit(
        &self,
        exe: &Executable,
        code: CodeId,
    ) -> Result<Resolved, Refused> {
        if code.0 as usize >= exe.code_count() {
            return Err(Refused::Source);
        }
        let arguments = self.arguments;
        // A closure's body takes its free names from the closure's cells,
        // so it is entered as a body, never through its library's
        // signature.
        let library = match exe.code(code).source_room.owner {
            _ if !exe.code(code).capture_names.is_empty() => None,
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
                || function.specialization.resource
                    != CallSource::Body(function.specialization.function)
            {
                return Err(Refused::Source);
            }
        }
        Ok(Resolved {
            code,
            library,
            at: self.at.clone(),
            arguments,
            occurrence: self.occurrence,
            rooted: self.rooted,
            dynamic: true,
            checked: false,
        })
    }
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

/// The role a dynamic call reads its callee from: the frame slot of its
/// name's lexical binding, or the session cell a free name resolves to.
pub(super) fn root_role(source: &Source, at: &Point) -> Result<Role, Refused> {
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
    crate::compile::region_isa::owner_role(
        source,
        &Owner::Visible {
            at: at.site.clone(),
            name: name.clone(),
        },
    )
    .ok_or(Refused::Source)
}
